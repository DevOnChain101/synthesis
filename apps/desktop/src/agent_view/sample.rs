//! A canned session for `--sample`, the layout tests and the offscreen shots: the events a real
//! engine would emit for the design's story ("Investigate why login redirects loop in auth.ts"),
//! fed through the same `ViewState` the live view uses, so the sample exercises the real reducers.

use serde_json::{Value, json};
use synthesis_events::{Budget, Envelope};
use synthesis_view::state::ViewState;

use crate::engine::{AskApproval, UiMsg};
use crate::live::Live;

struct Feed {
    seq: u64,
    out: Vec<Envelope>,
}

impl Feed {
    fn ev(&mut self, agent: Option<&str>, ty: &str, payload: Value) {
        self.seq += 1;
        let (m, s) = (4 + self.seq / 60, self.seq % 60);
        self.out.push(
            serde_json::from_value(json!({"v": 1, "seq": self.seq,
                "ts": format!("2026-11-07T23:{m:02}:{s:02}Z"), "session_id": "s1", "task_id": "t1",
                "turn_id": null, "agent_id": agent, "type": ty, "payload": payload}))
            .expect("sample event"),
        );
    }

    /// A finished tool call.
    fn tool(
        &mut self,
        agent: Option<&str>,
        id: &str,
        name: &str,
        input: Value,
        out: &str,
        ok: bool,
    ) {
        self.ev(
            agent,
            "tool.requested",
            json!({"tool_call_id": id, "name": name, "input": input, "input_truncated": false}),
        );
        self.ev(
            agent,
            "tool.completed",
            json!({"tool_call_id": id, "name": name, "status": if ok {"ok"} else {"error"},
                "is_error": !ok, "not_run": false, "duration_ms": 180, "preview": out,
                "bytes": out.len(), "truncated": false, "output": out}),
        );
    }
}

fn diff() -> Value {
    json!({"path": "src/lib/auth.ts", "kind": "edit", "added": 3, "removed": 1, "truncated": false,
        "hunks": [{"old_start": 41, "old_lines": 4, "new_start": 41, "new_lines": 6, "lines": [
            {"tag": " ", "old_no": 41, "new_no": 41, "text": "export async function requireSession() {"},
            {"tag": " ", "old_no": 42, "new_no": 42, "text": "  const session = await getSession()"},
            {"tag": "-", "old_no": 43, "text": "  if (!session) redirect('/login')"},
            {"tag": "+", "new_no": 43, "text": "  // a request that is already on /login must not redirect again"},
            {"tag": "+", "new_no": 44, "text": "  if (!session && !isLoginPath()) redirect('/login')"},
            {"tag": "+", "new_no": 45, "text": "  return session"},
            {"tag": " ", "old_no": 44, "new_no": 46, "text": "}"}]}]})
}

