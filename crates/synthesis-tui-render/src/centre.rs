//! Centre tabs of the HTML design (chat, diff, terminal, browser, file explorer): the strip, the
//! file tree with `git status` letters, the diff, the shell commands and the browser calls (the shell commands of the session). Pure; the
//! host reads git and hands the result over, an absent value is "n/a" or an empty state.

use crate::dash::{DashState, col_bad, col_muted, col_ok, col_text, col_warn};
use crate::theme::{fit, glyphs, paint, t};
use synthesis_tui_base::text::width;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Centre {
    #[default]
    Chat,
    Diff,
    Terminal,
    Browser,
    Files,
}

impl Centre {
    pub const ALL: [Centre; 5] = [
        Centre::Chat,
        Centre::Diff,
        Centre::Terminal,
        Centre::Browser,
        Centre::Files,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Centre::Chat => "chat",
            Centre::Diff => "diff",
            Centre::Terminal => "terminal",
            Centre::Browser => "browser",
            Centre::Files => "file explorer",
        }
    }

    pub fn parse(s: &str) -> Option<Centre> {
        Centre::ALL
            .into_iter()
            .find(|c| c.label() == s || (*c == Centre::Files && s == "files"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub depth: u16,
    pub name: String,
    pub dir: bool,
    /// `git status --porcelain` letter: `M`, `A`, `D`, `R`, `?`.
    pub status: Option<char>,
}

/// What the host read from the working tree; `loaded == false` and no `note`: still reading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectData {
    pub loaded: bool,
    /// Why there is nothing to show (`not a git repository`, `git not found`).
    pub note: Option<String>,
    pub rows: Vec<TreeRow>,
    /// Unified diff of the tracked changes, one line per entry.
    pub diff: Vec<String>,
    /// The tree or the diff was cut at the host's bound.
    pub truncated: bool,
}

/// The tab strip row and the columns `[x0, x1)` of each tab.
pub fn strip(active: Centre, w: i64) -> (String, Vec<(i64, i64, Centre)>) {
    let th = t();
    let mut x = 1;
    let mut hits = vec![];
    let items: Vec<String> = Centre::ALL
        .iter()
        .map(|c| {
            let item = if *c == active {
                format!(
                    "{}{}",
                    paint(th.pearl, glyphs().focus),
                    paint(th.strong, c.label())
                )
            } else {
                format!(" {}", paint(th.label, c.label()))
            };
            hits.push((x, x + width(&item) as i64, *c));
            x += width(&item) as i64 + 3;
            item
        })
        .collect();
    let row = format!(" {}", items.join("   "));
    (fit(&row, w.max(0) as usize), hits)
}

fn status_colour(c: char) -> crate::theme::Rgb {
    match c {
        'A' => col_ok(),
        'D' => col_bad(),
        '?' => col_muted(),
        _ => col_warn(),
    }
}

fn empty(p: &ProjectData) -> Option<String> {
    if let Some(n) = &p.note {
        Some(format!("n/a: {n}"))
    } else if !p.loaded {
        Some("reading the working tree ...".into())
    } else {
        None
    }
}

fn pad_to(lines: Vec<String>, w: i64, h: i64) -> Vec<String> {
    let mut v: Vec<String> = lines
        .into_iter()
        .take(h.max(0) as usize)
        .map(|l| fit(&l, w.max(0) as usize))
        .collect();
    v.resize(h.max(0) as usize, String::new());
    v
}

pub fn project(p: &ProjectData, w: i64, h: i64) -> Vec<String> {
    if let Some(n) = empty(p) {
        return pad_to(vec![format!(" {}", paint(col_muted(), &n))], w, h);
    }
    let mut out: Vec<String> = p
        .rows
        .iter()
        .map(|r| {
            let indent = " ".repeat(2 * r.depth as usize + 1);
            let name = if r.dir {
                format!("{}/", r.name)
            } else {
                r.name.clone()
            };
            let left = format!("{indent}{}", paint(col_text(), &name));
            match r.status {
                Some(c) => {
                    let gap = (w - 2 - width(&left) as i64).max(1);
                    format!(
                        "{left}{}{}",
                        " ".repeat(gap as usize),
                        paint(status_colour(c), &c.to_string())
                    )
                }
                None => left,
            }
        })
        .collect();
    if p.rows.is_empty() {
        out.push(format!(" {}", paint(col_muted(), "no files")));
    }
    if p.truncated && out.len() as i64 >= h {
        out.truncate((h - 1).max(0) as usize);
        out.push(format!(
            " {}",
            paint(col_muted(), "... more files not listed")
        ));
    }
    pad_to(out, w, h)
}

pub fn diff(p: &ProjectData, w: i64, h: i64) -> Vec<String> {
    if let Some(n) = empty(p) {
        return pad_to(vec![format!(" {}", paint(col_muted(), &n))], w, h);
    }
    let th = t();
    let mut out: Vec<String> = p
        .diff
        .iter()
        .map(|l| {
            let c = if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") {
                th.strong
            } else if l.starts_with('+') {
                col_ok()
            } else if l.starts_with('-') {
                col_bad()
            } else if l.starts_with("@@") {
                th.info
            } else {
                col_muted()
            };
            format!(" {}", paint(c, l))
        })
        .collect();
    if out.is_empty() {
        out.push(format!(" {}", paint(col_muted(), "no tracked changes")));
    } else if p.truncated && out.len() as i64 >= h {
        out.truncate((h - 1).max(0) as usize);
        out.push(format!(" {}", paint(col_muted(), "... diff cut")));
    }
    pad_to(out, w, h)
}

