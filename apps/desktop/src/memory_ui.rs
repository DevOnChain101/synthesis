//! `/memory list|search|add|show|edit|forget|review` (docs/specs/memory.md, `synt memory`). Memory is later
//! fed to the model, so everything that changes it is confirmed in the dock first (the dialog is not
//! answerable before the person engages it, and Esc is "no"), the text a note will carry is shown, a secret-shaped
//! note is refused, and nothing a model wrote is accepted by itself. In a folder that is not trusted (or
//! where memory is off) only the user scope is visible and writable. Plain Rust, no gpui; [`Live`] calls it.

use std::path::{Path, PathBuf};

use synthesis_app::memcmd::{Repo, disabled_here, enabled, markdown, repo_of};
use synthesis_config::{Env, is_secret_env_name};
use synthesis_harness::sanitize_for_display as san;
use synthesis_memory::{Filter, Memory, NewMemory, Store, Trust, scan};

use crate::live::{Dialog, DialogKind, Live, PickerKind};

const USAGE: &str = "usage: /memory [list | search QUERY | show ID [full] | add [--user] TEXT | edit ID TEXT | forget ID | review]";

/// Lines of one record before "N more lines" (`show ID full` expands).
const CLAMP: usize = 5;

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    List,
    Search(String),
    Show { id: String, full: bool },
    Add { text: String, user: bool },
    Edit { id: String, text: String },
    Forget(String),
    Review,
}

/// `None`: no argument (the instruction-files report).
pub fn parse(arg: &str) -> Result<Option<Cmd>, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(None);
    }
    let (sub, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    let rest = rest.trim();
    let need = |what: &str| {
        (!rest.is_empty())
            .then_some(rest)
            .ok_or(format!("/memory {sub}: {what}\n{USAGE}"))
    };
    Ok(Some(match sub {
        "list" => Cmd::List,
        "review" => Cmd::Review,
        "search" => Cmd::Search(need("give the words to look for")?.into()),
        "show" => {
            let id = need("give a record id")?;
            match id.split_whitespace().collect::<Vec<_>>()[..] {
                [id] => Cmd::Show {
                    id: id.into(),
                    full: false,
                },
                [id, "full"] => Cmd::Show {
                    id: id.into(),
                    full: true,
                },
                _ => return Err(USAGE.into()),
            }
        }
        "forget" => Cmd::Forget(
            need("give a record id")?
                .split_whitespace()
                .next()
                .unwrap_or("")
                .into(),
        ),
        "add" => {
            let (user, text) = match rest.strip_prefix("--user") {
                Some(t) if t.is_empty() || t.starts_with(char::is_whitespace) => (true, t.trim()),
                _ => (false, rest),
            };
            if text.is_empty() {
                return Err(format!("/memory add: give the note to save\n{USAGE}"));
            }
            Cmd::Add {
                text: text.into(),
                user,
            }
        }
        "edit" => {
            match need("give a record id and the new text")?.split_once(char::is_whitespace) {
                Some((id, text)) if !text.trim().is_empty() => Cmd::Edit {
                    id: id.into(),
                    text: text.trim().into(),
                },
                _ => {
                    return Err(format!(
                        "/memory edit: give a record id and the new text\n{USAGE}"
                    ));
                }
            }
        }
        _ => return Err(USAGE.into()),
    }))
}

/// What a confirmed dialog will do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pending {
    Add {
        text: String,
        user: bool,
    },
    Edit {
        id: String,
        text: String,
    },
    Forget {
        id: String,
    },
    /// A proposed or quarantined record under review: accept, reject or edit.
    Review {
        id: String,
    },
    Accept {
        id: String,
    },
}

/// Where memory is read from for one folder.
pub struct Ctx {
    pub home: PathBuf,
    pub repo: Repo,
    /// The project scope is readable and writable (trusted folder, memory on and not opted out here).
    pub project: bool,
    /// Why not, when `project` is false.
    pub why_not: Option<String>,
    secrets: Vec<String>,
}

impl Ctx {
    pub fn new(home: &Path, cwd: &Path, env: &Env, trusted: bool) -> Result<Self, String> {
        let settings = home.join("settings.json");
        if !enabled(&settings) {
            return Err("memory is off (synt.memory.enabled is false in the user settings)".into());
        }
        let repo = repo_of(cwd);
        let why_not = if !trusted {
            Some("this folder is not trusted, so project memory is off here; only the user scope is available".to_string())
        } else if disabled_here(&settings, &repo) {
            Some(
                "memory is turned off for this repository; only the user scope is available".into(),
            )
        } else {
            None
        };
        let secrets = env
            .iter()
            .filter(|(k, v)| is_secret_env_name(k) && v.len() >= 8)
            .map(|(_, v)| v.to_string())
            .collect();
        Ok(Ctx {
            home: home.into(),
            repo,
            project: why_not.is_none(),
            why_not,
            secrets,
        })
    }

