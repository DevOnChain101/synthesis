//! `/send` with no arguments: the picker of the live peers the harness lists (pure model; `Live` holds the
//! dialog). A pick becomes a one-shot target chip that holds the peer's id address (`ws:<key>/sess:<id>`):
//! names are only ever shown, never resolved.

use std::collections::HashMap;

use serde_json::Value;
use synthesis_view::running::PeerRow;

use crate::live::{age, label};
use crate::tui_view::activity::{Presence, presence};

/// One peer the person can pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerPick {
    /// `ws:<key>/sess:<session id>`: what `/send` resolves (by id).
    pub address: String,
    /// Display only (sanitised and clipped): the registry file is another process's.
    pub workspace: String,
    pub session: String,
    pub short_id: String,
    /// Last activity seen, epoch ms; 0 = none seen.
    pub last: i64,
}

impl PeerPick {
    /// `ws:<key>`, the group.
    pub fn ws(&self) -> &str {
        self.address.split("/sess:").next().unwrap_or("")
    }

    pub fn row_label(&self, now: i64) -> String {
        let when = if self.last > 0 {
            age(now - self.last)
        } else {
            "no activity seen".into()
        };
        format!(
            "{} \u{b7} {} \u{b7} {} \u{b7} {when}",
            self.session, self.workspace, self.short_id
        )
    }

    pub fn presence(&self, now: i64) -> Presence {
        presence(now, self.last)
    }
}

/// State behind the `/send` picker; the dialog's options are rebuilt from it.
#[derive(Clone, Debug, Default)]
pub struct SendPick {
    pub rows: Vec<PeerPick>,
    pub filter: String,
}

/// The chip over the prompt: where the next line goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendTo {
    pub address: String,
    /// `<workspace> › <session>`
    pub label: String,
}

/// When each peer last did something, as this window saw it: the registry has no activity time, so a
/// running peer is active now and an idle one from the poll that saw its state change.
#[derive(Clone, Debug, Default)]
pub struct Seen(HashMap<String, (String, i64)>);

impl Seen {
    pub fn update(&mut self, rows: &[Value], now: i64) {
        let mut next = HashMap::new();
        for v in rows {
            let address = PeerRow::from_json(v).address;
            if address.is_empty() {
                continue;
            }
            let status = v["agent"]["status"].as_str().unwrap_or("");
            let state = format!("{status}\0{}", v["task_summary"].as_str().unwrap_or(""));
            let at = match self.0.get(&address) {
                _ if matches!(status, "running" | "waiting_approval") => now,
                Some((s, at)) if *s == state => *at,
                Some(_) => now,
                None => 0,
            };
            next.insert(address, (state, at));
        }
        self.0 = next;
    }

    fn last(&self, address: &str) -> i64 {
        self.0.get(address).map_or(0, |(_, at)| *at)
    }
}

