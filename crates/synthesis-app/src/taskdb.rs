//! `tasks` and `verification_reports` rows keyed by the HARNESS task id.
//!
//! synthesis-session only offers `begin_task`, which invents its own id, while the harness names the
//! task itself and stamps that id on every event. A second connection (WAL, so it coexists with
//! the session writer) inserts the rows with the harness id instead. It refuses to touch a
//! database whose schema version it was not written for. Follow-up: `begin_task_with_id` /
//! `record_verification_report_with_id` in synthesis-session would make this module disappear.
//!
//! The full `VerificationReport` never leaves the harness (only `verification.completed` does),
//! so the stored report JSON is the event summary: id, attempt, status, one-line summary.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde_json::json;
use synthesis_events::{Envelope, Event, Redactor};

pub struct TaskDb {
    conn: Connection,
    session_id: String,
    redactor: Redactor,
}

impl TaskDb {
    pub fn open(
        synthesis_home: &Path,
        session_id: &str,
        secrets: Vec<String>,
    ) -> Result<Self, String> {
        let conn =
            Connection::open(synthesis_home.join("synthesis.db")).map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_millis(5000))
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(|e| e.to_string())?;
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if v != synthesis_session::SCHEMA_VERSION_LATEST {
            return Err(format!(
                "session database schema v{v}, this build writes task rows for v{}",
                synthesis_session::SCHEMA_VERSION_LATEST
            ));
        }
        Ok(Self {
            conn,
            session_id: session_id.into(),
            redactor: Redactor::new(secrets),
        })
    }

    pub fn on_event(&self, env: &Envelope) -> Result<(), String> {
        let Some(task_id) = env.task_id.as_deref() else {
            return Ok(());
        };
        let sql = |e: rusqlite::Error| e.to_string();
        match &env.event {
            Event::TaskStarted {
                request,
                acceptance_criteria,
                ..
            } => {
                let criteria =
                    serde_json::to_string(acceptance_criteria).map_err(|e| e.to_string())?;
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO tasks(id,session_id,request,acceptance_criteria,status,started_at) VALUES (?1,?2,?3,?4,'running',?5)",
                        params![
                            task_id,
                            self.session_id,
                            self.redactor.redact_str(request),
                            self.redactor.redact_str(&criteria),
                            env.ts
                        ],
                    )
                    .map_err(sql)?;
            }
            Event::TaskEnded { status, reason, .. } => {
                let status = serde_json::to_value(status)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| "failed".into());
                self.conn
                    .execute(
                        "UPDATE tasks SET status=?3, reason=?4, ended_at=?5 WHERE id=?1 AND session_id=?2",
                        params![
                            task_id,
                            self.session_id,
                            status,
                            self.redactor.redact_str(reason),
                            env.ts
                        ],
                    )
                    .map_err(sql)?;
            }
            Event::VerificationCompleted {
                report_id,
                status,
                attempt,
                summary,
            } => {
                let mut report = json!({
                    "id": report_id, "task_id": task_id, "session_id": self.session_id,
                    "attempt": attempt, "status": status, "summary": summary,
                    "partial": "event summary; the full report is not exposed by the harness",
                });
                self.redactor.redact_value(&mut report);
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO verification_reports(id,task_id,attempt,status,report,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                        params![report_id, task_id, attempt, status, report.to_string(), env.ts],
                    )
                    .map_err(sql)?;
            }
            _ => {}
        }
        Ok(())
    }
}