    fn open(&self) -> Result<Store, String> {
        Store::open(&self.home, self.secrets.clone()).map_err(|e| san(&e.to_string()))
    }

    /// `Filter.workspace`: this repository plus user scope, or (key "") the user scope alone.
    fn filter(&self, status: &str, limit: usize) -> Filter {
        Filter {
            workspace: Some(if self.project {
                self.repo.key.clone()
            } else {
                String::new()
            }),
            kind: None,
            status: (status != "any").then(|| status.to_string()),
            limit,
        }
    }

    fn visible(&self, m: &Memory) -> bool {
        m.scope == "user"
            || (self.project && m.scope == "workspace" && m.scope_key == self.repo.key)
    }

    fn get(&self, s: &Store, id: &str) -> Result<Memory, String> {
        match s.get(id).map_err(|e| san(&e.to_string()))? {
            Some(m) if self.visible(&m) => Ok(m),
            Some(_) => Err(format!(
                "{} is not in the memory this folder can use (another repository, agent scope, or project memory is off here)",
                san(id)
            )),
            None => Err(format!("no record {}", san(id))),
        }
    }

    fn scope_of(&self, user: bool) -> (&'static str, String) {
        if user || !self.project {
            ("user", String::new())
        } else {
            ("workspace", self.repo.key.clone())
        }
    }
}

fn age(updated: &str) -> String {
    let Some(t) = synthesis_memory::parse_ts(updated) else {
        return "?".into();
    };
    let s = (synthesis_memory::OffsetDateTime::now_utc() - t)
        .whole_seconds()
        .max(0);
    match s {
        0..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The trust as the person must read it: only `owner` is theirs; `untrusted` is never injected by itself.
fn trust_label(t: Trust) -> String {
    match t {
        Trust::Untrusted => "UNTRUSTED".into(),
        t => t.as_str().into(),
    }
}

/// One CLI-style row: id, kind, scope, trust, age, and the title on one line.
pub fn row(m: &Memory) -> String {
    let status = if m.status == "active" {
        String::new()
    } else {
        format!("[{}] ", m.status)
    };
    let title: String = san(&m.title).chars().take(100).collect();
    format!(
        "{}  {}  {}  {}  {}  {status}{title}",
        san(&m.id),
        m.kind,
        m.scope,
        trust_label(m.trust),
        age(&m.updated_at)
    )
}

/// A record in markdown form, clamped to [`CLAMP`] lines unless `full` (then the source and flags too).
pub fn show(m: &Memory, full: bool) -> String {
    let md = markdown(m);
    let lines: Vec<&str> = md.lines().collect();
    let mut out = if full || lines.len() <= CLAMP {
        md.clone()
    } else {
        format!(
            "{}\n... {} more line(s): /memory show {} full",
            lines[..CLAMP].join("\n"),
            lines.len() - CLAMP,
            san(&m.id)
        )
    };
    if m.trust == Trust::Untrusted {
        out.push_str("\nUNTRUSTED: written from web, MCP or outside-workspace content; it is never given to the model by itself");
    }
    if let Some(r) = m.flags.iter().find_map(|f| f.strip_prefix("quarantine:")) {
        out.push_str(&format!("\nquarantined by rule: {}", san(r)));
    }
    if full {
        out.push_str(&format!(
            "\nflags: {} \u{b7} source: {}",
            san(&m.flags.join(",")),
            san(&m.source.to_string())
        ));
    }
    out
}

/// The refusal for text that looks like a secret (memory is kept on disk and given to the model later).
fn refuse_secret(s: &Store, text: &str) -> Result<(), String> {
    if s.redact(text) != text.trim() {
        return Err("not saved: the text contains something shaped like a secret (a key, token or password). Memory is kept on disk and given to the model in later sessions, so Synthesis never stores secrets. Remove the secret and try again.".into());
    }
    Ok(())
}

/// A long random-looking token (base64 or hex of 32+ characters, mixed classes): the secret check is a
/// heuristic, so such text needs an extra explicit confirmation instead of passing silently.
pub fn long_token(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || "+/=_-".contains(c)))
        .any(|t| {
            let (lo, up, di) = t.chars().fold((false, false, false), |(l, u, d), c| {
                (
                    l | c.is_ascii_lowercase(),
                    u | c.is_ascii_uppercase(),
                    d | c.is_ascii_digit(),
                )
            });
            t.len() >= 32 && (lo as u8 + up as u8 + di as u8) >= 2
        })
}

