//! Reflection job queue (memory.md §7, §11): no daemon. A job is queued at task end and drained by
//! the next run or `synt memory reflect`. Claims expire after 10 minutes; 3 attempts, then `failed`.

use rusqlite::{TransactionBehavior, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::{new_id, now_iso};
use crate::{Result, Store};

pub const MAX_ATTEMPTS: i64 = 3;
const CLAIM_TTL_MINUTES: i64 = 10;
/// Reflection calls per UTC day. The spec caps USD (0.50), but the summarizer reports no usage, so
/// this counts calls instead (an assumption until the provider call returns tokens).
pub const MAX_CALLS_PER_DAY: i64 = 20;

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub workspace_key: String,
    pub session_id: String,
    pub task_id: String,
    pub payload: String,
    pub attempts: i64,
}

impl Store {
    pub fn enqueue_job(
        &mut self,
        workspace_key: &str,
        session_id: &str,
        task_id: &str,
        payload: &str,
    ) -> Result<String> {
        let id = new_id("rj");
        self.conn.execute(
            "INSERT INTO reflection_jobs(id, kind, workspace_key, session_id, task_id, status, created_at, payload)
             VALUES (?1, 'reflect', ?2, ?3, ?4, 'queued', ?5, ?6)",
            params![id, workspace_key, session_id, task_id, now_iso(), self.redact(payload)],
        )?;
        Ok(id)
    }

    /// Oldest queued job (or one whose claim expired) with attempts left; counts the attempt.
    pub fn claim_job(&mut self, worker: &str, now: OffsetDateTime) -> Result<Option<Job>> {
        let stale = (now - time::Duration::minutes(CLAIM_TTL_MINUTES))
            .format(&Rfc3339)
            .map_err(|e| crate::Error(e.to_string()))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = tx
            .query_row(
                "SELECT id, coalesce(workspace_key,''), coalesce(session_id,''), coalesce(task_id,''), payload, attempts
                 FROM reflection_jobs
                 WHERE (status='queued' OR (status='claimed' AND claimed_at < ?1)) AND attempts < ?2
                 ORDER BY created_at LIMIT 1",
                params![stale, MAX_ATTEMPTS],
                |r| {
                    Ok(Job {
                        id: r.get(0)?,
                        workspace_key: r.get(1)?,
                        session_id: r.get(2)?,
                        task_id: r.get(3)?,
                        payload: r.get(4)?,
                        attempts: r.get::<_, i64>(5)? + 1,
                    })
                },
            )
            .ok();
        if let Some(j) = &job {
            tx.execute(
                "UPDATE reflection_jobs SET status='claimed', claimed_by=?2, claimed_at=?3, attempts=attempts+1 WHERE id=?1",
                params![j.id, worker, now_iso()],
            )?;
        }
        tx.commit()?;
        Ok(job)
    }

    /// `None` = done. An error puts the job back until its attempts run out.
    pub fn finish_job(&mut self, id: &str, error: Option<&str>) -> Result<()> {
        let err = error.map(|e| self.redact(e));
        self.conn.execute(
            "UPDATE reflection_jobs SET
               status = CASE WHEN ?2 IS NULL THEN 'done' WHEN attempts >= ?3 THEN 'failed' ELSE 'queued' END,
               error = ?2, finished_at = ?4, claimed_by = NULL, claimed_at = NULL
             WHERE id = ?1",
            params![id, err, MAX_ATTEMPTS, now_iso()],
        )?;
        Ok(())
    }

    /// `synt memory reflect --retry`: failed jobs get one more attempt (past attempts stay counted).
    pub fn retry_failed(&mut self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE reflection_jobs SET status='queued', attempts=?1, error=NULL WHERE status='failed'",
            [MAX_ATTEMPTS - 1],
        )?)
    }

    /// Model calls made today (UTC): every attempt of a job counts, failed or still claimed, so a
    /// job that keeps failing cannot spend past the cap. Jobs that reached a model today (UTC), for the daily cap.
    pub fn reflections_today(&self, now: OffsetDateTime) -> Result<i64> {
        let day = now.date().to_string();
        Ok(self.conn.query_row(
            "SELECT coalesce(sum(attempts), 0) FROM reflection_jobs
             WHERE attempts > 0 AND substr(coalesce(finished_at, claimed_at, created_at), 1, 10) = ?1",
            [day],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path(), vec![]).unwrap();
        let now = OffsetDateTime::now_utc();
        assert!(s.claim_job("w", now).unwrap().is_none());
        let id = s.enqueue_job("ws", "s1", "t1", "{}").unwrap();
        let j = s.claim_job("w1", now).unwrap().unwrap();
        assert_eq!((j.id.as_str(), j.attempts), (id.as_str(), 1));
        // claimed: nobody else gets it, until the claim expires
        assert!(s.claim_job("w2", now).unwrap().is_none());
        let later = now + time::Duration::minutes(11);
        assert_eq!(s.claim_job("w2", later).unwrap().unwrap().attempts, 2);
        s.finish_job(&id, Some("timeout")).unwrap();
        assert_eq!(s.stats().unwrap().queued_jobs, 1);
        s.claim_job("w", now).unwrap().unwrap();
        s.finish_job(&id, Some("timeout")).unwrap();
        assert_eq!(s.stats().unwrap().failed_jobs, 1, "3 attempts, then failed");
        assert!(s.claim_job("w", now).unwrap().is_none());
        assert_eq!(s.retry_failed().unwrap(), 1);
        s.claim_job("w", now).unwrap().unwrap();
        s.finish_job(&id, None).unwrap();
        assert_eq!(
            s.reflections_today(OffsetDateTime::now_utc()).unwrap(),
            3,
            "every attempt counts"
        );
    }

    #[test]
    fn payload_is_redacted() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path(), vec!["topsecretvalue1".into()]).unwrap();
        s.enqueue_job("ws", "s", "t", "token topsecretvalue1")
            .unwrap();
        let j = s
            .claim_job("w", OffsetDateTime::now_utc())
            .unwrap()
            .unwrap();
        assert!(!j.payload.contains("topsecret"));
    }

    #[test]
    fn migration_from_v1_backs_up_first() {
        let d = tempfile::tempdir().unwrap();
        {
            let s = Store::open(d.path(), vec![]).unwrap();
            // simulate a build that only knew v1
            s.conn
                .execute("DELETE FROM schema_migrations WHERE version >= 2", [])
                .unwrap();
            for sql in [
                "ALTER TABLE reflection_jobs DROP COLUMN payload",
                "ALTER TABLE reflection_jobs DROP COLUMN tokens",
                "ALTER TABLE reflection_jobs DROP COLUMN cost_usd",
                "DROP TABLE memory_retrievals",
            ] {
                s.conn.execute(sql, []).unwrap();
            }
        }
        let s = Store::open(d.path(), vec![]).unwrap();
        assert_eq!(s.schema_version().unwrap(), 3);
        assert!(d.path().join("memory/memory.v1.bak.db").is_file());
    }

    #[test]
    fn newer_schema_opens_read_only() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path(), vec![]).unwrap();
        s.conn
            .execute("INSERT INTO schema_migrations VALUES (99, 'x')", [])
            .unwrap();
        drop(s);
        let mut s = Store::open(d.path(), vec![]).unwrap();
        assert!(s.read_only());
        assert!(s.enqueue_job("w", "s", "t", "{}").is_err());
    }
}