/// The events of the sample session, oldest first.
pub fn events() -> Vec<Envelope> {
    let mut f = Feed {
        seq: 0,
        out: vec![],
    };
    f.ev(
        None,
        "session.started",
        json!({"workspace": {"workspace_id": "w1", "root": "/Users/dev/synthesis", "cwd": "/Users/dev/synthesis",
            "additional_dirs": []}, "model": "claude-sonnet-5.5", "permission_mode": "acceptEdits", "source": "tui"}),
    );
    f.ev(
        None,
        "user.message",
        json!({"text": "Investigate why login redirects loop in auth.ts and fix it.", "images": []}),
    );
    f.ev(
        None,
        "task.started",
        json!({"request": "login redirect loop", "acceptance_criteria": [], "budget": Budget::default()}),
    );
    f.ev(
        None,
        "turn.started",
        json!({"model": "claude-sonnet-5.5", "provider": "anthropic", "iteration": 1}),
    );
    f.ev(
        None,
        "assistant.message",
        json!({"stop_reason": "tool_use", "content": [{"type": "text", "text":
            "I'll investigate the login redirect loop in auth.ts, find the root cause, and apply a fix.\nI'll start by reading the auth file and searching for the redirect logic."}]}),
    );
    f.tool(
        None,
        "c1",
        "Read",
        json!({"file_path": "src/lib/auth.ts"}),
        "import { cookies } from 'next/headers'\nimport { redirect } from 'next/navigation'\n\nexport async function getSession() {\n  const store = cookies()\n  return store.get('session')\n}\n\nexport async function requireSession() {\n  const session = await getSession()\n  if (!session) redirect('/login')\n}",
        true,
    );
    f.tool(
        None,
        "c2",
        "Grep",
        json!({"pattern": "redirect('/login')"}),
        "src/lib/auth.ts:43\nsrc/middleware.ts:18\nsrc/app/login/page.tsx:7",
        true,
    );
    f.ev(
        None,
        "todo.updated",
        json!({"todos": [
            {"content": "Reproduce the redirect loop", "status": "completed", "active_form": "Reproducing the redirect loop"},
            {"content": "Find the guard that redirects twice", "status": "completed", "active_form": "Finding the guard"},
            {"content": "Fix requireSession and cover it with a test", "status": "in_progress", "active_form": "Fixing requireSession"},
            {"content": "Run the auth tests and lint", "status": "pending", "active_form": "Running the checks"}]}),
    );
    // a sub-agent writes the test while the main agent edits
    f.ev(
        None,
        "subagent.started",
        json!({"agent_id": "a1", "agent_type": "developer", "name": "developer", "tool_call_id": "c3"}),
    );
    f.ev(
        Some("a1"),
        "turn.started",
        json!({"model": "claude-sonnet-5.5", "provider": "anthropic", "iteration": 1}),
    );
    f.ev(
        Some("a1"),
        "assistant.message",
        json!({"stop_reason": "tool_use", "content": [{"type": "text", "text":
            "Reading the authentication flow and locating the redirect guard."}]}),
    );
    f.tool(
        Some("a1"),
        "d1",
        "Read",
        json!({"file_path": "src/middleware.ts"}),
        "export function middleware() {}",
        true,
    );
    f.ev(
        Some("a1"),
        "assistant.message",
        json!({"stop_reason": "tool_use", "content": [{"type": "text", "text":
            "Updating auth.ts and its focused tests."}]}),
    );
    f.tool(
        Some("a1"),
        "d2",
        "Edit",
        json!({"file_path": "tests/auth.test.ts"}),
        "edited",
        true,
    );
    f.ev(
        None,
        "subagent.started",
        json!({"agent_id": "a2", "agent_type": "reviewer", "name": "reviewer"}),
    );
    f.ev(
        None,
        "subagent.stopped",
        json!({"agent_id": "a2", "agent_type": "reviewer", "reason": "done"}),
    );
    f.ev(
        None,
        "tool.requested",
        json!({"tool_call_id": "c4", "name": "Edit", "input": {"file_path": "src/lib/auth.ts"}, "input_truncated": false}),
    );
    f.ev(
        None,
        "tool.completed",
        json!({"tool_call_id": "c4", "name": "Edit", "status": "ok", "is_error": false, "not_run": false,
            "duration_ms": 40, "preview": "edited", "bytes": 6, "truncated": false, "output": "edited",
            "diff": diff()}),
    );
    f.ev(
        None,
        "tool.completed",
        json!({"tool_call_id": "close", "name": "Edit", "status": "ok", "is_error": false, "not_run": false,
            "duration_ms": 1, "preview": "", "bytes": 0, "truncated": false,
            "diff": {"path": "tests/auth.test.ts", "kind": "write", "added": 24, "removed": 0, "truncated": false, "hunks": []}}),
    );
    f.tool(
        None,
        "c5",
        "Bash",
        json!({"command": "pnpm test auth"}),
        "> auth@1.0.0 test\n> vitest run auth\n\n \u{2713} tests/auth.test.ts (14 tests) 212ms\n\n Test Files  1 passed (1)\n      Tests  14 passed (14)",
        true,
    );
    f.tool(
        None,
        "c6",
        "Bash",
        json!({"command": "pnpm lint"}),
        "src/lib/auth.ts\n  44:3  error  'isLoginPath' is not defined  no-undef\n\n1 problem\n\n[exit code 1]",
        false,
    );
    f.tool(
        None,
        "c7",
        "browser_navigate",
        json!({"url": "http://localhost:3000/login"}),
        "navigated",
        true,
    );
    f.ev(
        None,
        "verification.completed",
        json!({"report_id": "r1", "status": "failed", "attempt": 1,
            "summary": "failed: 1 passed, 1 failed, 0 blocked, 1 unverified check(s); 1 limitation(s)"}),
    );
    f.ev(
        None,
        "context.usage",
        json!({"used_tokens": 42100, "window_tokens": 200000, "percent": 21.0, "source": "provider", "stale": false}),
    );
    f.ev(
        None,
        "turn.completed",
        json!({"stop_reason": "tool_use", "duration_ms": 900, "usage": {"input_tokens": 41000, "output_tokens": 1100}}),
    );
    f.out
}