/// The reachable main agents of the other sessions, by workspace then session.
pub fn picks(rows: &[Value], own_sid: &str, seen: &Seen) -> Vec<PeerPick> {
    let mut out: Vec<PeerPick> = rows
        .iter()
        .filter(|v| v["agent"]["status"] != "unreachable")
        .map(|v| (v, PeerRow::from_json(v)))
        .filter(|(_, r)| {
            !r.address.is_empty()
                && (own_sid.is_empty() || !r.address.ends_with(&format!("/sess:{own_sid}")))
        })
        .map(|(_, r)| {
            let short = label(&r.session_short_id);
            let session = if r.session_name.is_empty() {
                short.clone()
            } else {
                label(&r.session_name)
            };
            let workspace = match label(&r.workspace_label) {
                w if w.is_empty() => "(unnamed)".into(),
                w => w,
            };
            PeerPick {
                last: seen.last(&r.address),
                address: r.address,
                workspace,
                session,
                short_id: short,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (&a.workspace, a.ws(), &a.session, &a.short_id).cmp(&(
            &b.workspace,
            b.ws(),
            &b.session,
            &b.short_id,
        ))
    });
    out
}

/// The picks every word of `filter` matches (session, workspace or short id, any case).
pub fn visible<'a>(rows: &'a [PeerPick], filter: &str) -> Vec<&'a PeerPick> {
    let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    rows.iter()
        .filter(|r| {
            let hay = format!("{} {} {}", r.session, r.workspace, r.short_id).to_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .collect()
}

/// Whether the harness still lists `address`.
pub fn listed(rows: &[Value], address: &str) -> bool {
    rows.iter()
        .any(|v| PeerRow::from_json(v).address == address)
}

/// `sent to ws:abcd1234../sess:12345678..`: the id address the line went to, shortened.
pub fn echo(address: &str) -> String {
    let (ws, sess) = address.split_once("/sess:").unwrap_or((address, ""));
    let cut = |s: &str, n| s.chars().take(n).collect::<String>();
    format!("sent to {}../sess:{}..", cut(ws, 11), cut(sess, 8))
}

/// The text of the picker's only row when nothing can be picked.
pub fn empty_note(peers_off: bool, any: bool) -> &'static str {
    if peers_off {
        "peers are off: start with --peers (or set synt.peers.enabled in user settings), then type /send again"
    } else if any {
        "no session matches the filter"
    } else {
        "no other live session is listed (it refreshes every 5 s; start the other one with --peers; synt.peers.allow decides which are permitted)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(key: &str, sid: &str, ws: &str, name: &str, status: &str) -> Value {
        json!({"address": format!("ws:{key}/sess:{sid}/agent:main"),
            "workspace": {"label": ws, "key": key, "root": "/w"},
            "session": {"short_id": sid.chars().take(8).collect::<String>(), "name": name},
            "agent": {"instance": "main", "kind": "main", "status": status}, "task_summary": ""})
    }

    #[test]
    fn peers_are_grouped_by_workspace_and_duplicate_names_keep_their_ids() {
        let rows = [
            row("bbbb", "22222222-x", "web", "api", "idle"),
            row("aaaa", "11111111-x", "api", "backend", "idle"),
            row("aaaa", "33333333-x", "api", "backend", "idle"),
            row("cccc", "44444444-x", "api", "", "unreachable"),
            row("dddd", "own00000-x", "web", "me", "idle"),
        ];
        let p = picks(&rows, "own00000-x", &Seen::default());
        let a: Vec<_> = p.iter().map(|p| p.address.as_str()).collect();
        // unreachable and the own session are not offered; same name twice stays two rows
        assert_eq!(
            a,
            [
                "ws:aaaa/sess:11111111-x",
                "ws:aaaa/sess:33333333-x",
                "ws:bbbb/sess:22222222-x"
            ]
        );
        assert_ne!(p[0].row_label(0), p[1].row_label(0));
        assert!(p[0].row_label(0).contains("11111111"));
    }

    #[test]
    fn the_filter_takes_every_word_in_any_column() {
        let rows = [
            row("aaaa", "11111111-x", "api", "backend", "idle"),
            row("bbbb", "22222222-x", "web", "backend", "idle"),
        ];
        let p = picks(&rows, "", &Seen::default());
        assert_eq!(visible(&p, "").len(), 2);
        assert_eq!(visible(&p, "BACK web").len(), 1);
        assert_eq!(visible(&p, "2222").len(), 1);
        assert!(visible(&p, "zzz").is_empty());
    }

    #[test]
    fn hostile_names_are_one_clean_clipped_line_and_blank_addresses_are_dropped() {
        let long = "x".repeat(300);
        let mut bad = row("aaaa", "11111111-x", "api\u{1b}[31m\nrm", &long, "idle");
        let p = picks(&[bad.clone()], "", &Seen::default());
        assert!(p[0].workspace.chars().all(|c| !c.is_control()));
        assert!(p[0].session.chars().count() <= 40);
        bad["address"] = json!("ws:aaaa/sess:1 2/agent:main");
        assert!(picks(&[bad], "", &Seen::default()).is_empty());
    }

    #[test]
    fn activity_is_now_while_running_and_from_the_last_change_when_idle() {
        let (mut seen, mut r) = (Seen::default(), row("aaaa", "1", "api", "b", "idle"));
        let at = |s: &Seen, r: &Value| picks(std::slice::from_ref(r), "", s)[0].last;
        seen.update(&[r.clone()], 1_000);
        assert_eq!(at(&seen, &r), 0, "first sight of an idle peer: none seen");
        seen.update(&[r.clone()], 2_000);
        assert_eq!(at(&seen, &r), 0);
        r["agent"]["status"] = json!("running");
        seen.update(&[r.clone()], 3_000);
        assert_eq!(at(&seen, &r), 3_000);
        r["agent"]["status"] = json!("idle");
        seen.update(&[r.clone()], 4_000);
        assert_eq!(at(&seen, &r), 4_000, "the change to idle");
        seen.update(&[r.clone()], 9_000);
        assert_eq!(at(&seen, &r), 4_000);
        let p = &picks(&[r], "", &seen)[0];
        assert_eq!(p.presence(4_000 + 29 * 60_000), Presence::Active);
        assert_eq!(p.presence(4_000 + 45 * 60_000), Presence::Recent);
        assert_eq!(p.presence(4_000 + 61 * 60_000), Presence::Inactive);
    }

    #[test]
    fn the_echo_shortens_the_ids() {
        assert_eq!(
            echo("ws:abcd1234ef56/sess:12345678-aaaa"),
            "sent to ws:abcd1234../sess:12345678.."
        );
    }
}
