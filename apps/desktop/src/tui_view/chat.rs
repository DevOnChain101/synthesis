//! The centre of the TUI view when it is live: the conversation drawn from `Convo` with the CLI's "quiet
//! chat" rules (docs/CLI-APP.md "Events": what a person needs, nothing else), the prompt box with its
//! popups, and the "jump to latest" button.

use gpui::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, list, prelude::*, px,
};
use gpui_component::input::Textarea;
use gpui_component::text::TextView;
use synthesis_cli_ui_render::theme as t;
use synthesis_view::convo::{ApprovalState, CheckRow, CheckState, Item, StepState};
use synthesis_view::toolview::{self, Glyph, Tone};

use super::panels::{dim, icon, strong, text};
use super::topo::with_alpha;
use super::tracked;
use super::{TuiView, layout};
use crate::live::{clip, label};
use crate::theme::hsla;
use crate::widgets::syntax;

/// Lines of a tool's output shown before "ctrl+o" (the CLI's default).
pub const COLLAPSED_LINES: usize = 5;

/// Whether a row of the quiet chat is shown. `verbose` is the CLI's switch for the internal rows
/// (`Item::Internal`: memory, peer and hook bookkeeping) and widens what approvals show.
pub fn visible(it: &Item, show_thinking: bool, verbose: bool, focus: bool) -> bool {
    // `/focus`: prompts, answers, results and notices only (the CLI's focus kinds)
    if focus
        && !matches!(
            it,
            Item::User(_) | Item::Assistant { .. } | Item::TaskEnd { .. } | Item::Notice { .. }
        )
    {
        return false;
    }
    match it {
        Item::Thinking(t) => show_thinking && !t.trim().is_empty(),
        // a granted permission is bookkeeping; waiting and denied ones are what a person needs
        Item::Approval { state, .. } => verbose || *state != ApprovalState::Allowed,
        Item::Internal(_) => verbose,
        // `!cmd`: the Bash card below it already names the command
        Item::User(t) => !t.starts_with('!'),
        Item::Agent { .. } => true,
        Item::Assistant { text, streaming } => *streaming || !text.trim().is_empty(),
        _ => true,
    }
}

/// `Some` when the chat holds nothing but notices (or nothing): the latest notice as (warning, text).
fn notices_only(rows: &[super::driver::Row], items: &[Item]) -> Option<Option<(bool, String)>> {
    let mut last = None;
    for r in rows {
        match r {
            super::driver::Row::Item(i) => match items.get(*i) {
                Some(Item::Notice { warning, text }) => last = Some((*warning, text.clone())),
                _ => return None,
            },
            _ => return None,
        }
    }
    Some(last)
}

/// Left edge of what sits under a speaker's text: the row's 10px padding, the 34px avatar column, the 10px gap.
const INDENT: f32 = 54.;

/// Characters of monospace text (7.5 px each) that fit in `avail` px.
fn cols_for(avail: f32) -> usize {
    ((avail / 7.5) as usize).max(20)
}

/// `text` as the rows it takes on screen: split at line ends, each line cut every `cols` characters, so
/// one very long line counts as the rows it fills.
fn wrap_rows(text: &str, cols: usize) -> Vec<String> {
    let mut out = vec![];
    for l in text.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let cs: Vec<char> = l.chars().collect();
        if cs.is_empty() {
            out.push(String::new());
        }
        out.extend(cs.chunks(cols).map(|c| c.iter().collect::<String>()));
    }
    out
}

/// The toggle under a clipped tool output.
fn toggle_label(cut: bool, more: usize) -> String {
    if cut {
        format!(
            "Show more ({more} more line{})",
            if more == 1 { "" } else { "s" }
        )
    } else {
        "Show less".to_string()
    }
}

