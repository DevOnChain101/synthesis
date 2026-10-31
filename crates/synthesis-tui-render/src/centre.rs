//! Centre tabs of the HTML design (chat, diff, terminal, browser, file explorer): the strip, the
//! file tree with `git status` letters, the diff, the shell commands and the browser calls (the shell commands of the session). Pure; the
//! host reads git and hands the result over, an absent value is "n/a" or an empty state.

use crate::dash::{DashState, col_bad, col_muted, col_ok, col_text, col_warn};
use crate::theme::{fit, glyphs, icon, paint, t};
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

struct FileDiff<'a> {
    path: &'a str,
    add: usize,
    del: usize,
    /// Hunk headers and body lines, header lines (`index`, `---`, `+++`) left out.
    body: Vec<&'a str>,
}

/// The unified diff split per file (`diff --git a/x b/x` starts a file).
fn files_of(diff: &[String]) -> Vec<FileDiff<'_>> {
    let mut out: Vec<FileDiff> = vec![];
    let mut in_hunk = false;
    for l in diff {
        if let Some(rest) = l.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b);
            out.push(FileDiff {
                path,
                add: 0,
                del: 0,
                body: vec![],
            });
            in_hunk = false;
        } else if let Some(f) = out.last_mut() {
            if l.starts_with("@@") {
                in_hunk = true;
            }
            if !in_hunk {
                continue;
            }
            match l.as_bytes().first() {
                Some(b'+') => f.add += 1,
                Some(b'-') => f.del += 1,
                _ => {}
            }
            f.body.push(l);
        }
    }
    out
}

