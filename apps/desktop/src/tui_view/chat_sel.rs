//! Whole-chat copy for the virtualised conversation: Cmd/Ctrl+A selects the full transcript without painting
//! every row (the selection library only knows the rows that are built), Cmd/Ctrl+C copies it, and the
//! right-click menu offers Copy / Copy message / Copy code block / Select all. The text is built from the
//! same items and the same `toolview` cards the rows paint, so it is what the person sees (redacted and
//! sanitized the same way). The assembly is pure (tested); the pointer part is `TuiView`.

use gpui::{ClipboardItem, Context, Pixels, Point};
use synthesis_view::convo::{ApprovalState, CheckState, Item, StepState};
use synthesis_view::toolview;

use super::driver::Row;
use super::quote::{MenuEntry, menu_overlay};
use super::{ChatTab, TuiView};

/// What a whole-chat copy puts on the clipboard at most.
pub const MAX_COPY_BYTES: usize = 2 * 1024 * 1024;

/// The right-click menu of the chat: where it is, and what it would copy, taken when it opened.
pub struct ChatMenu {
    pub at: Point<Pixels>,
    /// Text selected with the pointer when the menu opened.
    pub selection: String,
    pub message: Option<String>,
    pub code: Option<String>,
}

/// Which of Copy, Copy message, Copy code block and Select all can act.
pub fn enabled(m: &ChatMenu, all_selected: bool, has_rows: bool) -> [bool; 4] {
    [
        all_selected || !m.selection.is_empty(),
        m.message.is_some(),
        m.code.is_some(),
        has_rows,
    ]
}

/// `pieces` joined by a blank line, cut at a character boundary once `cap` bytes are reached (true: cut).
/// Lazy: pieces after the cap are never produced.
pub fn assemble(pieces: impl IntoIterator<Item = String>, cap: usize) -> (String, bool) {
    let mut out = String::new();
    for p in pieces {
        let sep = if out.is_empty() { 0 } else { 2 };
        if out.len() + sep + p.len() > cap {
            let room = cap.saturating_sub(out.len() + sep);
            let cut = (0..=room.min(p.len()))
                .rev()
                .find(|i| p.is_char_boundary(*i))
                .unwrap_or(0);
            if sep > 0 && cut > 0 {
                out.push_str("\n\n");
            }
            out.push_str(&p[..cut]);
            return (out, true);
        }
        if sep > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&p);
    }
    (out, false)
}