/// What the dock can show whole (a confirm never shows part of the text it is about).
const MAX_CHARS: usize = 2000;
const MAX_LINES: usize = 40;

fn check_len(what: &str, text: &str) -> Result<(), String> {
    let (c, l) = (text.chars().count(), text.lines().count());
    if c > MAX_CHARS || l > MAX_LINES {
        return Err(format!(
            "{what} is {c} chars and {l} lines, over {MAX_CHARS} chars / {MAX_LINES} lines, more than the dialog can show whole. Shorten it (a longer record can be read with `synt memory show` and approved there)."
        ));
    }
    Ok(())
}

fn snapshot(m: &Memory) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        m.id,
        m.status,
        m.trust.as_str(),
        m.version,
        m.updated_at,
        m.title,
        m.body
    )
}

/// Everything the confirmation is about, as it is now: [`apply`] refuses when it differs from what the
/// dialog was shown with (the record changed meanwhile).
pub fn fingerprint(c: &Ctx, p: &Pending) -> Result<String, String> {
    let s = c.open()?;
    Ok(match p {
        Pending::Add { text, user } => format!("add|{user}|{text}"),
        Pending::Edit { id, text } => format!("edit|{}|{text}", snapshot(&c.get(&s, id)?)),
        Pending::Forget { id } | Pending::Accept { id } | Pending::Review { id } => {
            format!("rec|{}", snapshot(&c.get(&s, id)?))
        }
    })
}

/// The whole text, line by line, with its size (what the dock shows in its scroll box).
fn whole(text: &str) -> String {
    let t = text.trim();
    format!(
        "{} line(s), {} chars, shown whole:\n{}",
        t.lines().count(),
        t.chars().count(),
        t.lines().map(san).collect::<Vec<_>>().join("\n")
    )
}

fn new_note(c: &Ctx, user: bool, text: &str, source: &str) -> NewMemory {
    let (scope, key) = c.scope_of(user);
    let title: String = text.chars().take(80).collect();
    let mut n = NewMemory::new(scope, &key, "fact", &title, text, Trust::Owner);
    n.confidence = 1.0;
    n.source = serde_json::json!({"origin": source});
    n
}

/// Runs a confirmed operation; the line to show.
pub fn apply(c: &Ctx, p: &Pending, seen: &str) -> Result<String, String> {
    if fingerprint(c, p)? != seen {
        return Err(
            "the note changed since the question was shown; nothing was done. Ask again.".into(),
        );
    }
    let mut s = c.open()?;
    let e = |x: synthesis_memory::Error| san(&x.to_string());
    match p {
        Pending::Add { text, user } => {
            check_len("the note", text)?;
            refuse_secret(&s, text)?;
            if c.scope_of(*user).0 != "user" {
                s.touch_workspace(&c.repo.key, &c.repo.root, &c.repo.label)
                    .map_err(e)?;
            }
            let o = s.put(new_note(c, *user, text, "desktop")).map_err(e)?;
            Ok(match o.rule {
                Some(r) => format!(
                    "{} {} ({}): quarantined by rule {r}, kept but not used until you accept it in /memory review",
                    o.op, o.id, o.status
                ),
                None => format!("{} {} ({})", o.op, o.id, o.status),
            })
        }
        Pending::Edit { id, text } => {
            let old = c.get(&s, id)?;
            if old.kind == "episode" || matches!(old.status.as_str(), "superseded" | "archived") {
                return Err(format!(
                    "{id} ({}, {}) cannot be edited",
                    old.kind, old.status
                ));
            }
            check_len("the note", text)?;
            refuse_secret(&s, text)?;
            let title: String = text.chars().take(80).collect();
            let mut n = NewMemory::new(
                &old.scope,
                &old.scope_key,
                &old.kind,
                &title,
                text,
                Trust::Owner,
            );
            (n.slot, n.pinned, n.tags, n.anchors, n.confidence) = (
                old.slot.clone(),
                old.pinned,
                old.tags.clone(),
                old.anchors.clone(),
                1.0,
            );
            n.source = serde_json::json!({"origin": "desktop_edit", "edits": id});
            let o = s.revise(id, n).map_err(e)?;
            Ok(match (o.op, o.rule) {
                ("quarantined", r) => format!(
                    "not applied: {} was quarantined by rule {} (see /memory review); {id} is unchanged",
                    o.id,
                    r.unwrap_or("?")
                ),
                ("deduped", _) => format!("no changes (identical to {})", o.id),
                _ => format!("edited {id}: new owner version {} ({})", o.id, o.status),
            })
        }
        Pending::Forget { id } => {
            c.get(&s, id)?;
            s.set_status(id, "archived").map_err(e)?;
            Ok(format!(
                "archived {id} (kept with its provenance; it is no longer given to the model)"
            ))
        }
        Pending::Accept { id } => {
            let m = c.get(&s, id)?;
            if !matches!(m.status.as_str(), "proposed" | "quarantined") {
                return Err(format!("{id} is {}, not pending", m.status));
            }
            check_len(id, &format!("{}\n{}", m.title, m.body))?;
            s.set_status(id, "active").map_err(e)?;
            Ok(format!(
                "{id} is active (trust stays {})",
                trust_label(m.trust)
            ))
        }
        Pending::Review { .. } => Err("choose accept, reject or edit".into()),
    }
}