/// `.msg`: a 32px avatar (radius 7) and, beside it, the tracked who label over the body (`cli.html`).
fn msg(avatar: Div, who: &str, who_colour: gpui::Hsla, time: &str, body: Div) -> Div {
    div()
        .flex()
        .gap(px(10.))
        .px(px(10.))
        .py(px(8.))
        .child(
            div()
                .size(px(32.))
                .flex_none()
                .rounded(px(7.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .overflow_hidden()
                .child(avatar.rounded(px(6.))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(10.))
                        .child(
                            tracked(who, 12.5, 0.06, who_colour).font_weight(FontWeight::SEMIBOLD),
                        )
                        .child(div().text_color(dim()).child(time.to_string())),
                )
                .child(body),
        )
}

/// The title of a tool card (`cli.html`: "Reading file", "Searching", "Running", "Applying patch").
fn tool_title(name: &str) -> String {
    match name {
        "Read" => "Reading file".into(),
        "Grep" | "Glob" | "WebSearch" | "WebFetch" => "Searching".into(),
        "Bash" | "BashOutput" | "KillShell" => "Running".into(),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "Applying patch".into(),
        "Task" => "Agent".into(),
        n if n.starts_with("mcp__") => "MCP".into(),
        n if n.starts_with("browser_") => format!("Browser {}", n.trim_start_matches("browser_")),
        n => n.to_string(),
    }
}

/// The icon and title colour of a tool card (`.tool .th`): by what the tool does.
fn tool_look(name: &str) -> (&'static str, gpui::Hsla) {
    match name {
        "Read" => ("book-open", hsla(t::INFO)),
        "Grep" | "Glob" => ("search", hsla(t::ACCENT_LIGHT)),
        "Bash" => ("square-terminal", hsla(t::WARN)),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ("replace", hsla(t::DIFF_REMOVE)),
        _ => ("wrench", hsla(t::SUCCESS)),
    }
}

/// Text colour of a [`Tone`] of a tool card.
fn tone_fg(tone: Tone) -> gpui::Hsla {
    match tone {
        Tone::Text => text(),
        Tone::Dim => dim(),
        Tone::Strong => strong(),
        Tone::Accent => hsla(t::ACCENT_LIGHT),
        Tone::Ok | Tone::Add => hsla(t::SUCCESS),
        Tone::Err | Tone::Del => hsla(t::ERROR),
        Tone::Warn => hsla(t::WARN),
        Tone::Hunk => hsla(t::INFO),
    }
}

/// Background of a diff row.
fn tone_bg(tone: Tone) -> Option<gpui::Hsla> {
    match tone {
        Tone::Add => Some(with_alpha(hsla(t::SUCCESS), 0.10)),
        Tone::Del => Some(with_alpha(hsla(t::ERROR), 0.10)),
        _ => None,
    }
}

fn line_box(s: &str, c: gpui::Hsla) -> Div {
    div()
        .min_w(px(0.))
        .whitespace_normal()
        .text_color(c)
        .child(if s.is_empty() {
            " ".to_string()
        } else {
            s.to_string()
        })
}

impl TuiView {
    /// The conversation, with the picked sub-agent's transcript beside it (`.chatstage.with-subagent`: the
    /// conversation takes 2fr, the sub-agent 1fr, at least 220px).
    pub(super) fn chat_stage(&self, cx: &mut Context<Self>) -> AnyElement {
        let inner = self.centre_w - 2.;
        let sub_w = (inner / 3.).max(220.).min(inner);
        let sub = self
            .ts
            .sub
            .as_deref()
            // the transcript goes with its roster row (viewable for 5 minutes after it finished)
            .filter(|id| self.snap.roster.iter().any(|r| r.id == *id))
            .and_then(|id| self.sub_pane(id, sub_w, cx));
        if sub.is_none() {
            return self.conversation(cx);
        }
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.conversation(cx)),
            )
            .children(sub)
            .into_any_element()
    }

    /// `.subagent-view`: the transcript of one sub-agent, its model and its state.
    fn sub_pane(&self, id: &str, w: f32, cx: &mut Context<Self>) -> Option<Div> {
        let lu = self.live.as_ref()?;
        let a = lu.live.view.panes.sub(id)?;
        let tz = lu.cfg.tz_min;
        let live = a.live();
        Some(
            div()
                .debug_selector(|| "subagent".to_string())
                .w(px(w))
                .flex_none()
                .min_h(px(0.))
                .my(px(10.))
                .flex()
                .flex_col()
                .overflow_hidden()
                .border_1()
                .border_color(hsla(t::ACCENT))
                .rounded(px(7.))
                .bg(super::with_alpha_black(0.72))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .justify_between()
                        .gap(px(8.))
                        .px(px(10.))
                        .py(px(8.))
                        .border_b_1()
                        .border_color(hsla(t::RULE))
                        .whitespace_nowrap()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_color(hsla(t::ACCENT))
                                .font_weight(FontWeight::BOLD)
                                .child(super::panels::dot(hsla(t::ACCENT), live))
                                .child(format!("@{}", a.name)),
                        )
                        .child(
                            div().min_w(px(0.)).truncate().text_color(text()).child(
                                div()
                                    .flex()
                                    .gap(px(5.))
                                    .child(div().text_color(hsla(t::INFO)).child(
                                        if a.model.is_empty() {
                                            a.kind.clone()
                                        } else {
                                            a.model.clone()
                                        },
                                    ))
                                    .child("*")
                                    .child(
                                        div().text_color(hsla(t::ACCENT)).child(a.status.clone()),
                                    ),
                            ),
                        ),
                )
                .child(
                    div()
                        .id("subagent-log")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .track_scroll(&self.sub_scroll)
                        .px(px(10.))
                        .py(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(9.))
                        .children(a.entries.iter().enumerate().map(|(i, e)| {
                            let tool = e.kind == synthesis_view::agent::EntryKind::Tool;
                            let c = if e.failed {
                                hsla(t::ERROR)
                            } else if tool {
                                text()
                            } else {
                                strong()
                            };
                            div()
                                .pl(px(8.))
                                .border_l_2()
                                .border_color(hsla(t::ACCENT))
                                .child(div().text_size(px(11.)).text_color(dim()).child(
                                    e.ts.map_or(String::new(), |ms| super::snapshot::clock(ms, tz)),
                                ))
                                .child(
                                    self.clamped(
                                        format!("sub-{id}-{i}"),
                                        wrap_rows(
                                            &if tool {
                                                format!("\u{25B8} {}", e.text)
                                            } else {
                                                e.text.clone()
                                            },
                                            cols_for(w - 60.),
                                        )
                                        .into_iter()
                                        .map(|l| (l, c, None))
                                        .collect(),
                                        cx,
                                    ),
                                )
                        }))
                        .when(a.entries.is_empty(), |d| {
                            d.child(div().text_color(dim()).child("nothing yet"))
                        }),
                ),
        )
    }

    /// The scrolling conversation (or the welcome screen when there is nothing yet). A virtual
    /// list: only the rows near the viewport are built, so a 2000-row session costs what a short
    /// one does.
    pub(super) fn conversation(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(lu) = &self.live else {
            return div().into_any_element();
        };
        // no message yet (notices alone, e.g. after /close-workspace or /clear): the welcome, with the latest notice
        if let Some(n) = notices_only(&lu.rows, &lu.live.view.convo.items) {
            return self
                .welcome()
                .children(n.map(|(warning, text)| {
                    div()
                        .max_w(px(560.))
                        .px(px(16.))
                        .mt(px(12.))
                        .text_color(if warning { hsla(t::WARN) } else { dim() })
                        .child(text)
                }))
                .into_any_element();
        }
        let rows = list(
            lu.list.clone(),
            cx.processor(|this: &mut Self, ix: usize, _window, cx| this.row(ix, cx)),
        )
        .size_full()
        .pb(px(10.));
        // the padding is outside the list: a row scrolled to the top edge stops below it
        let mut wrap = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .pt(px(10.))
            .debug_selector(|| "chat-scroll".to_string())
            // a click anywhere on the chat gives the prompt the keys (rows and buttons handle theirs first)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if let Some(lu) = &mut this.live {
                        lu.live.focus = crate::live::Focus::Chat;
                        lu.dirty = true;
                        lu.prompt.update(cx, |s, cx| s.focus(window, cx));
                        cx.notify();
                    }
                }),
            )
            .child(rows);
        // only when the content is taller than the viewport and the person has scrolled up away from the end
        let max = f32::from(lu.list.max_offset_for_scrollbar().y);
        let at = -f32::from(lu.list.scroll_px_offset_for_scrollbar().y);
        if jump_visible(max, at) {
            wrap = wrap.child(
                div()
                    .id("jump-latest")
                    .absolute()
                    .bottom(px(10.))
                    .right(px(24.))
                    .px(px(12.))
                    .py(px(4.))
                    .rounded(px(14.))
                    .border_1()
                    .border_color(hsla(t::ACCENT))
                    .bg(hsla(t::ELEVATED))
                    .text_color(strong())
                    .cursor_pointer()
                    .child("\u{2193} Jump to latest")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(lu) = &mut this.live {
                            lu.stick();
                        }
                        cx.notify();
                    })),
            );
        }
        wrap.into_any_element()
    }

    /// Row `ix` of the chat list, with the gap below it.
    fn row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(lu) = &self.live else {
            return div().into_any_element();
        };
        let c = lu.live.convo();
        // everything but the messages sits under the speaker's text (avatar column + gap)
        let under = |d: AnyElement| div().ml(px(INDENT)).child(d).into_any_element();
        let el = match lu.rows.get(ix) {
            Some(super::driver::Row::Item(i)) => match c.items.get(*i) {
                Some(it @ (Item::User(_) | Item::Assistant { .. })) => self.item(*i, it, cx),
                Some(it) => under(self.item(*i, it, cx)),
                None => div().into_any_element(),
            },
            Some(super::driver::Row::Plan) => under(self.plan_card().into_any_element()),
            Some(super::driver::Row::Activity) => under(self.activity_line(lu)),
            None => div().into_any_element(),
        };
        // the row a click in Recent Activity jumped to glows for a moment
        let flash = match (lu.rows.get(ix), lu.flash) {
            (Some(super::driver::Row::Item(i)), Some((f, until))) => {
                *i == f && crate::live::now_ms() < until
            }
            _ => false,
        };
        div()
            .pb(px(10.))
            .child(
                div()
                    .rounded(px(6.))
                    .when(flash, |d| d.bg(with_alpha(hsla(t::ACCENT), 0.22)))
                    .child(el),
            )
            .into_any_element()
    }

    /// The working line, as the Claude Code CLI draws it: a spinner glyph, a verb, then (elapsed, tokens, esc
    /// hint). Elapsed is from the task's start event, tokens from the usage of its finished turns (left out
    /// while there are none); a running tool or sub-agent names itself instead of the verb.
    fn activity_line(&self, lu: &super::driver::LiveUi) -> AnyElement {
        let c = &lu.live.view.convo;
        let motion = lu.cfg.tui.motion;
        let now = crate::live::now_ms();
        let secs = c
            .task_started_ms()
            .map_or(0, |t0| ((now - t0) / 1000).max(0) as u64);
        use super::activity::{Activity, activity_of};
        let act = activity_of(&lu.live).unwrap_or(Activity::Thinking);
        let waiting = act == Activity::Approval;
        let what = match &act {
            Activity::Approval => "waiting for your approval".to_string(),
            Activity::Compacting => "Compacting the conversation\u{2026}".to_string(),
            Activity::Agent(kind) => format!("{kind} agent working\u{2026}"),
            Activity::Tool(tool) => format!("Running {tool}\u{2026}"),
            Activity::Verifying => "Verifying the result\u{2026}".to_string(),
            Activity::Starting => "Starting the session\u{2026}".to_string(),
            Activity::Thinking => format!("{}\u{2026}", activity_verb(secs)),
        };
        let tone = if waiting {
            hsla(t::WARN)
        } else {
            hsla(t::ACCENT)
        };
        let mut meta = vec![];
        if c.task_started_ms().is_some() {
            meta.push(elapsed_label(secs));
        }
        if c.task_tokens() > 0 {
            meta.push(format!("\u{2193} {} tokens", token_label(c.task_tokens())));
        }
        meta.push(
            if waiting {
                "answer below"
            } else {
                "esc to interrupt"
            }
            .to_string(),
        );
        let line = div()
            .id("activity-line")
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .text_color(tone)
                    .child(if motion { spinner_glyph(now) } else { "*" }),
            )
            .child(div().text_color(tone).child(what))
            .child(
                div()
                    .text_color(dim())
                    .child(format!("({})", meta.join(" \u{b7} "))),
            );
        if motion {
            // frames while busy: the glyph and the clock are read again on every one
            use gpui::AnimationExt as _;
            line.with_animation(
                "activity-frames",
                gpui::Animation::new(std::time::Duration::from_millis(800))
                    .repeat()
                    .with_max_fps(crate::widgets::chrome::ANIM_FPS),
                |el, _| el,
            )
            .into_any_element()
        } else {
            line.into_any_element()
        }
    }

    fn plan_card(&self) -> Div {
        let Some(lu) = &self.live else { return div() };
        let steps = &lu.live.convo().plan;
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(10.))
            .border_1()
            .border_color(hsla(t::BORDER))
            .rounded(px(layout::PANEL_RADIUS))
            .child(
                div()
                    .text_color(strong())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Plan"),
            )
            .children(steps.iter().map(|s| {
                let (mark, c) = match s.state {
                    StepState::Done => ("\u{2611}", hsla(t::SUCCESS)),
                    StepState::Active => ("\u{25D0}", hsla(t::ACCENT)),
                    StepState::Todo => ("\u{2610}", dim()),
                };
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().text_color(c).child(mark))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_color(if s.state == StepState::Done {
                                dim()
                            } else {
                                text()
                            })
                            .child(s.text.clone()),
                    )
            }))
    }

    fn item(&self, idx: usize, it: &Item, cx: &mut Context<Self>) -> AnyElement {
        let Some(lu) = &self.live else {
            return div().into_any_element();
        };
        // `HH:MM` of the event that made the row
        let time = lu.live.convo().time_of(idx).map_or(String::new(), |ms| {
            super::snapshot::clock(ms, lu.cfg.tz_min)[..5].to_string()
        });
        match it {
            Item::User(s) => {
                let (head, quoted) = super::quote::split(s);
                msg(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(hsla(t::INFO))
                        .child(icon("user", 14., hsla(t::BG))),
                    "YOU",
                    hsla(t::INFO),
                    &time,
                    div()
                        .text_color(strong())
                        .child(head)
                        .children(super::quote::blocks(&quoted)),
                )
                .into_any_element()
            }
            Item::Assistant { text: s, .. } => msg(
                div().size_full().bg(gpui::black()).child(
                    gpui::img(crate::brand::mark())
                        .size_full()
                        .rounded(px(6.))
                        .object_fit(gpui::ObjectFit::Cover),
                ),
                "SYNTHESIS",
                hsla(t::ACCENT),
                &time,
                div()
                    .min_w(px(0.))
                    .text_color(strong())
                    .child(TextView::markdown(("assistant", idx), s.clone())),
            )
            .into_any_element(),
            Item::Thinking(s) => div()
                .pl(px(10.))
                .border_l_2()
                .border_color(hsla(t::BORDER))
                .text_color(dim())
                .italic()
                .child(s.clone())
                .into_any_element(),
            Item::Tool { .. } => self.tool_card(idx, it, cx),
            Item::Verification { status, summary } => {
                let ok = status == "passed";
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_color(if ok { hsla(t::SUCCESS) } else { hsla(t::WARN) })
                            .child(format!(
                                "{} verification {status}",
                                if ok { "\u{2713}" } else { "!" }
                            )),
                    )
                    .child(
                        self.clamped(
                            format!("row-{idx}"),
                            wrap_rows(summary, cols_for(self.centre_w - INDENT - 40.))
                                .into_iter()
                                .map(|l| (l, dim(), None))
                                .collect(),
                            cx,
                        ),
                    )
                    .into_any_element()
            }
            Item::Checks {
                rows,
                total,
                done,
                summary,
            } => self.checks_card(idx, rows, *total, done.as_deref(), summary, cx),
            Item::Approval { prompt, state, .. } => {
                let (word, c) = match state {
                    ApprovalState::Pending => ("waiting for you", hsla(t::WARN)),
                    ApprovalState::Allowed => ("allowed", hsla(t::SUCCESS)),
                    ApprovalState::Denied => ("denied", hsla(t::ERROR)),
                };
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().text_color(c).child(format!("permission {word}")))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_color(dim())
                            .child(prompt.clone()),
                    )
                    .into_any_element()
            }
            Item::Notice { warning, text: s } => {
                let c = if *warning { hsla(t::WARN) } else { dim() };
                let rows = wrap_rows(s, cols_for(self.centre_w - INDENT - 40.));
                self.clamped(
                    format!("row-{idx}"),
                    rows.into_iter().map(|l| (l, c, None)).collect(),
                    cx,
                )
                .text_color(c)
                .into_any_element()
            }
            Item::Agent { kind, running, .. } => div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_color(dim())
                .child(super::panels::dot(hsla(t::ACCENT), *running))
                .child(format!(
                    "{kind} agent {}",
                    if *running {
                        "running in the background"
                    } else {
                        "finished"
                    }
                ))
                .into_any_element(),
            Item::Internal(s) => self
                .clamped(
                    format!("row-{idx}"),
                    wrap_rows(s, cols_for(self.centre_w - INDENT - 40.))
                        .into_iter()
                        .map(|l| (l, dim(), None))
                        .collect(),
                    cx,
                )
                .into_any_element(),
            Item::TaskEnd {
                ok,
                text: s,
                word,
                checks,
            } => {
                // unverified / blocked / cancelled are a warning, failed is an error, never green
                let (mark, c) = match (*ok, word.as_str()) {
                    (true, _) => ("\u{2713}", hsla(t::SUCCESS)),
                    (_, "unverified" | "blocked" | "cancelled") => ("!", hsla(t::WARN)),
                    _ => ("\u{2717}", hsla(t::ERROR)),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_color(c).child(format!("{mark} {s}")))
                    .child(
                        self.clamped(
                            format!("row-{idx}"),
                            wrap_rows(checks, cols_for(self.centre_w - INDENT - 40.))
                                .into_iter()
                                .map(|l| (l, dim(), None))
                                .collect(),
                            cx,
                        ),
                    )
                    .into_any_element()
            }
        }
    }

    /// Output rows clipped to [`COLLAPSED_LINES`] with a "Show more" / "Show less" toggle (state kept in
    /// `expanded` under `key`). Every block of command or tool output in the chat goes through here.
    fn clamped(
        &self,
        key: String,
        rows: Vec<(String, gpui::Hsla, Option<gpui::Hsla>)>,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self
            .live
            .as_ref()
            .is_some_and(|lu| lu.live.expand_all || lu.live.expanded.contains(&key));
        let total = rows.len();
        let cut = !open && total > COLLAPSED_LINES;
        let more = total - COLLAPSED_LINES.min(total);
        div()
            .flex()
            .flex_col()
            .children(
                rows.into_iter()
                    .take(if cut { COLLAPSED_LINES } else { usize::MAX })
                    .map(|(s, fg, bg)| {
                        let d = line_box(&s, fg);
                        match bg {
                            Some(b) => d.bg(b),
                            None => d,
                        }
                    }),
            )
            .when(cut || (open && total > COLLAPSED_LINES), |d| {
                d.child(
                    div()
                        .id(gpui::SharedString::from(format!("more-{key}")))
                        .debug_selector({
                            let k = format!("more-{key}");
                            move || k.clone()
                        })
                        .cursor_pointer()
                        .text_color(hsla(t::ACCENT))
                        .child(toggle_label(cut, more))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(lu) = &mut this.live {
                                let e = &mut lu.live.expanded;
                                if !e.remove(&key) {
                                    e.insert(key.clone());
                                }
                            }
                            cx.notify();
                        })),
                )
            })
    }

    /// The completion gate's checks: a title, then one row per check (spinner while it runs, the
    /// result mark after it, its duration) and under a check that did not pass the end of its output,
    /// clipped like every output block.
    fn checks_card(
        &self,
        idx: usize,
        rows: &[CheckRow],
        total: usize,
        done: Option<&str>,
        summary: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let motion = self.live.as_ref().is_some_and(|lu| lu.cfg.tui.motion);
        let cols = cols_for(self.centre_w - INDENT - 40.);
        let finished = rows
            .iter()
            .filter(|r| r.state != CheckState::Running)
            .count();
        let (word, c) = match done {
            Some("passed") => ("passed".to_string(), hsla(t::SUCCESS)),
            Some("failed") => ("failed".to_string(), hsla(t::ERROR)),
            Some("ended") => ("interrupted".to_string(), dim()),
            Some(w) => (w.to_string(), hsla(t::WARN)),
            None => (format!("{finished}/{total}"), hsla(t::WARN)),
        };
        div()
            .id(("checks", idx))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(div().text_color(c).child(format!("verification {word}")))
            .children(rows.iter().enumerate().map(|(i, r)| {
                let (mark, fg) = match r.state {
                    CheckState::Running if motion => {
                        (spinner_glyph(crate::live::now_ms()), hsla(t::WARN))
                    }
                    CheckState::Running => ("*", hsla(t::WARN)),
                    CheckState::Passed => ("\u{2713}", hsla(t::SUCCESS)),
                    CheckState::Failed => ("\u{2717}", hsla(t::ERROR)),
                    CheckState::Unverified | CheckState::Blocked => ("!", hsla(t::WARN)),
                };
                let secs = if r.state == CheckState::Running {
                    String::new()
                } else {
                    format!("  {:.1}s", r.duration_ms as f64 / 1000.)
                };
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().flex_none().text_color(fg).child(mark))
                            .child(div().flex_none().text_color(text()).child(r.name.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(dim())
                                    .child(format!(
                                        "{}{secs}",
                                        r.command.as_deref().unwrap_or("(nothing to run)")
                                    )),
                            ),
                    )
                    .when(
                        !matches!(r.state, CheckState::Running | CheckState::Passed)
                            && !r.tail.trim().is_empty(),
                        |d| {
                            d.child(
                                div().pl(px(20.)).child(
                                    self.clamped(
                                        format!("row-{idx}-{i}"),
                                        wrap_rows(&r.tail, cols.saturating_sub(3).max(20))
                                            .into_iter()
                                            .map(|l| (l, dim(), None))
                                            .collect(),
                                        cx,
                                    ),
                                ),
                            )
                        },
                    )
            }))
            .when(done.is_some() && !summary.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(dim())
                        .child(synthesis_harness::sanitize_for_display(summary)),
                )
            })
            .into_any_element()
    }

    /// Opens a clipped card whole, or clips it again (the state of its "Show more" too).
    pub(super) fn toggle_card(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(lu) = &mut self.live {
            let e = &mut lu.live.expanded;
            if !e.remove(key) {
                e.insert(key.to_string());
            }
        }
        cx.notify();
    }

    /// One tool call: the header (icon, title, what it did, badges, state glyph, chevron) and the rows
    /// `toolview::shape` made for its kind. A click on the header opens or clips the body.
    fn tool_card(&self, idx: usize, it: &Item, cx: &mut Context<Self>) -> AnyElement {
        let Item::Tool { id, name, .. } = it else {
            return div().into_any_element();
        };
        let lu = self.live.as_ref();
        let open = lu.is_some_and(|lu| lu.live.expand_all || lu.live.expanded.contains(id));
        // shaped once per change of the call, not on every paint
        thread_local! {
            static CARDS: std::cell::RefCell<toolview::CardCache> = std::cell::RefCell::default();
        }
        let cols = cols_for(self.centre_w - INDENT - 40.);
        let Some(card) = CARDS.with(|c| c.borrow_mut().get(it, open, cols)) else {
            return div().into_any_element();
        };
        let (mark, c) = match card.glyph {
            Glyph::Running => (
                if lu.is_some_and(|lu| lu.cfg.tui.motion) {
                    spinner_glyph(crate::live::now_ms())
                } else {
                    card.glyph.mark()
                },
                hsla(t::WARN),
            ),
            Glyph::Ok => (card.glyph.mark(), hsla(t::SUCCESS)),
            Glyph::Error => (card.glyph.mark(), hsla(t::ERROR)),
            Glyph::Denied | Glyph::Interrupted => (card.glyph.mark(), dim()),
        };
        let (ic, kind_col) = tool_look(name);
        let tid0 = id.clone();
        let chev = if open {
            "chevron-down"
        } else {
            "chevron-right"
        };
        let gutter_w = card.gutter as f32 * 7.5 + 10.;
        let mut head = div()
            .id(("tool-head", idx))
            .debug_selector({
                let k = format!("tool-head-{id}");
                move || k.clone()
            })
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .py(px(7.))
            .cursor_pointer()
            .hover(|d| d.bg(super::with_alpha_white(0.03)))
            // one click on the header opens a clipped card, the next clips it again (the chevron is part
            // of the header; the body, where text is selected, is not)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_card(&tid0, cx)))
            .child(icon(ic, 16., kind_col))
            .child(
                div()
                    .flex_none()
                    .text_color(kind_col)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tool_title(name)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .gap(px(8.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .max_w_full()
                            .truncate()
                            .text_color(text())
                            .child(card.head.clone()),
                    )
                    .when(!card.sub.is_empty(), |d| {
                        d.child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(dim())
                                .child(card.sub.clone()),
                        )
                    }),
            )
            .children(card.badges.iter().map(|b| {
                div()
                    .flex_none()
                    .text_color(tone_fg(b.tone))
                    .child(b.text.clone())
            }))
            .child(div().flex_none().text_color(c).child(mark));
        if card.collapsible {
            let tid1 = id.clone();
            head = head.child(
                div()
                    .id(("tool-chev", idx))
                    .flex_none()
                    .cursor_pointer()
                    .text_color(dim())
                    .child(icon(chev, 14., dim()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_card(&tid1, cx);
                    })),
            );
        }
        let mut card_el = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(hsla(t::BORDER))
            .rounded(px(6.))
            .bg(super::with_alpha_black(0.6))
            .overflow_hidden()
            .child(head);
        if !card.rows.is_empty() {
            card_el = card_el.child(div().px(px(10.)).py(px(4.)).flex().flex_col().children(
                card.rows.iter().map(|r| {
                    let (fg, bg) = (tone_fg(r.tone), tone_bg(r.tone));
                    let line = if r.text.is_empty() {
                        div().child(" ")
                    } else if r.code {
                        div().child(syntax::styled(&r.text))
                    } else {
                        div().child(r.text.clone())
                    };
                    div()
                        .flex()
                        .text_color(fg)
                        .when_some(bg, |d, b| d.bg(b))
                        .when(card.gutter > 0, |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .w(px(gutter_w))
                                    .pr(px(8.))
                                    .text_right()
                                    .text_color(dim())
                                    .child(r.no.map(|n| n.to_string()).unwrap_or_default()),
                            )
                        })
                        .child(line.flex_1().min_w(px(0.)).whitespace_normal())
                }),
            ));
        }
        // the sub-agent this very call started (`subagent.started.tool_call_id`), if its pane exists
        let pane = lu.and_then(|lu| {
            lu.live
                .view
                .panes
                .subs
                .iter()
                .find(|a| a.tool_call_id.as_deref() == Some(id))
                .map(|a| a.id.clone())
        });
        if card.collapsible || pane.is_some() {
            let more = card.hidden;
            card_el = card_el.child(
                div()
                    .px(px(10.))
                    .pb(px(4.))
                    .flex()
                    .gap(px(14.))
                    .when(card.collapsible, |d| {
                        let key = id.clone();
                        d.child(
                            div()
                                .id(gpui::SharedString::from(format!("more-{id}")))
                                .debug_selector({
                                    let k = format!("more-{id}");
                                    move || k.clone()
                                })
                                .cursor_pointer()
                                .text_color(hsla(t::ACCENT))
                                .child(match (open, more) {
                                    (false, n) => {
                                        format!("+{n} line{}", if n == 1 { "" } else { "s" })
                                    }
                                    (true, 0) => "Show less".to_string(),
                                    (true, n) => format!("Show less (+{n} not shown)"),
                                })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_card(&key, cx)),
                                ),
                        )
                    })
                    .when_some(pane, |d, sub| {
                        d.child(
                            div()
                                .id(("tool-pane", idx))
                                .cursor_pointer()
                                .text_color(hsla(t::ACCENT))
                                .child("Open agent pane")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ts.sub = Some(sub.clone());
                                    this.ts.tab = super::ChatTab::Chat;
                                    cx.notify();
                                })),
                        )
                    }),
            );
        }
        card_el.into_any_element()
    }

    pub(super) fn welcome(&self) -> Div {
        let s = &self.snap;
        div()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .child(gpui::img(crate::brand::mark()).size(px(128.)))
            .child(super::tracked("SYNTHESIS", 16., 0.32, strong()))
            .child(div().text_color(dim()).child("Autonomous coding agent"))
            .child(
                div()
                    .mt(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .w_full()
                    .max_w(px(560.))
                    .px(px(16.))
                    .children(s.tips.iter().map(|tip| {
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().text_color(hsla(t::ACCENT)).child("\u{00B7}"))
                            .child(div().child(tip.clone()))
                    })),
            )
    }

    /// The live prompt: the text box, the queue count, the search line and the completion popup.
    pub(super) fn live_prompt(&self, cx: &mut Context<Self>) -> Div {
        let Some(lu) = &self.live else { return div() };
        let l = &lu.live;
        let mut col = div().flex().flex_col().flex_none();
        if let Some(c) = &lu.comp {
            col = col.child(
                div()
                    .debug_selector(|| "completion".to_string())
                    .mx(px(12.))
                    .mb(px(4.))
                    .p(px(4.))
                    .border_1()
                    .border_color(hsla(t::BORDER))
                    .rounded(px(layout::PANEL_RADIUS))
                    .bg(hsla(t::ELEVATED))
                    .children(c.items.iter().enumerate().map(|(i, (label, detail))| {
                        div()
                            .flex()
                            .gap(px(12.))
                            .px(px(8.))
                            .rounded(px(4.))
                            .when(i == c.sel, |d| d.bg(hsla(t::BORDER)))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(if i == c.sel { strong() } else { text() })
                                    .child(label.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(dim())
                                    .child(detail.clone()),
                            )
                    })),
            );
        }
        if let Some((needle, _)) = &lu.search {
            col = col.child(
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .text_color(dim())
                    .child(format!(
                        "(history search) `{needle}`  Ctrl+R older, Enter keeps, Esc leaves"
                    )),
            );
        }
        if !l.images.is_empty() {
            col = col.child(
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .flex()
                    .gap(px(6.))
                    .children(l.images.iter().map(|a| {
                        div()
                            .px(px(8.))
                            .rounded(px(10.))
                            .border_1()
                            .border_color(hsla(t::ACCENT))
                            .text_color(strong())
                            .child(format!("{} {:.1} MB", a.name, a.bytes as f64 / 1e6))
                    }))
                    .child(div().text_color(dim()).child("/images clear drops them")),
            );
        }
        // queued lines: one clipped row each, with "Send now" and remove; accepted sent-now lines wait below them
        let small = |id: String, label: &'static str| {
            div()
                .id(gpui::SharedString::from(id))
                .flex_none()
                .px(px(6.))
                .rounded(px(4.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .text_color(strong())
                .cursor_pointer()
                .child(label)
        };
        col = col
            .children(l.queue.iter().enumerate().map(|(i, q)| {
                let command = q.starts_with('/') || q.starts_with('!');
                div()
                    .debug_selector(move || format!("queued-{i}"))
                    .mx(px(12.))
                    .mb(px(2.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_color(dim())
                    .child(div().flex_none().child("queued"))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(clip(q, 200)))
                    .when(!command, |d| {
                        d.child(small(format!("send-now-{i}"), "Send now").on_click(
                            cx.listener(move |this, _, _, cx| this.queue_row(i, true, cx)),
                        ))
                    })
                    .child(
                        small(format!("unqueue-{i}"), "\u{d7}").on_click(
                            cx.listener(move |this, _, _, cx| this.queue_row(i, false, cx)),
                        ),
                    )
            }))
            .children(l.pending.iter().map(|(_, to, q)| {
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .flex()
                    .gap(px(8.))
                    .text_color(dim())
                    .child(div().flex_none().child(format!(
                        "sent now to @{}, delivered at the next step",
                        label(to)
                    )))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(clip(q, 200)))
            }));
        if l.busy && l.target != "main" {
            col = col.child(
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .text_color(strong())
                    .child(format!(
                        "to @{} (Ctrl+T cycles, Esc cancels the task)",
                        l.target
                    )),
            );
        }
        if let Some(to) = &l.send_to {
            col = col.child(
                div()
                    .mx(px(12.))
                    .mb(px(2.))
                    .text_color(strong())
                    .child(format!(
                        "to {} (Enter sends once, Esc or Ctrl+T clears)",
                        to.label
                    )),
            );
        }
        let busy = l.busy || l.starting;
        col.child(input_box(
            div()
                .relative()
                .flex_1()
                .min_w(px(0.))
                .child(NativeCaret(false))
                .child(Textarea::new(&lu.prompt).appearance(false).bordered(false))
                .child(NativeCaret(true))
                .child(block_cursor(lu.prompt.clone(), lu.blink.clone())),
            l.focus == crate::live::Focus::Chat,
            busy,
            Some(Box::new(cx.listener(|this, _, window, cx| {
                this.submit_prompt(false, window, cx)
            }))),
        ))
    }
}