/// The fenced code blocks of a markdown text, bodies only, joined by a blank line (`None`: there is none).
pub fn code_blocks(md: &str) -> Option<String> {
    let (mut blocks, mut cur, mut fence): (Vec<String>, Vec<&str>, Option<String>) =
        (vec![], vec![], None);
    for l in md.lines() {
        let t = l.trim_start();
        let ticks = t.chars().take_while(|c| *c == '`' || *c == '~').count();
        let mark = t.chars().next().filter(|_| ticks >= 3);
        match (&fence, mark) {
            (None, Some(m)) => fence = Some(m.to_string().repeat(ticks)),
            (Some(f), Some(_)) if t.starts_with(f.as_str()) && t.trim_end().len() == ticks => {
                blocks.push(cur.join("\n"));
                cur.clear();
                fence = None;
            }
            (Some(_), _) => cur.push(l),
            _ => {}
        }
    }
    // an unterminated fence (a streaming reply) is still a block
    if fence.is_some() {
        blocks.push(cur.join("\n"));
    }
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

/// The text of a tool card: header, badges, rows.
fn card_text(c: &toolview::Card) -> String {
    let mut s = c.head.clone();
    if !c.sub.is_empty() {
        s += &format!("  {}", c.sub);
    }
    for b in &c.badges {
        s += &format!("  {}", b.text);
    }
    for r in &c.rows {
        s += &format!("\n{}", r.text);
    }
    s
}

/// What a row says, without the speaker label. `cols` and `open` are the card's (a collapsed card copies
/// what it shows).
pub fn item_body(it: &Item, cols: usize, open: bool) -> Option<String> {
    Some(match it {
        Item::User(s) | Item::Thinking(s) | Item::Internal(s) => s.clone(),
        Item::Assistant { text, .. } | Item::Notice { text, .. } => text.clone(),
        Item::Tool { .. } => card_text(&toolview::shape(it, open, cols)?),
        Item::Verification { status, summary } => format!("verification {status}\n{summary}"),
        Item::Checks {
            rows,
            done,
            summary,
            ..
        } => {
            let mut s = format!("verification {}", done.as_deref().unwrap_or("running"));
            for r in rows {
                let cmd = r.command.as_deref().unwrap_or("(nothing to run)");
                s += &format!("\n{} {cmd}", r.name);
                if !matches!(r.state, CheckState::Running | CheckState::Passed)
                    && !r.tail.trim().is_empty()
                {
                    s += &format!("\n{}", r.tail);
                }
            }
            if done.is_some() && !summary.is_empty() {
                s += &format!("\n{}", synthesis_harness::sanitize_for_display(summary));
            }
            s
        }
        Item::Approval { prompt, state, .. } => {
            let w = match state {
                ApprovalState::Pending => "waiting for you",
                ApprovalState::Allowed => "allowed",
                ApprovalState::Denied => "denied",
            };
            format!("permission {w} {prompt}")
        }
        Item::Agent { kind, running, .. } => format!(
            "{kind} agent {}",
            if *running {
                "running in the background"
            } else {
                "finished"
            }
        ),
        Item::TaskEnd { text, checks, .. } => format!("{text}\n{checks}").trim_end().to_string(),
    })
}

/// A row with its speaker label, as the transcript has it.
pub fn item_piece(it: &Item, cols: usize, open: bool) -> Option<String> {
    let body = item_body(it, cols, open)?;
    Some(match it {
        Item::User(_) => format!("YOU\n{body}"),
        Item::Assistant { .. } => format!("SYNTHESIS\n{body}"),
        _ => body,
    })
}

fn plan_text(steps: &[synthesis_view::convo::Step]) -> String {
    let mut s = "Plan".to_string();
    for p in steps {
        let mark = match p.state {
            StepState::Done => "[x]",
            StepState::Active => "[~]",
            StepState::Todo => "[ ]",
        };
        s += &format!("\n{mark} {}", p.text);
    }
    s
}

impl TuiView {
    fn card_cols(&self) -> usize {
        super::chat::cols_for(self.centre_w - super::chat::INDENT - 40.)
    }

    fn is_open(&self, it: &Item) -> bool {
        let (Some(lu), Item::Tool { id, .. }) = (&self.live, it) else {
            return false;
        };
        lu.live.expand_all || lu.live.expanded.contains(id)
    }

    /// The whole shown conversation as text (rows hidden by the quiet-chat rules are not in it).
    fn transcript(&self) -> (String, bool) {
        let Some(lu) = &self.live else {
            return (String::new(), false);
        };
        let c = lu.live.convo();
        let cols = self.card_cols();
        assemble(
            lu.rows.iter().filter_map(|r| match r {
                Row::Item(i) => {
                    let it = c.items.get(*i)?;
                    item_piece(it, cols, self.is_open(it))
                }
                Row::Plan => Some(plan_text(&c.plan)),
                Row::Activity => None,
            }),
            MAX_COPY_BYTES,
        )
    }

    pub(super) fn select_all_chat(&mut self, cx: &mut Context<Self>) {
        self.ts.chat_all = true;
        self.ts.chat_menu = None;
        cx.notify();
    }

    pub(super) fn copy_transcript(&mut self, cx: &mut Context<Self>) {
        let (t, cut) = self.transcript();
        if t.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(t));
        if cut && let Some(lu) = &mut self.live {
            lu.live.note(
                "the conversation is longer than 2 MB: the first 2 MB were copied",
                true,
            );
            lu.dirty = true;
        }
    }

    /// Right-click on row `ix`: the menu opens at the pointer with what it could copy.
    pub(super) fn open_chat_menu(
        &mut self,
        ix: usize,
        at: Point<Pixels>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lu) = &self.live else { return };
        let selection = if gpui_base::TextSelection::has_selection(window, cx) {
            gpui_base::TextSelection::selected_text(window, cx)
        } else {
            String::new()
        };
        let item = match lu.rows.get(ix) {
            Some(Row::Item(i)) => lu.live.convo().items.get(*i),
            _ => None,
        };
        let message = item.and_then(|it| item_body(it, self.card_cols(), self.is_open(it)));
        let code = match item {
            Some(Item::Assistant { text, .. }) => code_blocks(text),
            _ => None,
        };
        self.ts.menu = None;
        self.ts.chat_menu = Some(ChatMenu {
            at,
            selection,
            message,
            code,
        });
        cx.notify();
    }

    pub(super) fn chat_menu_view(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let m = self.ts.chat_menu.as_ref()?;
        let has_rows = self.live.as_ref().is_some_and(|l| !l.rows.is_empty());
        let on = enabled(m, self.ts.chat_all, has_rows);
        Some(menu_overlay(
            m.at,
            vec![
                MenuEntry::new("menu-copy", "Copy", on[0], |t, cx| {
                    if t.ts.chat_all {
                        t.copy_transcript(cx);
                    } else if let Some(m) = &t.ts.chat_menu {
                        cx.write_to_clipboard(ClipboardItem::new_string(m.selection.clone()));
                    }
                }),
                MenuEntry::new("menu-message", "Copy message", on[1], |t, cx| {
                    if let Some(s) = t.ts.chat_menu.as_ref().and_then(|m| m.message.clone()) {
                        cx.write_to_clipboard(ClipboardItem::new_string(s));
                    }
                }),
                MenuEntry::new("menu-code", "Copy code block", on[2], |t, cx| {
                    if let Some(s) = t.ts.chat_menu.as_ref().and_then(|m| m.code.clone()) {
                        cx.write_to_clipboard(ClipboardItem::new_string(s));
                    }
                }),
                MenuEntry::new("menu-all", "Select all", on[3], |t, cx| {
                    t.select_all_chat(cx)
                }),
            ],
            cx,
        ))
    }
}

