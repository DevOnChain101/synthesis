//! A model message drawn as prose and fenced sections (the chat of the TUI view, the Super Agent session chats
//! and the conductor's chat). Each fenced block is its own box: a header with the kind (command, code, diff,
//! text) and the language, buttons on the right (Copy; Run in terminal for a command, when the owner offers
//! it), then the lines (a diff coloured by line). The classification and the rules for running are pure
//! (`synthesis_view::codeblock`); this file only draws, and the owner decides what a Run click does.

use gpui::{
    AnyElement, ClipboardItem, Div, ElementId, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div,
    prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::codeblock::{self as cb, Block, Kind, Seg};

use super::chrome::{OnClick, dim, strong, text, with_alpha};
use super::seltext::SelText;
use super::syntax;
use crate::theme::hsla;
use crate::tui_view::chat::{tone_bg, tone_fg};

use cb::MAX_LINES;

/// The pointer entered (`true`) or left a button.
pub type OnHover = Box<dyn Fn(&bool, &mut gpui::Window, &mut gpui::App)>;

/// The Run button of a command section: its label (a second click asks for a different one), and the handler.
pub struct RunUi {
    pub label: SharedString,
    /// The label asks for a deliberate second click.
    pub warn: bool,
    pub on_click: OnClick,
    /// The pointer entered (`true`) or left the button: the gate wants it resting before a click counts.
    pub on_hover: Option<OnHover>,
}

/// Whether `segs` holds a fenced block at all (a message without one keeps its plain markdown).
pub fn has_blocks(segs: &[Seg]) -> bool {
    segs.iter().any(|s| matches!(s, Seg::Block(_)))
}

/// The message: `prose(i, markdown)` draws a stretch of prose (the owner's markdown, with its own link and image
/// rules), `run(i, block, command)` gives the Run button of a complete command block (or `None`).
pub fn message(
    id: &str,
    segs: &[Seg],
    tainted: bool,
    prose: &mut dyn FnMut(usize, String) -> AnyElement,
    run: &mut dyn FnMut(usize, &Block, &str) -> Option<RunUi>,
) -> Div {
    let mut col = div().flex().flex_col().gap(px(6.)).min_w(px(0.));
    for (i, s) in segs.iter().enumerate() {
        col = col.child(match s {
            Seg::Prose(p) => prose(i, p.clone()),
            Seg::Block(b) => {
                let cmd = (b.kind == Kind::Command && b.closed && !b.hidden)
                    .then(|| cb::command_text(&b.lang, &b.body));
                // a block longer than what is drawn, or with a tab, is Copy only
                let refusal = cmd.as_deref().and_then(|c| cb::run_refusal(b, c));
                let r = cmd
                    .as_deref()
                    .filter(|_| refusal.is_none())
                    .and_then(|c| run(i, b, c));
                section(
                    SharedString::from(format!("{id}-b{i}")),
                    b,
                    cmd,
                    r,
                    refusal,
                    tainted,
                )
                .into_any_element()
            }
        });
    }
    col
}

fn button(
    id: SharedString,
    label: SharedString,
    warn: bool,
    on_click: OnClick,
    on_hover: Option<OnHover>,
) -> impl IntoElement {
    let c = if warn { hsla(t::WARN) } else { dim() };
    let sel = id.to_string();
    div()
        .id(ElementId::from(id))
        .debug_selector(move || sel.clone())
        .flex_none()
        .px(px(8.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(if warn { c } else { hsla(t::RULE) })
        .text_color(c)
        .text_size(px(11.5))
        .cursor_pointer()
        .hover(|d| d.bg(hsla(t::ELEVATED)).text_color(strong()))
        .on_click(on_click)
        .when_some(on_hover, |d, f| d.on_hover(f))
        .child(label)
}

fn section(
    id: SharedString,
    b: &Block,
    cmd: Option<String>,
    run: Option<RunUi>,
    refusal: Option<&str>,
    tainted: bool,
) -> Div {
    let head = if b.lang.is_empty() || b.lang == b.kind.label() {
        b.kind.label().to_string()
    } else {
        format!("{} \u{b7} {}", b.kind.label(), b.lang)
    };
    let head = match (refusal, tainted && b.kind == Kind::Command) {
        (Some(why), _) => format!("{head} \u{b7} Run off: {why}"),
        (None, true) => format!("{head} \u{b7} based on web/untrusted content"),
        _ => head,
    };
    let mut buttons = div().flex().items_center().gap(px(6.));
    if b.hidden {
        buttons = buttons.child(
            div()
                .text_color(hsla(t::WARN))
                .text_size(px(11.5))
                .child("hidden characters: shown escaped, not copied or run"),
        );
    } else if b.closed {
        let copy = cmd.clone().unwrap_or_else(|| b.body.clone());
        buttons = buttons.child(button(
            format!("{id}-copy").into(),
            "Copy".into(),
            false,
            Box::new(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
            }),
            None,
        ));
        if let Some(r) = run {
            buttons = buttons.child(button(
                format!("{id}-run").into(),
                r.label,
                r.warn,
                r.on_click,
                r.on_hover,
            ));
        }
    }
    // a command shows exactly what Copy and Run take (prompts stripped); the text as written follows, dimmed
    let shown = if b.hidden {
        cb::escape_invisible(&b.body)
    } else {
        cmd.clone().unwrap_or_else(|| b.body.clone())
    };
    let as_written = cmd
        .as_ref()
        .filter(|c| c.as_str() != b.body.trim_end())
        .map(|_| b.body.clone());
    let lines: Vec<&str> = shown.lines().collect();
    let more = lines.len().saturating_sub(MAX_LINES);
    let tones: Vec<_> = if b.kind == Kind::Diff && !b.hidden {
        cb::diff_tones(&shown)
    } else {
        lines
            .iter()
            .map(|l| (synthesis_view::toolview::Tone::Text, *l))
            .collect()
    };
    let mut body = div().flex().flex_col().py(px(5.)).text_size(px(12.5));
    for (i, (tone, l)) in tones.into_iter().take(MAX_LINES).enumerate() {
        let mut row = div()
            .px(px(10.))
            .min_w(px(0.))
            .whitespace_normal()
            .text_color(if b.kind == Kind::Diff {
                tone_fg(tone)
            } else {
                text()
            });
        if b.kind == Kind::Diff
            && let Some(bg) = tone_bg(tone)
        {
            row = row.bg(bg);
        }
        let mut st = SelText::new(
            SharedString::from(format!("{id}-l{i}")),
            if l.is_empty() { " " } else { l }.to_string(),
        );
        if b.kind == Kind::Code {
            st = st.highlights(syntax::highlights(l));
        }
        body = body.child(row.child(st));
    }
    if let Some(w) = as_written {
        body = body.child(
            div()
                .px(px(10.))
                .pt(px(4.))
                .text_color(dim())
                .child("as written:"),
        );
        for (i, l) in w.lines().take(MAX_LINES).enumerate() {
            body = body.child(
                div()
                    .px(px(10.))
                    .min_w(px(0.))
                    .whitespace_normal()
                    .text_color(dim())
                    .child(SelText::new(
                        SharedString::from(format!("{id}-w{i}")),
                        if l.is_empty() { " " } else { l }.to_string(),
                    )),
            );
        }
    }
    if more > 0 {
        body = body.child(
            div()
                .px(px(10.))
                .text_color(dim())
                .child(format!("{more} more lines (Copy takes them all)")),
        );
    }
    div()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .rounded(px(6.))
        .border_1()
        .border_color(hsla(t::BORDER))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(10.))
                .py(px(3.))
                .bg(with_alpha(gpui::black(), 0.16))
                .border_b_1()
                .border_color(hsla(t::RULE))
                .child(div().text_color(dim()).text_size(px(11.5)).child(head))
                .child(buttons),
        )
        .child(body)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{Context, IntoElement, Modifiers, Render, TestAppContext, Window};

    use super::*;

    struct Probe {
        text: &'static str,
        runs: Rc<Cell<usize>>,
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let runs = self.runs.clone();
            let segs = cb::split(self.text);
            message(
                "p",
                &segs,
                false,
                &mut |_, s| div().child(s).into_any_element(),
                &mut |_, _, _| {
                    let runs = runs.clone();
                    Some(RunUi {
                        label: "Run in terminal".into(),
                        warn: false,
                        on_click: Box::new(move |_, _, _| runs.set(runs.get() + 1)),
                        on_hover: None,
                    })
                },
            )
        }
    }

    fn open<'a>(
        cx: &'a mut TestAppContext,
        text: &'static str,
    ) -> (&'a mut gpui::VisualTestContext, Rc<Cell<usize>>) {
        cx.update(gpui_component::init);
        let runs = Rc::new(Cell::new(0));
        let r = runs.clone();
        let (_, v) = cx.add_window_view(move |_, _| Probe { text, runs: r });
        v.run_until_parked();
        (v, runs)
    }

    fn click(v: &mut gpui::VisualTestContext, sel: &'static str) -> bool {
        let Some(b) = v.debug_bounds(sel) else {
            return false;
        };
        v.simulate_click(b.center(), Modifiers::none());
        true
    }

    #[gpui::test]
    fn a_command_has_copy_and_run_and_copy_leaves_the_prompt_out(cx: &mut TestAppContext) {
        let (v, runs) = open(
            cx,
            "go:\n```bash\n$ cargo test\n```\n```rust\nfn a() {}\n```\n",
        );
        assert!(click(v, "p-b1-copy"));
        assert_eq!(
            v.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("cargo test")
        );
        assert!(click(v, "p-b1-run"));
        assert_eq!(runs.get(), 1);
        // code: Copy only, with the raw text
        assert!(v.debug_bounds("p-b2-run").is_none());
        assert!(click(v, "p-b2-copy"));
        assert_eq!(
            v.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("fn a() {}")
        );
    }

    #[gpui::test]
    fn hidden_characters_and_a_streaming_block_have_no_buttons(cx: &mut TestAppContext) {
        let (v, _) = open(cx, "```sh\nls \u{202e}x\n```\n```sh\nls");
        for s in ["p-b0-copy", "p-b0-run", "p-b1-copy", "p-b1-run"] {
            assert!(v.debug_bounds(s).is_none(), "{s}");
        }
    }

    #[gpui::test]
    fn a_block_longer_than_what_is_drawn_or_with_a_tab_is_copy_only(cx: &mut TestAppContext) {
        let long: &'static str =
            Box::leak(format!("```sh\n{}\n```", "echo a\n".repeat(401)).into_boxed_str());
        let (v, _) = open(cx, long);
        assert!(v.debug_bounds("p-b0-copy").is_some() && v.debug_bounds("p-b0-run").is_none());
        let (v, _) = open(cx, "```sh\necho\ta\n```");
        assert!(v.debug_bounds("p-b0-copy").is_some() && v.debug_bounds("p-b0-run").is_none());
    }

    #[gpui::test]
    fn a_slash_command_in_a_text_fence_is_copy_only(cx: &mut TestAppContext) {
        let (v, _) = open(cx, "```text\n/add-plugin pstack\n```");
        assert!(v.debug_bounds("p-b0-copy").is_some() && v.debug_bounds("p-b0-run").is_none());
    }
}