/// The text above the options of a confirmation: what will happen, with what trust, and the COMPLETE
/// text it is about (the dock shows it in a scroll box). Text over the cap is refused, never shown in part.
pub fn describe(c: &Ctx, p: &Pending) -> Result<String, String> {
    let s = c.open()?;
    let heuristic = "The secret check is a heuristic, not a guarantee: read the text yourself.";
    let user_scope = |user: bool| -> String {
        if user || !c.project {
            format!(
                "USER SCOPE: this note applies to EVERY repository.{}\n",
                if c.project {
                    ""
                } else {
                    " This folder is NOT trusted (project memory is off here), so it goes to the user scope."
                }
            )
        } else {
            format!("Scope: this repository ({}).\n", san(&c.repo.label))
        }
    };
    let token = |text: &str| {
        if long_token(text) {
            "\nWARNING: the text holds a long random-looking string (32+ characters). If it is a key or token, answer No."
        } else {
            ""
        }
    };
    Ok(match p {
        Pending::Add { text, user } => {
            check_len("the note", text)?;
            refuse_secret(&s, text)?;
            let rule = scan(text.trim(), Trust::Owner)
                .map(|r| format!(" The scanner flags it (rule {r}): it will be kept quarantined, not used, until you accept it in /memory review."))
                .unwrap_or_default();
            format!(
                "{}Saves it with trust 'owner' (you typed it). Later sessions give it to the model as data in its context, so keep out anything you would not want it to read. {heuristic}{rule}{}\n{}",
                user_scope(*user),
                token(text),
                whole(text)
            )
        }
        Pending::Edit { id, text } => {
            let old = c.get(&s, id)?;
            check_len("the note", text)?;
            refuse_secret(&s, text)?;
            format!(
                "{}Replaces {id} ({}, {}) by a new version with trust 'owner'. The old text is kept as superseded. Later sessions give the new text to the model as data. {heuristic}{}\n{}",
                user_scope(old.scope == "user"),
                old.kind,
                trust_label(old.trust),
                token(text),
                whole(text)
            )
        }
        Pending::Forget { id } => {
            let m = c.get(&s, id)?;
            format!(
                "Archives {id}: it stays in the store with its provenance but is no longer given to the model.\n{}",
                whole(&m.title)
            )
        }
        Pending::Accept { id } | Pending::Review { id } => {
            let m = c.get(&s, id)?;
            check_len(id, &format!("{}\n{}", m.title, m.body))?;
            let who = match m.trust {
                Trust::Untrusted => {
                    "UNTRUSTED: it cites web, MCP or outside-workspace content. Accepting it removes it from review but its trust stays 'untrusted', so it is never given to the model by itself."
                }
                Trust::Agent => {
                    "Written by the model from its own reasoning, not by you. Accepting makes it active: later sessions give it to the model as data."
                }
                _ => "Accepting makes it active: later sessions give it to the model as data.",
            };
            format!("{who}\n{}", whole(&show(&m, true)))
        }
    })
}

/// The text needs the extra "I checked" answer (a long random-looking string).
fn needs_ack(p: &Pending) -> bool {
    matches!(p, Pending::Add { text, .. } | Pending::Edit { text, .. } if long_token(text))
}