/// Cmd/Ctrl+A selects the whole chat in the chat tab: the input keeps its own select-all while it holds text.
pub fn select_all_applies(tab: ChatTab, input_empty: bool, has_selection: bool) -> bool {
    tab == ChatTab::Chat && (input_empty || has_selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_join_with_a_blank_line_and_stop_at_the_cap() {
        let p = |s: &str| s.to_string();
        assert_eq!(
            assemble([p("a"), p("b")], 100),
            ("a\n\nb".to_string(), false)
        );
        assert_eq!(assemble([], 10), (String::new(), false));
        // "ab" + 2 + "cd" = 6 > 5: the second piece is cut to what is left
        assert_eq!(
            assemble([p("ab"), p("cdef")], 5),
            ("ab\n\nc".to_string(), true)
        );
        assert_eq!(assemble([p("ab"), p("cdef")], 3), ("ab".to_string(), true));
        // never inside a character
        assert_eq!(
            assemble([p("\u{e9}\u{e9}")], 3),
            ("\u{e9}".to_string(), true)
        );
        // pieces past the cap are never made
        let mut made = 0;
        let it = (0..1000).map(|_| {
            made += 1;
            "x".repeat(10)
        });
        assert!(assemble(it, 25).1);
        assert!(made <= 3);
    }

    #[test]
    fn code_blocks_come_from_fences_only() {
        assert_eq!(code_blocks("no code `here`"), None);
        let md = "text\n```rust\nfn a() {}\n```\nmore\n~~~\nb\n~~~\n";
        assert_eq!(code_blocks(md).as_deref(), Some("fn a() {}\n\nb"));
        // a longer fence is not closed by a shorter one; an open fence still counts
        assert_eq!(code_blocks("````\n```\nx\n````").as_deref(), Some("```\nx"));
        assert_eq!(code_blocks("```\nstreaming").as_deref(), Some("streaming"));
    }

    #[test]
    fn rows_become_labelled_text_and_tool_output_is_the_redacted_card() {
        let user = item_piece(&Item::User("hi".into()), 80, false).unwrap();
        assert_eq!(user, "YOU\nhi");
        let a = Item::Assistant {
            text: "yo".into(),
            streaming: false,
        };
        assert_eq!(item_piece(&a, 80, false).unwrap(), "SYNTHESIS\nyo");
        let n = Item::Notice {
            warning: true,
            text: "careful".into(),
        };
        assert_eq!(item_piece(&n, 80, false).unwrap(), "careful");
    }

    #[test]
    fn select_all_leaves_the_inputs_own_to_the_input() {
        assert!(select_all_applies(ChatTab::Chat, true, false));
        assert!(select_all_applies(ChatTab::Chat, false, true));
        assert!(!select_all_applies(ChatTab::Chat, false, false));
        assert!(!select_all_applies(ChatTab::Files, true, true));
    }

    #[test]
    fn the_menu_enables_what_it_can_copy() {
        let m = |sel: &str, msg: bool, code: bool| ChatMenu {
            at: Point::default(),
            selection: sel.into(),
            message: msg.then(|| "m".into()),
            code: code.then(|| "c".into()),
        };
        assert_eq!(
            enabled(&m("", false, false), false, true),
            [false, false, false, true]
        );
        assert_eq!(
            enabled(&m("x", true, true), false, true),
            [true, true, true, true]
        );
        // select-all active: Copy works without a pointer selection
        assert_eq!(
            enabled(&m("", true, false), true, true),
            [true, true, false, true]
        );
        assert!(!enabled(&m("", false, false), false, false)[3]);
    }
}
