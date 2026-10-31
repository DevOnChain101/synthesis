//! Agent Topology page data, after `STATE.topology1` / `renderTopology` of the owner's TUI HTML
//! (`~/Vibe/synthesis-cli-ui/index.html`): eight group cards around a Synthesis core, same
//! positions, group colours and links. Content comes from the live session; the link geometry is
//! the HTML's `drawLinks` ported as pure functions (the page only paints it).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pos {
    User,
    Hooks,
    Guards,
    Sub,
    Skills,
    Mcp,
    Tools,
    Mem,
}

/// Group colours of the HTML `:root` (`--blue`, `--amber`, ...).
pub const BLUE: u32 = 0x6EA8FF;
pub const AMBER: u32 = 0xE0B252;
pub const RED: u32 = 0xE5636B;
pub const PURPLE: u32 = 0xB794F6;
pub const GREEN: u32 = 0x4ADE80;
pub const ORANGE: u32 = 0xF59E42;
pub const TEAL: u32 = 0x4FD1C5;
pub const PINK: u32 = 0xF472B6;

#[derive(Debug, Clone)]
pub struct Card {
    pub pos: Pos,
    pub title: &'static str,
    pub color: u32,
    pub lines: Vec<String>,
}

/// What the live session knows. Empty fields give an honest "none yet" line, never invented rows.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub project: String,
    pub session: String,
    pub prompt: String,
    pub sandbox: String,
    pub network: String,
    pub mode: String,
    pub hooks: Vec<String>,
    pub subagents: Vec<String>,
    pub mcp: Vec<String>,
    pub tools: Vec<String>,
    pub memory: Option<bool>,
}

fn or_none(v: &[String], none: &str) -> Vec<String> {
    if v.is_empty() {
        vec![none.into()]
    } else {
        v.iter().take(3).cloned().collect()
    }
}