/// `(old, new)` first line numbers of a hunk header `@@ -12,6 +12,28 @@ ...`.
fn hunk_start(h: &str) -> (i64, i64) {
    let num = |t: &str| {
        t.trim_start_matches(['-', '+'])
            .split(',')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    let mut it = h.split_whitespace().skip(1);
    (num(it.next().unwrap_or("")), num(it.next().unwrap_or("")))
}

/// One bordered card per changed file (the HTML `.dfile`): the path with `+n -m`, the hunk header,
/// then the lines with old and new line numbers.
fn diff_cards(diff: &[String], w: i64) -> Vec<String> {
    let th = t();
    let g = glyphs();
    let edge = |c: &str| paint(th.border, c);
    let inner = (w - 4).max(8) as usize;
    let mut out = vec![];
    for f in files_of(diff) {
        let meta = format!(
            "{} {}",
            paint(col_ok(), &format!("{}{}", g.add, f.add)),
            paint(col_bad(), &format!("{}{}", g.del, f.del))
        );
        let left_w = inner.saturating_sub(width(&meta) + 1);
        let left = format!(
            "{} {}",
            paint(th.label, icon("file")),
            paint(
                th.strong,
                &crate::util::short(f.path, left_w.saturating_sub(2))
            )
        );
        out.push(format!(
            "{}{}",
            edge(&format!(
                "{}{}",
                g.box_tl,
                g.box_h.repeat(w.max(4) as usize - 2)
            )),
            edge(g.box_tr)
        ));
        out.push(format!(
            "{} {} {meta} {}",
            edge(g.box_v),
            fit(&left, left_w),
            edge(g.box_v)
        ));
        let (mut o, mut n) = (0, 0);
        for l in &f.body {
            let text = if l.starts_with("@@") {
                (o, n) = hunk_start(l);
                paint(th.info, l)
            } else {
                let (sign, c) = match l.as_bytes().first() {
                    Some(b'+') => ('+', Some(col_ok())),
                    Some(b'-') => ('-', Some(col_bad())),
                    _ => (' ', None),
                };
                let (so, sn) = match sign {
                    '+' => (String::new(), n.to_string()),
                    '-' => (o.to_string(), String::new()),
                    _ => (o.to_string(), n.to_string()),
                };
                if sign != '+' {
                    o += 1;
                }
                if sign != '-' {
                    n += 1;
                }
                let body = l.get(1..).unwrap_or("");
                format!(
                    "{} {} {} {}",
                    paint(col_muted(), &format!("{so:>4}")),
                    paint(col_muted(), &format!("{sn:>4}")),
                    c.map_or(" ".into(), |c| paint(c, &sign.to_string())),
                    c.map_or(paint(col_text(), body), |c| paint(c, body))
                )
            };
            out.push(format!(
                "{} {} {}",
                edge(g.box_v),
                fit(&text, inner),
                edge(g.box_v)
            ));
        }
        out.push(edge(&format!(
            "{}{}{}",
            g.box_bl,
            g.box_h.repeat(w.max(4) as usize - 2),
            g.box_br
        )));
    }
    out
}

pub fn diff(p: &ProjectData, w: i64, h: i64) -> Vec<String> {
    if let Some(n) = empty(p) {
        return pad_to(vec![format!(" {}", paint(col_muted(), &n))], w, h);
    }
    let mut out = diff_cards(&p.diff, w);
    if out.is_empty() {
        out.push(format!(" {}", paint(col_muted(), "no tracked changes")));
    } else if p.truncated && out.len() as i64 >= h {
        out.truncate((h - 1).max(0) as usize);
        out.push(format!(" {}", paint(col_muted(), "... diff cut")));
    }
    pad_to(out, w, h)
}

/// File explorer: the tree on the left, the changes of the working tree on the right, side by side
/// (the HTML `.fx`). Narrow centres keep the tree alone.
pub fn files(p: &ProjectData, w: i64, h: i64) -> Vec<String> {
    if w < 60 || empty(p).is_some() {
        return project(p, w, h);
    }
    let tw = (w / 3).clamp(20, 30);
    let (left, right) = (project(p, tw, h), diff(p, w - tw - 1, h));
    let rule = paint(t().border, glyphs().box_v);
    left.into_iter()
        .zip(right)
        .map(|(l, r)| format!("{}{rule}{r}", fit(&l, tw as usize)))
        .collect()
}

/// Browser tool calls of the session as a request list (the HTML `.rows`): a status dot, the
/// action, what it was applied to, how it ended. No browser use is an empty state.
pub fn browser(s: &DashState, w: i64, h: i64, now: i64) -> Vec<String> {
    let mut v = calls(
        s,
        w,
        h.saturating_sub(2),
        now,
        |n| n.starts_with("browser"),
        "no browser activity yet",
        false,
    );
    let (total, failed) = s
        .session
        .tools
        .items
        .iter()
        .filter(|t| t.name.starts_with("browser"))
        .fold((0, 0), |(a, b), t| (a + 1, b + usize::from(t.is_error)));
    if total > 0 && h > 2 {
        v.push(String::new());
        v.push(format!(
            " {}",
            paint(col_muted(), &format!("{total} calls · {failed} failed"))
        ));
    }
    pad_to(v, w, h)
}

/// The shell commands the session ran, newest last, each with the start of its output and how it
/// ended (the HTML terminal pane: a `>` prompt line per command).
pub fn terminal(s: &DashState, w: i64, h: i64, now: i64) -> Vec<String> {
    calls(s, w, h, now, |n| n == "Bash", "no shell commands yet", true)
}

fn calls(
    s: &DashState,
    w: i64,
    h: i64,
    now: i64,
    pick: impl Fn(&str) -> bool,
    none: &str,
    output: bool,
) -> Vec<String> {
    let mut runs: Vec<String> = vec![];
    for t in s.session.tools.items.iter().filter(|t| pick(&t.name)) {
        let (mark, c) = match (t.end, t.is_error) {
            (None, _) => ("…", col_warn()),
            (Some(_), true) => (glyphs().fail, col_bad()),
            (Some(_), false) => (glyphs().ok, col_ok()),
        };
        let dur = t
            .start
            .map(|a| crate::util::fmt_dur_ms((t.end.unwrap_or(now) - a).max(0)))
            .unwrap_or_else(|| "n/a".into());
        let lead = if output {
            paint(t_strong(), ">")
        } else {
            paint(c, mark)
        };
        let left = format!(" {lead} {}", paint(col_text(), &t.summary));
        let tail = if output {
            format!("{} {dur}", paint(c, mark))
        } else {
            let verb = t.name.trim_start_matches("browser").trim_start_matches('_');
            format!("{} {dur}", if verb.is_empty() { "call" } else { verb })
        };
        let gap = (w - 1 - width(&left) as i64 - width(&tail) as i64).max(1);
        runs.push(format!(
            "{left}{}{}",
            " ".repeat(gap as usize),
            paint(col_muted(), &tail)
        ));
        if output {
            runs.extend(
                t.output
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(4)
                    .map(|l| format!("   {}", paint(col_muted(), l.trim_end()))),
            );
        }
    }
    if runs.is_empty() {
        return pad_to(vec![format!(" {}", paint(col_muted(), none))], w, h);
    }
    let skip = runs.len().saturating_sub(h.max(0) as usize);
    pad_to(runs.into_iter().skip(skip).collect(), w, h)
}

fn t_strong() -> crate::theme::Rgb {
    t().strong
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
            diff: vec![
                "diff --git a/a.rs b/a.rs".into(),
                "@@ -1 +1 @@".into(),
                "+x".into(),
            ],
            ..Default::default()
        };
        let l = strip_ansi(&project(&d, 20, 2)[0]);
        assert!(l.contains("a.rs") && l.trim_end().ends_with('M'));
        assert_eq!(project(&d, 20, 2).len(), 2);
        let card = diff(&d, 24, 5)
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            card.contains("a.rs") && card.contains("+1") && card.contains("+ x"),
            "{card}"
        );
    }

    #[test]
    fn file_explorer_keeps_the_rule_in_one_column_on_every_row() {
        let d = ProjectData {
            loaded: true,
            rows: vec![TreeRow {
                depth: 0,
                name: "a.rs".into(),
                dir: false,
                status: Some('M'),
            }],
            diff: vec![
                "diff --git a/a.rs b/a.rs".into(),
                "@@ -1 +1 @@".into(),
                "+x".into(),
            ],
            ..Default::default()
        };
        let rows = files(&d, 70, 8);
        assert_eq!(rows.len(), 8);
        for r in &rows {
            assert_eq!(strip_ansi(r).chars().nth(23), Some('│'), "{r:?}");
        }
    }
}
