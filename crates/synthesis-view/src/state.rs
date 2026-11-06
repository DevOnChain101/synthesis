//! One [`ViewState`] a front end holds: feed it events with [`ViewState::apply`], fill the store
//! panels with [`ViewState::load_from_store`], read every panel from it.

use std::path::Path;

use serde_json::Value;
use synthesis_core::WorkspaceContext;
use synthesis_events::{Envelope, Record};
use synthesis_session::Reader;

use crate::activity::{ActivityModel, ToolsSkillsModel};
use crate::agent::AgentPanes;
use crate::convo::Convo;
use crate::model::{Session, apply_event, parse_ts};
use crate::running::RunningList;
use crate::status::StatusModel;
use crate::topology::Topology;
use crate::usage::{UsageHistory, load_from};
use crate::workspaces::{LoadOpts, WorkspaceList};

/// What [`ViewState::load_from_store`] needs beyond the store.
#[derive(Clone, Copy, Default)]
pub struct StoreLoad<'a> {
    /// Replay this session's events into the conversation and panels.
    pub session_id: Option<&'a str>,
    pub now_ms: i64,
    /// Minutes east of UTC, for the usage days.
    pub tz_offset_min: i32,
    pub workspaces: LoadOpts<'a>,
}

#[derive(Clone, Debug, Default)]
pub struct ViewState {
    pub convo: Convo,
    /// Sub-agent transcripts, commands, verification, browser calls and changed files (Agent view).
    pub panes: AgentPanes,
    /// Activity, Tools & Skills and Topology are read off this.
    pub session: Session,
    pub status: StatusModel,
    pub workspaces: WorkspaceList,
    pub running: RunningList,
    pub usage: UsageHistory,
}

impl ViewState {
    /// One event of the live stream (or of a replay). The event's own timestamp is the clock.
    pub fn apply(&mut self, e: &Envelope) {
        let now = parse_ts(&e.ts).unwrap_or(0);
        self.convo.apply(e);
        self.panes.apply(e);
        apply_event(&mut self.session, e);
        self.status.apply(e, now);
    }

    pub fn activity(&self) -> ActivityModel {
        ActivityModel::from_session(&self.session)
    }

    pub fn tools_skills(&self) -> ToolsSkillsModel {
        ToolsSkillsModel::from_session(&self.session)
    }

    /// The context percent comes from the status line's numbers, so the two panels agree.
    pub fn topology(&self, now_ms: i64) -> Topology {
        let (used, window) = self.status.context.numbers();
        let pct = (window > 0).then(|| used as f64 * 100.0 / window as f64);
        Topology::from_session(&self.session, now_ms, pct)
    }

    /// Peers come from the harness listing, not the store.
    pub fn set_peers(&mut self, rows: &[Value]) {
        self.running = RunningList::new(&self.workspaces, rows);
    }