/// Browser tool calls of the session, with how each ended; no browser use is an empty state.
pub fn browser(s: &DashState, w: i64, h: i64, now: i64) -> Vec<String> {
    calls(
        s,
        w,
        h,
        now,
        |n| n.starts_with("browser"),
        "no browser activity yet",
    )
}

/// The shell commands the session ran, newest last, with how each ended. Output text is not kept
/// by the event model, so it is not shown.
pub fn terminal(s: &DashState, w: i64, h: i64, now: i64) -> Vec<String> {
    calls(s, w, h, now, |n| n == "Bash", "no shell commands yet")
}

fn calls(
    s: &DashState,
    w: i64,
    h: i64,
    now: i64,
    pick: impl Fn(&str) -> bool,
    none: &str,
) -> Vec<String> {
    let runs: Vec<String> = s
        .session
        .tools
        .items
        .iter()
        .filter(|t| pick(&t.name))
        .map(|t| {
            let (mark, c) = match (t.end, t.is_error) {
                (None, _) => ("…", col_warn()),
                (Some(_), true) => (glyphs().fail, col_bad()),
                (Some(_), false) => (glyphs().ok, col_ok()),
            };
            let dur = t
                .start
                .map(|a| crate::util::fmt_dur_ms((t.end.unwrap_or(now) - a).max(0)))
                .unwrap_or_else(|| "n/a".into());
            let left = format!(" {} {}", paint(c, mark), paint(col_text(), &t.summary));
            let gap = (w - 1 - width(&left) as i64 - dur.len() as i64).max(1);
            format!(
                "{left}{}{}",
                " ".repeat(gap as usize),
                paint(col_muted(), &dur)
            )
        })
        .collect();
    if runs.is_empty() {
        return pad_to(vec![format!(" {}", paint(col_muted(), none))], w, h);
    }
    let skip = runs.len().saturating_sub(h.max(0) as usize);
    pad_to(runs.into_iter().skip(skip).collect(), w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_tui_base::text::strip_ansi;

    #[test]
    fn strip_hits_cover_each_tab_once() {
        let (row, hits) = strip(Centre::Diff, 60);
        assert_eq!(hits.len(), 5);
        assert!(strip_ansi(&row).contains("file explorer"));
        assert!(hits.windows(2).all(|w| w[0].1 <= w[1].0));
        assert_eq!(Centre::parse("diff"), Some(Centre::Diff));
        assert_eq!(Centre::parse("nope"), None);
    }

    #[test]
    fn unread_or_missing_tree_says_so_and_data_shows_letters() {
        let p = ProjectData::default();
        assert!(strip_ansi(&project(&p, 40, 3)[0]).contains("reading"));
        let n = ProjectData {
            loaded: true,
            note: Some("not a git repository".into()),
            ..Default::default()
        };
        assert!(strip_ansi(&diff(&n, 40, 3)[0]).contains("n/a: not a git repository"));
        let d = ProjectData {
            loaded: true,
            rows: vec![TreeRow {
                depth: 1,
                name: "a.rs".into(),
                dir: false,
                status: Some('M'),
            }],
            diff: vec!["+x".into()],
            ..Default::default()
        };
        let l = strip_ansi(&project(&d, 20, 2)[0]);
        assert!(l.contains("a.rs") && l.trim_end().ends_with('M'));
        assert_eq!(project(&d, 20, 2).len(), 2);
        assert!(strip_ansi(&diff(&d, 20, 2)[0]).contains("+x"));
    }
}