/// The "Jump to latest" pill shows only when there is something to scroll (`max` px of scroll range) and the view
/// is more than a few px above the end (`at` px from the top); new output below the fold keeps it shown.
fn jump_visible(max: f32, at: f32) -> bool {
    const TOLERANCE: f32 = 4.;
    max > TOLERANCE && at < max - TOLERANCE
}

const SPINNER: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];
const VERBS: [&str; 8] = [
    "Thinking",
    "Reasoning",
    "Pondering",
    "Analyzing",
    "Synthesizing",
    "Working",
    "Considering",
    "Planning",
];

/// Braille spinner frame at `ms` (one frame per 80 ms).
pub(super) fn spinner_glyph(ms: i64) -> &'static str {
    SPINNER[(ms.max(0) / 80) as usize % SPINNER.len()]
}

/// The verb of the working line: a new one every 4 s of the task.
fn activity_verb(elapsed_s: u64) -> &'static str {
    VERBS[(elapsed_s / 4) as usize % VERBS.len()]
}

/// `45s`, `2m 05s`.
fn elapsed_label(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

/// `850`, `1.2k`, `12k`, `1.5M`.
fn token_label(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1e3),
        10_000..=999_999 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Blink state of the prompt's block caret: where the caret was when it last moved (or the text last
/// changed) and whether a repaint timer is already waiting.
#[derive(Clone, Default)]
pub struct Blink {
    seen: std::rc::Rc<std::cell::Cell<Option<(usize, usize, std::time::Instant)>>>,
    armed: std::rc::Rc<std::cell::Cell<bool>>,
}

const BLINK_MS: u128 = 530;

/// Whether the caret shows `elapsed_ms` after it last moved: on for 530 ms, off for 530 ms, ...
fn blink_on(elapsed_ms: u128) -> bool {
    (elapsed_ms / BLINK_MS).is_multiple_of(2)
}

/// Sets the theme's caret colour when the tree is built: the elements after it (in tree order) get that
/// colour, since the input reads it when its own `render` runs. Used as a pair around the prompt so only the
/// prompt loses the library's thin bar; every other input keeps it.
#[derive(IntoElement)]
struct NativeCaret(bool);

impl RenderOnce for NativeCaret {
    fn render(self, _: &mut gpui::Window, cx: &mut gpui::App) -> impl IntoElement {
        cx.global_mut::<gpui_component::Theme>().caret = if self.0 {
            hsla(t::PEARL)
        } else {
            // not fully transparent: the input's own view re-resolves an alpha-0 caret to its text colour
            // whenever it re-renders (blink, focus), which brought the thin bar back
            gpui::hsla(0., 0., 0., 0.001)
        };
        div().size_0()
    }
}

/// The prompt's caret: a block as high as the text's line box, one cell wide, blinking while the
/// prompt has the keys. The input's own thin bar is hidden by [`NativeCaret`].
fn block_cursor(
    prompt: gpui::Entity<gpui_component::input::TextareaState>,
    blink: Blink,
) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |_, (), window, cx| {
            let st = prompt.read(cx);
            let Some((cb, _)) = st.cursor_layout() else {
                return;
            };
            if !gpui::Focusable::focus_handle(st, cx).is_focused(window) {
                blink.seen.set(None);
                return;
            }
            let (cur, len) = (st.cursor(), st.value().len());
            let now = std::time::Instant::now();
            let since = match blink.seen.get() {
                Some((c, l, t0)) if (c, l) == (cur, len) => t0,
                _ => {
                    blink.seen.set(Some((cur, len, now)));
                    now
                }
            };
            let ms = now.duration_since(since).as_millis();
            if !blink.armed.replace(true) {
                let wait = (BLINK_MS - ms % BLINK_MS) as u64;
                let armed = blink.armed.clone();
                window
                    .spawn(cx, async move |cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(wait))
                            .await;
                        armed.set(false);
                        cx.update(|window, _| window.refresh()).ok();
                    })
                    .detach();
            }
            if !blink_on(ms) {
                return;
            }
            // a terminal block: one cell wide and as high as the text's whole line box
            let ts = window.text_style();
            let w = ts.font_size.to_pixels(window.rem_size()) * 0.6;
            window.paint_quad(gpui::fill(
                gpui::Bounds::new(cb.origin, gpui::size(w, cb.size.height)),
                hsla(t::PEARL),
            ));
        },
    )
    .absolute()
    .size_full()
}