    /// Workspaces, usage history and (when `session_id` is set) the session's events. A session
    /// that cannot be read is an error, not a shorter conversation. The folder of `ws` is listed
    /// even before it has a session.
    pub fn load_from_store(
        &mut self,
        r: &Reader,
        ws: &WorkspaceContext,
        o: &StoreLoad,
    ) -> Result<(), String> {
        self.workspaces = WorkspaceList::load(r, Some(Path::new(&ws.root)), &o.workspaces)?;
        self.running = RunningList::new(&self.workspaces, &[]);
        self.usage = UsageHistory {
            days: load_from(r, o.now_ms, o.tz_offset_min),
        };
        if let Some(id) = o.session_id {
            let events = r
                .read_events(id, 0, usize::MAX >> 1)
                .map_err(|e| e.to_string())?;
            self.convo = Convo::default();
            self.panes = AgentPanes::default();
            self.session = Session::new(id);
            self.status = StatusModel::default();
            // events of a newer schema are skipped on purpose (consumers ignore unknown types)
            for rec in events {
                if let Record::Known(e) = rec {
                    self.apply(&e);
                }
            }
            if self.session.cwd.is_empty() {
                self.session.cwd = ws.cwd.to_string_lossy().into_owned();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::ActivityKind;
    use crate::topology::Stage;
    use serde_json::json;

    fn ev(seq: u64, ts: &str, ty: &str, payload: Value) -> Envelope {
        serde_json::from_value(json!({"v": 1, "seq": seq, "ts": ts, "session_id": "s1",
            "task_id": null, "turn_id": null, "type": ty, "payload": payload}))
        .unwrap()
    }

    fn stream() -> Vec<Envelope> {
        vec![
            ev(
                1,
                "2026-11-07T10:00:00Z",
                "user.message",
                json!({"text": "run the tests", "images": []}),
            ),
            ev(
                2,
                "2026-11-07T10:00:01Z",
                "assistant.message",
                json!({"stop_reason": "tool_use",
                "content": [{"type": "text", "text": "on it"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "cargo test"}}]}),
            ),
            ev(
                3,
                "2026-11-07T10:00:02Z",
                "hook.ran",
                json!({"event": "PreToolUse", "hook_name": "PreToolUse:Bash",
                "exit_code": 2, "outcome": "blocked", "duration_ms": 3}),
            ),
        ]
    }

    #[test]
    fn one_stream_feeds_every_panel() {
        let mut v = ViewState::default();
        for e in stream() {
            v.apply(&e);
        }
        let a = v.activity();
        assert_eq!(a.rows[0].kind, ActivityKind::Prompt);
        assert_eq!(a.rows[0].text, "run the tests");
        assert!(
            a.log
                .iter()
                .any(|r| r.kind == ActivityKind::Tool && r.text.contains("cargo test"))
        );

        let t = v.tools_skills();
        assert_eq!(t.tools, [("Bash".to_string(), 1)]);
        assert_eq!(t.running.len(), 1, "the call has no result yet");

        let now = parse_ts("2026-11-07T10:00:02Z").unwrap() + 100;
        let top = v.topology(now);
        assert_eq!(top.user.prompts, 1);
        assert_eq!(top.user.last_prompt, "run the tests");
        assert_eq!(top.exec.items.len(), 1);
        assert_eq!(top.guards.items.len(), 1);
        assert!(top.guards.alert && top.guards.blocked == 1, "{top:?}");
        assert_eq!(top.stage, Some(Stage::Guard), "a guard just ended");
    }

    #[test]
    fn stages_are_a_closed_set_with_stable_keys() {
        assert_eq!(
            Stage::ALL.map(|s| Stage::from_key(s.key())),
            Stage::ALL.map(Some)
        );
        assert_eq!(Stage::from_key("nope"), None);
    }

    #[test]
    fn loads_workspaces_usage_and_a_session_from_the_store() {
        use synthesis_core::PermissionMode;
        use synthesis_events::Budget;
        use synthesis_session::{NewMessage, NewSession, Store, StoreOptions};

        let t = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(t.path()).unwrap();
        let dir = base.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let ws = WorkspaceContext::open("w1", &dir, &dir, &[], dir.join(".art"), "pg").unwrap();
        let store = Store::open(&base.join("home"), StoreOptions::default()).unwrap();
        let mut w = store
            .create_session(NewSession {
                id: Some("s1".into()),
                workspace: &ws,
                model_config_id: "m".into(),
                permission_mode: PermissionMode::AcceptEdits,
                permission_policy_id: "default".into(),
                budget: Budget::default(),
                parent: None,
                title: None,
            })
            .unwrap();
        w.append_message(NewMessage::user(json!("hi"))).unwrap();
        w.append_event(synthesis_events::Event::UserMessage {
            text: "hi there".into(),
            display_text: None,
            images: vec![],
        })
        .unwrap();

        let mut v = ViewState::default();
        let live = |id: &str| id == "s1";
        let o = StoreLoad {
            session_id: Some("s1"),
            now_ms: 1_800_000_000_000,
            workspaces: LoadOpts {
                is_live: Some(&live),
                ..LoadOpts::default()
            },
            ..StoreLoad::default()
        };
        v.load_from_store(&store.reader().unwrap(), &ws, &o)
            .unwrap();
        assert_eq!(v.workspaces.workspaces.len(), 1);
        assert_eq!(v.running.live.len(), 1);
        assert_eq!(v.usage.days.len(), 365);
        assert_eq!(v.session.prompts, 1);
        assert!(matches!(v.convo.items[0], crate::convo::Item::User(_)));
    }
}