fn dialog(kind: PickerKind, title: &str, header: String, options: &[(&str, &str)]) -> Dialog {
    Dialog {
        kind: DialogKind::Picker {
            kind,
            title: title.into(),
            headers: vec![(0, header)],
        },
        options: options
            .iter()
            .map(|(k, l)| (k.to_string(), l.to_string()))
            .collect(),
        // "no" is first and selected: nothing is saved unless the person moves off it and engages
        sel: 0,
        armed_until: 0,
        engaged: false,
    }
}

impl Live {
    fn memory_ctx(&self) -> Result<Ctx, String> {
        let e = self
            .engine
            .as_ref()
            .ok_or("the session is still starting")?;
        if e.is_remote() {
            return Err(crate::engine::NOT_REMOTE.into());
        }
        Ctx::new(&e.synthesis_home, Path::new(&e.cwd), &e.env, self.trusted)
    }

    /// `/memory ARG` with an argument.
    pub fn memory_cmd(&mut self, arg: &str, now: i64) {
        let cmd = match parse(arg) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => return self.note(e, true),
        };
        let r = self
            .memory_ctx()
            .and_then(|c| self.memory_run(&c, cmd, now));
        if let Err(e) = r {
            self.note(format!("/memory: {e}"), true);
        }
    }

    fn memory_run(&mut self, c: &Ctx, cmd: Cmd, now: i64) -> Result<(), String> {
        let s = c.open()?;
        let e = |x: synthesis_memory::Error| san(&x.to_string());
        let gate = c.why_not.as_ref().map(|w| format!("note: {w}"));
        match cmd {
            Cmd::List | Cmd::Search(_) | Cmd::Review => {
                let (rows, what) = match &cmd {
                    Cmd::List => (s.list(&c.filter("active", 50)).map_err(e)?, "record(s)"),
                    Cmd::Search(q) => (
                        s.search(q, &c.filter("active", 10)).map_err(e)?,
                        "record(s)",
                    ),
                    _ => {
                        let mut v = vec![];
                        for st in ["proposed", "quarantined"] {
                            v.extend(s.list(&c.filter(st, 100)).map_err(e)?);
                        }
                        (v, "item(s) to review")
                    }
                };
                if matches!(cmd, Cmd::Review) {
                    if rows.is_empty() {
                        self.note(
                            format!(
                                "0 items to review{}",
                                gate.map(|g| format!("\n{g}")).unwrap_or_default()
                            ),
                            false,
                        );
                    } else {
                        self.open_review(&rows, gate, now);
                    }
                    return Ok(());
                }
                let mut lines: Vec<String> = rows.iter().map(row).collect();
                lines.push(format!("{} {what}; /memory show ID for one", rows.len()));
                lines.extend(gate);
                self.note(lines.join("\n"), false);
            }
            Cmd::Show { id, full } => {
                let m = c.get(&s, &id)?;
                self.note(show(&m, full), false);
            }
            Cmd::Add { text, user } => self.confirm(c, Pending::Add { text, user }, now)?,
            Cmd::Edit { id, text } => self.confirm(c, Pending::Edit { id, text }, now)?,
            Cmd::Forget(id) => self.confirm(c, Pending::Forget { id }, now)?,
        }
        Ok(())
    }

    fn open_review(&mut self, rows: &[Memory], gate: Option<String>, now: i64) {
        let header = format!(
            "{} pending. Nothing is accepted by itself; pick one to read it and decide.{}",
            rows.len(),
            gate.map(|g| format!(" {g}")).unwrap_or_default()
        );
        let mut d = dialog(PickerKind::MemReview, "Review memory", header, &[]);
        d.options = rows.iter().map(|m| (m.id.clone(), row(m))).collect();
        self.push_memory_dialog(d, None, now);
    }

    fn push_memory_dialog(&mut self, d: Dialog, p: Option<(Pending, String)>, now: i64) {
        self.mem_pending = p;
        self.push_dialog(d, now);
    }

    /// The confirmation: says what will happen, "no" first. Refusals (a secret, a record out of scope) stop here.
    fn confirm(&mut self, c: &Ctx, p: Pending, now: i64) -> Result<(), String> {
        if self.dialogs.iter().any(|d| {
            matches!(
                &d.kind,
                DialogKind::Picker {
                    kind: PickerKind::MemConfirm,
                    ..
                }
            )
        }) {
            return Err("answer the open memory question first".into());
        }
        let header = describe(c, &p)?;
        let seen = fingerprint(c, &p)?;
        let ack = needs_ack(&p);
        let add_title = match &p {
            Pending::Add { user, .. } if *user || !c.project => {
                "Save to USER memory (every repository)?"
            }
            _ => "Save this to memory?",
        };
        let (title, options): (&str, &[(&str, &str)]) = match &p {
            Pending::Add { .. } => (
                add_title,
                &[
                    ("no", "No, do not save (esc also cancels)"),
                    ("yes", "Yes, save it"),
                ],
            ),
            Pending::Edit { .. } => (
                "Replace this memory note?",
                &[
                    ("no", "No, keep the old note (esc also cancels)"),
                    ("yes", "Yes, replace it"),
                ],
            ),
            Pending::Forget { .. } => (
                "Forget this memory note?",
                &[
                    ("no", "No, keep it (esc also cancels)"),
                    ("yes", "Yes, archive it"),
                ],
            ),
            Pending::Accept { .. } => (
                "Accept this memory note?",
                &[
                    ("no", "No, leave it pending (esc also cancels)"),
                    ("yes", "Yes, accept it"),
                ],
            ),
            Pending::Review { .. } => (
                "Review this memory note",
                &[
                    ("no", "Leave it pending (esc also cancels)"),
                    ("accept", "Accept: make it active"),
                    ("reject", "Reject: archive it"),
                    ("edit", "Edit: put it in the prompt to change"),
                ],
            ),
        };
        // a long random-looking string: the only way to save is a line that says the person checked it
        let flagged = [
            ("no", "No, do not save (esc also cancels)"),
            (
                "yes_token",
                "Yes, save it: I checked that the long random-looking string is not a secret",
            ),
        ];
        let options = if ack { &flagged[..] } else { options };
        self.push_memory_dialog(
            dialog(PickerKind::MemConfirm, title, header, options),
            Some((p, seen)),
            now,
        );
        Ok(())
    }

    /// An answer of the review list or of a confirmation (called from [`Live::answer`]).
    pub fn memory_answer(&mut self, kind: PickerKind, key: &str, now: i64) {
        let r = self
            .memory_ctx()
            .and_then(|c| match (kind, self.mem_pending.take(), key) {
                (PickerKind::MemReview, _, id) => self
                    .confirm(&c, Pending::Review { id: id.into() }, now)
                    .map(|_| None),
                (_, Some((Pending::Review { id }, _)), "accept") => {
                    self.confirm(&c, Pending::Accept { id }, now).map(|_| None)
                }
                (_, Some((Pending::Review { id }, _)), "reject") => {
                    self.confirm(&c, Pending::Forget { id }, now).map(|_| None)
                }
                (_, Some((Pending::Review { id }, _)), "edit") => {
                    let m = c.get(&c.open()?, &id)?;
                    self.returned_prompt = Some(format!(
                        "/memory edit {id} {}",
                        m.body.split_whitespace().collect::<Vec<_>>().join(" ")
                    ));
                    Ok(None)
                }
                (_, Some((p, seen)), "yes" | "yes_token") => apply(&c, &p, &seen).map(Some),
                _ => Ok(None),
            });
        match r {
            Ok(Some(done)) => self.note(done, false),
            Ok(None) => {}
            Err(e) => self.note(format!("/memory: {e}"), true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(d: &Path, trusted: bool) -> Ctx {
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let proj = d.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        Ctx::new(
            &home,
            &proj,
            &Env::from_pairs([("SYNT_API_KEY", "sk-live-abcdefgh12345")]),
            trusted,
        )
        .unwrap()
    }

    fn run(c: &Ctx, p: &Pending) -> Result<String, String> {
        apply(c, p, &fingerprint(c, p)?)
    }

    fn add(text: &str) -> Pending {
        Pending::Add {
            text: text.into(),
            user: false,
        }
    }

    #[test]
    fn parses_the_subcommands() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("list").unwrap(), Some(Cmd::List));
        assert_eq!(
            parse("search  two words").unwrap(),
            Some(Cmd::Search("two words".into()))
        );
        assert_eq!(
            parse("add use tabs").unwrap(),
            Some(Cmd::Add {
                text: "use tabs".into(),
                user: false
            })
        );
        assert_eq!(
            parse("add --user use tabs").unwrap(),
            Some(Cmd::Add {
                text: "use tabs".into(),
                user: true
            })
        );
        assert_eq!(
            parse("add --users x").unwrap(),
            Some(Cmd::Add {
                text: "--users x".into(),
                user: false
            })
        );
        assert_eq!(
            parse("show m-1 full").unwrap(),
            Some(Cmd::Show {
                id: "m-1".into(),
                full: true
            })
        );
        assert_eq!(
            parse("forget m-1").unwrap(),
            Some(Cmd::Forget("m-1".into()))
        );
        assert_eq!(
            parse("edit m-1 new text").unwrap(),
            Some(Cmd::Edit {
                id: "m-1".into(),
                text: "new text".into()
            })
        );
        for bad in [
            "add",
            "add --user",
            "search",
            "show",
            "edit m-1",
            "forget",
            "nope",
        ] {
            assert!(parse(bad).unwrap_err().contains("usage"), "{bad}");
        }
    }

    #[test]
    fn add_and_forget_go_through_the_store_with_owner_trust() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), true);
        let done = run(&c, &add("Tests run with cargo test")).unwrap();
        assert!(done.starts_with("added m-"), "{done}");
        let s = c.open().unwrap();
        let rows = s.list(&c.filter("active", 10)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].trust, rows[0].scope.as_str()),
            (Trust::Owner, "workspace")
        );
        assert!(row(&rows[0]).contains("owner") && row(&rows[0]).contains("Tests run"));
        let id = rows[0].id.clone();
        drop(s);
        run(&c, &Pending::Forget { id: id.clone() }).unwrap();
        assert_eq!(
            c.open().unwrap().get(&id).unwrap().unwrap().status,
            "archived"
        );
    }

    #[test]
    fn a_secret_shaped_note_is_refused_and_nothing_is_stored() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), true);
        for t in [
            "key is sk-live-abcdefgh12345 ok",
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            let e = run(&c, &add(t)).unwrap_err();
            assert!(e.contains("secret") && !e.contains("abcdefgh"), "{e}");
            // the confirm is refused too, before any dialog opens
            assert!(describe(&c, &add(t)).is_err());
        }
        assert!(
            c.open()
                .unwrap()
                .list(&c.filter("any", 10))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_untrusted_folder_sees_and_writes_only_the_user_scope() {
        let d = tempfile::tempdir().unwrap();
        let trusted = ctx(d.path(), true);
        run(&trusted, &add("repo note")).unwrap();
        run(
            &trusted,
            &Pending::Add {
                text: "user note".into(),
                user: true,
            },
        )
        .unwrap();
        let c = ctx(d.path(), false);
        assert!(!c.project && c.why_not.as_deref().unwrap().contains("not trusted"));
        let titles: Vec<String> = c
            .open()
            .unwrap()
            .list(&c.filter("active", 10))
            .unwrap()
            .into_iter()
            .map(|m| m.title)
            .collect();
        assert_eq!(titles, ["user note"]);
        let repo_id = trusted
            .open()
            .unwrap()
            .list(&trusted.filter("active", 10))
            .unwrap()
            .into_iter()
            .find(|m| m.scope == "workspace")
            .unwrap()
            .id;
        assert!(
            run(&c, &Pending::Forget { id: repo_id })
                .unwrap_err()
                .contains("not in the memory")
        );
        // an add lands in the user scope, and the confirmation says why
        assert!(
            describe(&c, &add("x y z")).unwrap().contains("USER SCOPE")
                && describe(&c, &add("x y z")).unwrap().contains("NOT trusted")
        );
        run(&c, &add("from untrusted folder")).unwrap();
        assert_eq!(
            c.open()
                .unwrap()
                .list(&c.filter("active", 10))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn accepting_changes_only_the_status_and_untrusted_stays_untrusted() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), true);
        let mut n = NewMemory::new(
            "workspace",
            &c.repo.key,
            "fact",
            "from the web",
            "the site says X",
            Trust::Untrusted,
        );
        n.status = Some("proposed".into());
        let id = c.open().unwrap().put(n).unwrap().id;
        let text = describe(&c, &Pending::Review { id: id.clone() }).unwrap();
        assert!(
            text.contains("UNTRUSTED") && text.contains("never given to the model by itself"),
            "{text}"
        );
        run(&c, &Pending::Accept { id: id.clone() }).unwrap();
        let m = c.open().unwrap().get(&id).unwrap().unwrap();
        assert_eq!((m.status.as_str(), m.trust), ("active", Trust::Untrusted));
        assert!(
            run(&c, &Pending::Accept { id })
                .unwrap_err()
                .contains("not pending")
        );
    }

    #[test]
    fn show_clamps_to_five_lines_until_full() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), true);
        run(&c, &add("a\nb\nc\nd\ne\nf\ng")).unwrap();
        let m = c
            .open()
            .unwrap()
            .list(&c.filter("active", 1))
            .unwrap()
            .remove(0);
        let short = show(&m, false);
        assert!(
            short.contains("more line(s)") && !short.contains("\ng"),
            "{short}"
        );
        assert!(show(&m, true).contains("\ng"));
    }

    #[test]
    fn a_confirmation_defaults_to_no_and_esc_or_type_ahead_cannot_approve() {
        let mut l = Live::new();
        let d = dialog(
            PickerKind::MemConfirm,
            "Save this to memory?",
            "h".into(),
            &[("no", "No"), ("yes", "Yes")],
        );
        l.push_memory_dialog(
            d,
            Some((Pending::Forget { id: "m-1".into() }, String::new())),
            10_000,
        );
        let d = &l.dialogs[0];
        assert_eq!((d.sel, d.options[0].0.as_str()), (0, "no"));
        // like an approval: Enter and digits are ignored until the person engages it
        assert!(d.needs_person() && !d.answerable());
        // a key typed while it arms picks nothing
        assert_eq!(l.pick(1, 10_000), None);
        // Esc is "no": the dialog goes, nothing is pending, nothing is applied
        assert_eq!(l.reject(), None);
        assert!(l.dialogs.is_empty() && l.mem_pending.is_none());
    }

    #[test]
    fn the_review_list_is_a_plain_picker_and_confirming_needs_the_person() {
        let mut l = Live::new();
        let d = dialog(PickerKind::MemReview, "Review memory", "h".into(), &[]);
        l.push_memory_dialog(d, None, 0);
        assert!(
            !l.dialogs[0].needs_person(),
            "choosing a row only opens the confirmation"
        );
    }

    #[test]
    fn text_is_confirmed_whole_or_refused() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), true);
        // within the cap: the last line is in what the dock shows, with the size
        let ok = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let h = describe(&c, &add(&ok)).unwrap();
        assert!(
            h.contains("30 line(s)") && h.contains("line 30") && h.contains("heuristic"),
            "{h}"
        );
        // over the cap (chars or lines): refused, not clipped
        for long in ["x ".repeat(1001), "a\n".repeat(41)] {
            let e = describe(&c, &add(&long)).unwrap_err();
            assert!(e.contains("Shorten"), "{e}");
            assert!(run(&c, &add(&long)).unwrap_err().contains("Shorten"));
        }
        // a pending record too long to show whole cannot be accepted here
        let mut n = NewMemory::new(
            "workspace",
            &c.repo.key,
            "fact",
            "t",
            &"w ".repeat(1000),
            Trust::Agent,
        );
        n.status = Some("proposed".into());
        let id = c.open().unwrap().put(n).unwrap().id;
        assert!(
            describe(&c, &Pending::Accept { id: id.clone() })
                .unwrap_err()
                .contains("Shorten")
        );
        assert!(run(&c, &Pending::Accept { id }).is_err());
    }

    #[test]
    fn a_record_that_changed_since_the_dialog_is_not_acted_on() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), true);
        let mut n = NewMemory::new(
            "workspace",
            &c.repo.key,
            "fact",
            "t",
            "body one",
            Trust::Agent,
        );
        n.status = Some("proposed".into());
        let id = c.open().unwrap().put(n).unwrap().id;
        let p = Pending::Accept { id: id.clone() };
        let seen = fingerprint(&c, &p).unwrap();
        // the record is changed (here: parked again) before the person answers
        c.open().unwrap().set_status(&id, "quarantined").unwrap();
        let e = apply(&c, &p, &seen).unwrap_err();
        assert!(e.contains("changed since"), "{e}");
        assert_ne!(
            c.open().unwrap().get(&id).unwrap().unwrap().status,
            "active"
        );
    }

    #[test]
    fn long_random_looking_strings_need_the_extra_answer() {
        assert!(long_token(
            "use 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 here"
        ));
        assert!(long_token(
            "x QWxhZGRpbjpvcGVuIHNlc2FtZSBhbmQgbW9yZSBieXRlcw== y"
        ));
        assert!(!long_token(
            "run cargo test --workspace before pushing, see apps/desktop/src/tui_view/dialog"
        ));
        assert!(!long_token("short Ab1 tokens only"));
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path(), true);
        let mut l = Live::new();
        l.confirm(
            &c,
            add("id 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
            0,
        )
        .unwrap();
        let keys: Vec<&str> = l.dialogs[0].options.iter().map(|o| o.0.as_str()).collect();
        assert_eq!(keys, ["no", "yes_token"]);
        let DialogKind::Picker { headers, .. } = &l.dialogs[0].kind else {
            panic!()
        };
        assert!(headers[0].1.contains("WARNING") && headers[0].1.contains("9f86d081884c7d65"));
    }
}