/// `.inputbar`: 46px box (border + 44px form), margin 0 12px 12px, the `>` prompt mark, `content`, and the
/// 28px send button. The border takes the accent while the prompt has the keys.
pub fn input_box(
    content: impl IntoElement,
    focused: bool,
    busy: bool,
    send: Option<super::panels::OnClick>,
) -> Div {
    div()
        .debug_selector(|| "prompt".to_string())
        .flex_none()
        .mx(px(12.))
        .mb(px(12.))
        .min_h(px(46.))
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(12.))
        .border_1()
        // the focus frame is the chat panel's own border now; the box only keeps a quiet tint while focused
        .border_color(if focused {
            with_alpha(hsla(t::ACCENT), 0.35)
        } else {
            hsla(t::BORDER)
        })
        .rounded(px(layout::PANEL_RADIUS))
        .bg(hsla(t::SURFACE))
        .child(
            div()
                .text_color(strong())
                .child(if busy { "\u{2026}" } else { ">" }),
        )
        .child(content)
        .child(
            div()
                .id("send")
                .debug_selector(|| "send".to_string())
                .size(px(28.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .cursor_pointer()
                .when_some(send, |d, f| d.on_click(f))
                .child(icon("send", 14., hsla(t::ACCENT))),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_caret_blinks_530_ms_on_530_ms_off() {
        assert!(blink_on(0) && blink_on(529));
        assert!(!blink_on(530) && !blink_on(1059));
        assert!(blink_on(1060));
    }

    #[test]
    fn notices_alone_leave_the_welcome_up() {
        use super::super::driver::Row;
        let n = |t: &str| Item::Notice {
            warning: false,
            text: t.into(),
        };
        assert_eq!(notices_only(&[], &[]), Some(None));
        let items = [n("a"), n("b"), Item::User("hi".into())];
        let rows = [Row::Item(0), Row::Item(1)];
        assert_eq!(notices_only(&rows, &items), Some(Some((false, "b".into()))));
        assert_eq!(notices_only(&[Row::Item(0), Row::Item(2)], &items), None);
        assert_eq!(notices_only(&[Row::Activity], &items), None);
    }

    #[test]
    fn wrap_rows_counts_what_a_line_takes_on_screen() {
        assert_eq!(wrap_rows("a\r\nb\n\nc", 10), ["a", "b", "", "c"]);
        // a one-line file as long as ten rows is ten rows, not one
        assert_eq!(wrap_rows(&"x".repeat(100), 10).len(), 10);
        assert_eq!(wrap_rows("a\rb", 10), ["a", "b"]);
        assert!(wrap_rows("", 10).is_empty());
    }

    #[test]
    fn a_users_bang_line_is_not_repeated_above_its_card() {
        assert!(!visible(&Item::User("!ls -la".into()), false, false, false));
        assert!(visible(&Item::User("ls -la".into()), false, false, false));
    }

    #[test]
    fn the_toggle_names_what_it_does() {
        assert_eq!(toggle_label(true, 1), "Show more (1 more line)");
        assert_eq!(toggle_label(true, 7), "Show more (7 more lines)");
        assert_eq!(toggle_label(false, 7), "Show less");
    }

    #[test]
    fn the_jump_pill_shows_only_when_scrolled_up_from_a_longer_content() {
        // everything fits: nothing to scroll, no pill
        assert!(!jump_visible(0., 0.));
        // at the bottom (or within a few px of it)
        assert!(!jump_visible(500., 500.));
        assert!(!jump_visible(500., 497.));
        // scrolled up
        assert!(jump_visible(500., 200.));
        // new output arrived below the fold while scrolled up: the range grew, still scrolled up
        assert!(jump_visible(900., 200.));
    }

    #[test]
    fn the_working_line_reads_real_numbers() {
        assert_eq!(elapsed_label(45), "45s");
        assert_eq!(elapsed_label(125), "2m 05s");
        assert_eq!(token_label(850), "850");
        assert_eq!(token_label(1234), "1.2k");
        assert_eq!(token_label(12_345), "12k");
        assert_eq!(activity_verb(0), "Thinking");
        assert_eq!(activity_verb(4), "Reasoning");
        assert_ne!(spinner_glyph(0), spinner_glyph(80));
        assert_eq!(spinner_glyph(0), spinner_glyph(800));
    }

    #[test]
    fn the_quiet_chat_hides_what_a_person_does_not_need() {
        let thinking = Item::Thinking("hm".into());
        assert!(!visible(&thinking, false, false, false));
        assert!(visible(&thinking, true, false, false));
        let ap = |state| Item::Approval {
            id: "a".into(),
            tool_call_id: "t".into(),
            prompt: "p".into(),
            state,
        };
        assert!(!visible(&ap(ApprovalState::Allowed), false, false, false));
        assert!(visible(&ap(ApprovalState::Allowed), false, true, false));
        assert!(visible(&ap(ApprovalState::Pending), false, false, false));
        assert!(visible(&ap(ApprovalState::Denied), false, false, false));
        assert!(!visible(
            &Item::Assistant {
                text: " ".into(),
                streaming: false
            },
            false,
            false,
            false
        ));
        // /focus keeps the conversation and drops the work
        assert!(visible(&Item::User("x".into()), false, false, true));
        assert!(!visible(&Item::Thinking("hm".into()), true, false, true));
        // internal rows: verbose only, and never in /focus
        let internal = Item::Internal("memory: 1 note(s) retrieved (12 tokens)".into());
        assert!(!visible(&internal, false, false, false));
        assert!(visible(&internal, false, true, false));
        assert!(!visible(&internal, false, true, true));
        // what needs the person stays in the quiet chat and in /focus
        let warn = Item::Notice {
            warning: true,
            text: "budget exceeded: turns 5 / 5".into(),
        };
        assert!(visible(&warn, false, false, false) && visible(&warn, false, false, true));
    }

    /// The row list of one conversation under the quiet-chat and /focus switches.
    #[test]
    fn the_row_list_of_a_conversation_with_every_new_row_kind() {
        use synthesis_view::convo::Convo;
        let ev = |t: &str, p: serde_json::Value| -> synthesis_events::Envelope {
            serde_json::from_value(
                serde_json::json!({"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z",
                "session_id":"s","task_id":null,"turn_id":null,"type":t,"payload":p}),
            )
            .unwrap()
        };
        let addr = serde_json::json!({"workspace_key":"k","workspace_label":"api","session_id":"s2","session_name":"backend","instance":"i"});
        let mut c = Convo::default();
        for e in [
            ev(
                "user.message",
                serde_json::json!({"text":"ship it","images":[]}),
            ),
            ev(
                "memory.retrieved",
                serde_json::json!({"ids":["m"],"tokens":9,"candidates":1}),
            ),
            ev(
                "hook.ran",
                serde_json::json!({"event":"Stop","hook_name":"lint","exit_code":0,"outcome":"success","duration_ms":1}),
            ),
            ev(
                "hook.ran",
                serde_json::json!({"event":"Stop","hook_name":"lint","exit_code":1,"outcome":"error","duration_ms":1}),
            ),
            ev(
                "browser.login_required",
                serde_json::json!({"origin":"https://x.test","profile":"p"}),
            ),
            ev(
                "peer.delegation.accepted",
                serde_json::json!({"delegation_id":"d","request_id":"r","from":addr,"mode":"plan","cap":"plan"}),
            ),
            ev(
                "peer.reply",
                serde_json::json!({"request_id":"r","status":"answered","text":"42","bytes":2,"duration_ms":1}),
            ),
            ev(
                "budget.exceeded",
                serde_json::json!({"budget":"turns","limit":5.0,"used":5.0}),
            ),
            ev(
                "verification.completed",
                serde_json::json!({"report_id":"r","status":"unverified","attempt":1,"summary":"cargo test: not run"}),
            ),
            ev(
                "task.ended",
                serde_json::json!({"status":"unverified","reason":"no_check_ran","usage":{"input_tokens":0,"output_tokens":0},"changed_files":[],"artifacts_root":"/x"}),
            ),
        ] {
            c.apply(&e);
        }
        let rows = |verbose, focus| -> Vec<String> {
            c.items
                .iter()
                .filter(|i| visible(i, false, verbose, focus))
                .map(|i| match i {
                    Item::User(s) => format!("user: {s}"),
                    Item::Notice { warning, text } => {
                        format!("{}: {text}", if *warning { "warn" } else { "note" })
                    }
                    Item::Internal(t) => format!("internal: {t}"),
                    Item::TaskEnd { text, checks, .. } => format!("end: {text} [{checks}]"),
                    o => format!("{o:?}"),
                })
                .collect()
        };
        let quiet = [
            "user: ship it",
            "warn: hook lint (Stop): error",
            "warn: log in to https://x.test in the browser window, then confirm",
            "note: working for peer backend (plan mode)",
            "note: peer reply: answered  42",
            "warn: budget exceeded: turns 5 / 5",
            "end: task unverified: no_check_ran [cargo test: not run]",
        ];
        assert_eq!(rows(false, false), quiet);
        let verbose = rows(true, false);
        assert_eq!(verbose.len(), quiet.len() + 2);
        assert_eq!(
            verbose[1],
            "internal: memory: 1 note(s) retrieved (9 tokens)"
        );
        assert_eq!(verbose[2], "internal: hook lint (Stop): success");
        // /focus: the conversation and what needs the person; the same with verbose
        assert_eq!(rows(false, true), quiet);
        assert_eq!(rows(true, true), quiet);
    }
}
