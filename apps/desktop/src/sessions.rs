//! What the two left sidebars list: workspaces (projects) and the sessions of the selected one,
//! from the session store. Grouping and status mapping are pure and tested.

use synthesis_tui::SavedSession;

use crate::convo::Convo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    NeedsYou,
    Done,
}

/// The live conversation decides: a pending approval needs the person, a running task is running,
/// everything else (and every stored session) is done.
pub fn status_of(live: Option<&Convo>) -> Status {
    match live {
        Some(c) if c.needs_you() > 0 => Status::NeedsYou,
        Some(c) if c.running => Status::Running,
        _ => Status::Done,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub cwd: String,
    pub name: String,
    pub sessions: usize,
}

fn base_name(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// One row per distinct cwd, newest activity first; `current` is always listed, even when empty.
pub fn workspaces(saved: &[SavedSession], current: &str) -> Vec<Workspace> {
    let mut rows: Vec<(i64, Workspace)> = vec![];
    for s in saved {
        match rows.iter_mut().find(|(_, w)| w.cwd == s.cwd) {
            Some((t, w)) => {
                *t = (*t).max(s.mtime);
                w.sessions += 1;
            }
            None => rows.push((
                s.mtime,
                Workspace {
                    cwd: s.cwd.clone(),
                    name: base_name(&s.cwd),
                    sessions: 1,
                },
            )),
        }
    }
    if !rows.iter().any(|(_, w)| w.cwd == current) {
        rows.push((
            i64::MAX,
            Workspace {
                cwd: current.into(),
                name: base_name(current),
                sessions: 0,
            },
        ));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.into_iter().map(|(_, w)| w).collect()
}

const DAY_MS: i64 = 86_400_000;

pub fn group_label(age_ms: i64) -> &'static str {
    match age_ms {
        a if a < DAY_MS => "Today",
        a if a < 2 * DAY_MS => "Yesterday",
        a if a < 7 * DAY_MS => "This week",
        _ => "Earlier",
    }
}

pub fn age_text(age_ms: i64) -> String {
    let m = age_ms.max(0) / 60_000;
    match m {
        0 => "now".into(),
        1..=59 => format!("{m}m"),
        60..=1439 => format!("{}h", m / 60),
        _ => format!("{}d", m / 1440),
    }
}

/// Sessions of `cwd` grouped by recency (newest first inside a group); empty groups are dropped.
pub fn grouped<'a>(
    saved: &'a [SavedSession],
    cwd: &str,
    now_ms: i64,
) -> Vec<(&'static str, Vec<&'a SavedSession>)> {
    let mut mine: Vec<&SavedSession> = saved.iter().filter(|s| s.cwd == cwd).collect();
    mine.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    let mut out: Vec<(&'static str, Vec<&SavedSession>)> = vec![];
    for s in mine {
        let g = group_label(now_ms - s.mtime);
        match out.last_mut() {
            Some((l, v)) if *l == g => v.push(s),
            _ => out.push((g, vec![s])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convo::{ApprovalState, Item};

    fn s(id: &str, cwd: &str, mtime: i64) -> SavedSession {
        SavedSession {
            id: id.into(),
            cwd: cwd.into(),
            title: id.into(),
            updated_at: String::new(),
            mtime,
        }
    }

    #[test]
    fn status_maps_pending_approval_then_running_then_done() {
        assert_eq!(status_of(None), Status::Done);
        let mut c = Convo {
            running: true,
            ..Convo::default()
        };
        assert_eq!(status_of(Some(&c)), Status::Running);
        c.items.push(Item::Approval {
            id: "a".into(),
            tool_call_id: "t".into(),
            prompt: String::new(),
            state: ApprovalState::Pending,
        });
        assert_eq!(status_of(Some(&c)), Status::NeedsYou);
    }

    #[test]
    fn workspaces_group_by_cwd_and_always_list_the_current_one() {
        let saved = [s("1", "/a/x", 10), s("2", "/a/y", 30), s("3", "/a/x", 20)];
        let w = workspaces(&saved, "/a/new");
        assert_eq!(
            w.iter()
                .map(|w| (w.name.as_str(), w.sessions))
                .collect::<Vec<_>>(),
            [("new", 0), ("y", 1), ("x", 2)]
        );
    }

    #[test]
    fn sessions_group_by_age_newest_first() {
        let now = 10 * DAY_MS;
        let saved = [
            s("old", "/p", now - 9 * DAY_MS),
            s("a", "/p", now - 1000),
            s("b", "/p", now - 5000),
            s("other", "/q", now),
            s("y", "/p", now - DAY_MS - 5),
        ];
        let g = grouped(&saved, "/p", now);
        let shape: Vec<_> = g
            .iter()
            .map(|(l, v)| (*l, v.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()))
            .collect();
        assert_eq!(
            shape,
            [
                ("Today", vec!["a", "b"]),
                ("Yesterday", vec!["y"]),
                ("Earlier", vec!["old"])
            ]
        );
        assert_eq!(age_text(90 * 60_000), "1h");
    }
}
