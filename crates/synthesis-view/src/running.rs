//! The Running panel: the sessions a process holds now, plus the peers other sessions advertise
//! (`/peers`, messaging.md B9).

use serde::Serialize;
use serde_json::Value;

use crate::workspaces::{SessionRow, WorkspaceList};

/// One row of the harness's peer listing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PeerRow {
    pub workspace_label: String,
    pub workspace_root: String,
    pub session_short_id: String,
    pub session_name: String,
    pub agent: String,
    pub kind: String,
    pub status: String,
    pub task: String,
}

fn s(v: &Value, path: &[&str]) -> String {
    path.iter()
        .try_fold(v, |v, k| v.get(k))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

impl PeerRow {
    pub fn from_json(v: &Value) -> Self {
        Self {
            workspace_label: s(v, &["workspace", "label"]),
            workspace_root: s(v, &["workspace", "root"]),
            session_short_id: s(v, &["session", "short_id"]),
            session_name: s(v, &["session", "name"]),
            agent: s(v, &["agent", "instance"]),
            kind: s(v, &["agent", "kind"]),
            status: s(v, &["agent", "status"]),
            task: s(v, &["task_summary"]),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RunningList {
    /// Live sessions of every workspace, newest first, with the folder they belong to.
    pub live: Vec<(String, SessionRow)>,
    pub peers: Vec<PeerRow>,
}

impl RunningList {
    pub fn new(w: &WorkspaceList, peers: &[Value]) -> Self {
        let mut live: Vec<(String, SessionRow)> = w
            .workspaces
            .iter()
            .flat_map(|w| {
                w.sessions
                    .iter()
                    .filter(|s| s.is_live)
                    .map(|s| (w.root.clone(), s.clone()))
            })
            .collect();
        live.sort_by_key(|(_, s)| std::cmp::Reverse(s.mtime));
        Self {
            live,
            peers: peers.iter().map(PeerRow::from_json).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_peer_row_reads_the_listing_and_misses_become_empty() {
        let r = PeerRow::from_json(&json!({"workspace": {"label": "api", "root": "/w/api"},
            "session": {"short_id": "3c9e01aa"},
            "agent": {"instance": "main", "kind": "main", "status": "idle"}, "task_summary": "fix"}));
        assert_eq!(
            (
                r.workspace_label.as_str(),
                r.agent.as_str(),
                r.task.as_str()
            ),
            ("api", "main", "fix")
        );
        assert_eq!(r.session_name, "");
        assert_eq!(PeerRow::from_json(&json!({})), PeerRow::default());
    }

    #[test]
    fn only_live_sessions_are_running() {
        assert!(
            RunningList::new(&WorkspaceList::default(), &[])
                .live
                .is_empty()
        );
    }
}