/// The stored report behind `verification.completed` of the sample.
fn report() -> Value {
    json!({"checks": [
        {"id": "k1", "kind": "unit", "command": "pnpm test auth", "status": "passed", "exit_code": 0,
         "duration_ms": 2100, "summary": "14 passed", "blocked_reason": null,
         "tests": {"passed": 14, "failed": 0, "skipped": 0}},
        {"id": "k2", "kind": "lint", "command": "pnpm lint", "status": "failed", "exit_code": 1,
         "duration_ms": 900, "summary": "'isLoginPath' is not defined", "blocked_reason": null},
        {"id": "k3", "kind": "browser_e2e", "command": null, "status": "unverified", "exit_code": null,
         "duration_ms": 0, "summary": "", "blocked_reason": "the browser check needs a person"}],
        "limitations": ["no browser check ran"], "changed_files": ["src/lib/auth.ts", "tests/auth.test.ts"]})
}

/// The sample as a `Live`: the reducers fed the events above, no engine.
pub fn live() -> Live {
    let mut l = Live::new();
    let mut v = ViewState::default();
    for e in events() {
        v.apply(&e);
    }
    v.panes.set_report("r1", &report());
    v.session.cwd = "/Users/dev/synthesis".into();
    l.view = v;
    l.starting = false;
    l.cwd = "/Users/dev/synthesis".into();
    l.sid = "sample01".into();
    l
}

/// The sample with a permission ask waiting (the inline approval row and the dialog).
pub fn live_with_dialog() -> Live {
    let mut l = live();
    l.view.apply(
        &serde_json::from_value(json!({"v": 1, "seq": 99, "ts": "2026-11-07T23:07:00Z",
            "session_id": "s1", "task_id": "t1", "turn_id": null, "type": "tool.approval_required",
            "payload": {"approval_id": "ap1", "tool_call_id": "c8", "dialog": "permission",
                "prompt": "Bash: pnpm install", "options": ["yes", "no"], "suggestions": []}}))
        .expect("sample event"),
    );
    l.on_msg(
        UiMsg::Approval(Box::new(AskApproval {
            id: "ap1".into(),
            agent_id: None,
            tool: "Bash".into(),
            input: json!({"command": "pnpm install"}),
            dialog: "permission",
            prompt: "Bash: pnpm install".into(),
            reason: "this command installs packages from the network".into(),
            origin: None,
            suggestions: vec![],
        })),
        0,
    );
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_feeds_every_pane_of_the_agent_view() {
        let l = live();
        let p = &l.view.panes;
        assert_eq!(p.subs.len(), 2);
        assert_eq!(p.live_subs(), 1);
        assert_eq!(p.sub("a1").unwrap().entries.len(), 4);
        assert_eq!(p.commands.len(), 2);
        assert_eq!(p.commands[1].exit_code, Some(1));
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.totals(), (27, 1));
        assert_eq!(p.browser.len(), 1);
        assert!(p.verify[0].loaded && p.verify[0].checks.len() == 3);
        assert_eq!(l.view.convo.plan.len(), 4);
        assert!(!l.view.convo.items.is_empty());
    }

    #[test]
    fn the_dialog_sample_waits_for_the_person() {
        let l = live_with_dialog();
        assert_eq!(l.dialogs.len(), 1);
        assert_eq!(l.view.convo.needs_you(), 1);
    }
}