pub fn cards(s: &Snapshot) -> Vec<Card> {
    let c = |pos, title, color, lines| Card {
        pos,
        title,
        color,
        lines,
    };
    vec![
        c(
            Pos::User,
            "USER & PROMPT",
            BLUE,
            vec![
                s.project.clone(),
                s.session.clone(),
                if s.prompt.is_empty() {
                    "no prompt yet".into()
                } else {
                    s.prompt.chars().take(28).collect()
                },
            ],
        ),
        c(
            Pos::Hooks,
            "HOOKS",
            AMBER,
            or_none(&s.hooks, "none ran yet"),
        ),
        c(
            Pos::Guards,
            "GUARDS",
            RED,
            vec![
                format!("sandbox · {}", s.sandbox),
                format!("network · {}", s.network),
                format!("approval · {}", s.mode),
            ],
        ),
        c(
            Pos::Sub,
            "SUBAGENTS",
            PURPLE,
            or_none(&s.subagents, "none started"),
        ),
        c(
            Pos::Skills,
            "SKILLS",
            GREEN,
            vec!["not listed in this build yet".into()],
        ),
        c(Pos::Mcp, "MCP", ORANGE, or_none(&s.mcp, "none configured")),
        c(
            Pos::Tools,
            "TOOLS",
            TEAL,
            or_none(&s.tools, "none used yet"),
        ),
        c(
            Pos::Mem,
            "MEMORY",
            PINK,
            vec![match s.memory {
                Some(true) => "long-term memory on".into(),
                Some(false) => "long-term memory off".into(),
                None => "no session".into(),
            }],
        ),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub fn cx(&self) -> f32 {
        (self.l + self.r) / 2.0
    }
    pub fn cy(&self) -> f32 {
        (self.t + self.b) / 2.0
    }
}

type P = (f32, f32);

fn quad(a: P, c: P, b: P, out: &mut Vec<P>) {
    for i in 1..=6 {
        let t = i as f32 / 6.0;
        let u = 1.0 - t;
        out.push((
            u * u * a.0 + 2.0 * u * t * c.0 + t * t * b.0,
            u * u * a.1 + 2.0 * u * t * c.1 + t * t * b.1,
        ));
    }
}

/// `vertical` of drawLinks: down/up, across at mid height, down/up again, corners rounded.
fn vertical(s: P, e: P) -> Vec<P> {
    if (s.0 - e.0).abs() < 1.0 {
        return vec![s, e];
    }
    let my = (s.1 + e.1) / 2.0;
    let (dx, dy) = ((e.0 - s.0).signum(), (e.1 - s.1).signum());
    let r = 10f32
        .min((e.0 - s.0).abs() / 2.0)
        .min((e.1 - s.1).abs() / 4.0);
    let mut v = vec![s, (s.0, my - dy * r)];
    quad(
        *v.last().unwrap_or(&s),
        (s.0, my),
        (s.0 + dx * r, my),
        &mut v,
    );
    v.push((e.0 - dx * r, my));
    let from = (e.0 - dx * r, my);
    quad(from, (e.0, my), (e.0, my + dy * r), &mut v);
    v.push(e);
    v
}

/// `horizontal` of drawLinks.
fn horizontal(s: P, e: P) -> Vec<P> {
    if (s.1 - e.1).abs() < 1.0 {
        return vec![s, e];
    }
    let mx = (s.0 + e.0) / 2.0;
    let (dx, dy) = ((e.0 - s.0).signum(), (e.1 - s.1).signum());
    let r = 10f32
        .min((e.0 - s.0).abs() / 4.0)
        .min((e.1 - s.1).abs() / 2.0);
    let mut v = vec![s, (mx - dx * r, s.1)];
    quad(
        *v.last().unwrap_or(&s),
        (mx, s.1),
        (mx, s.1 + dy * r),
        &mut v,
    );
    v.push((mx, e.1 - dy * r));
    let from = (mx, e.1 - dy * r);
    quad(from, (mx, e.1), (mx + dx * r, e.1), &mut v);
    v.push(e);
    v
}

/// Link polylines from each card to the core, exactly the attachment points of the HTML:
/// user/hooks/guards enter the core's top edge (left, centre, right), skills and subagents its
/// sides, mcp/tools/memory its bottom edge.
pub fn links(rect: impl Fn(Pos) -> Rect, core: Rect) -> Vec<Vec<P>> {
    let off = (core.r - core.l) * 0.24;
    let (lx, rx) = (core.cx() - off, core.cx() + off);
    let (u, h, g) = (rect(Pos::User), rect(Pos::Hooks), rect(Pos::Guards));
    let (sub, sk) = (rect(Pos::Sub), rect(Pos::Skills));
    let (m, t, me) = (rect(Pos::Mcp), rect(Pos::Tools), rect(Pos::Mem));
    vec![
        vertical((u.cx(), u.b), (lx, core.t)),
        vertical((h.cx(), h.b), (core.cx(), core.t)),
        vertical((g.cx(), g.b), (rx, core.t)),
        horizontal((sub.r, sub.cy()), (core.l, core.cy())),
        horizontal((sk.l, sk.cy()), (core.r, core.cy())),
        vertical((m.cx(), m.t), (lx, core.b)),
        vertical((t.cx(), t.t), (core.cx(), core.b)),
        vertical((me.cx(), me.t), (rx, core.b)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_cards_in_the_html_order_with_honest_empty_lines() {
        let c = cards(&Snapshot::default());
        let titles: Vec<_> = c.iter().map(|c| c.title).collect();
        assert_eq!(
            titles,
            [
                "USER & PROMPT",
                "HOOKS",
                "GUARDS",
                "SUBAGENTS",
                "SKILLS",
                "MCP",
                "TOOLS",
                "MEMORY"
            ]
        );
        assert_eq!(c[1].lines, ["none ran yet"]);
        assert_eq!(c[5].lines, ["none configured"]);
    }

    #[test]
    fn links_join_every_card_to_the_core_edges() {
        let core = Rect {
            l: 300.0,
            t: 160.0,
            r: 500.0,
            b: 300.0,
        };
        let rect = |p: Pos| match p {
            Pos::User => Rect {
                l: 0.0,
                t: 0.0,
                r: 200.0,
                b: 100.0,
            },
            Pos::Hooks => Rect {
                l: 300.0,
                t: 0.0,
                r: 500.0,
                b: 100.0,
            },
            Pos::Guards => Rect {
                l: 600.0,
                t: 0.0,
                r: 800.0,
                b: 100.0,
            },
            Pos::Sub => Rect {
                l: 0.0,
                t: 160.0,
                r: 200.0,
                b: 300.0,
            },
            Pos::Skills => Rect {
                l: 600.0,
                t: 160.0,
                r: 800.0,
                b: 300.0,
            },
            Pos::Mcp => Rect {
                l: 0.0,
                t: 360.0,
                r: 200.0,
                b: 460.0,
            },
            Pos::Tools => Rect {
                l: 300.0,
                t: 360.0,
                r: 500.0,
                b: 460.0,
            },
            Pos::Mem => Rect {
                l: 600.0,
                t: 360.0,
                r: 800.0,
                b: 460.0,
            },
        };
        let l = links(rect, core);
        assert_eq!(l.len(), 8);
        // hooks sits right above the core centre: a straight vertical line
        assert_eq!(l[1], vec![(400.0, 100.0), (400.0, 160.0)]);
        // user enters the top edge left of the centre (24% of the core width)
        assert_eq!(*l[0].last().unwrap(), (400.0 - 48.0, 160.0));
        // subagents enter the left edge at the core's vertical centre, straight
        assert_eq!(l[3], vec![(200.0, 230.0), (300.0, 230.0)]);
    }
}
