//! The conductor's settings (super-agent-s1.md 4.4, conductor.md 3.2). User and managed settings only: a
//! project file is never read here, and the model comes from the USER config (`load_config` with no project
//! layer), so a repository cannot pick the model that steers the person's sessions.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use synthesis_config::{Env, ProjectTrust};

use crate::{homes, load_config};

/// What the confirm knob may put on its auto list. `close` and `message_worker` can never be there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    SendMessage,
    Interrupt,
    CreateSession,
}

impl Action {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "send_message" => Some(Self::SendMessage),
            "interrupt" => Some(Self::Interrupt),
            "create_session" => Some(Self::CreateSession),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// A card for everything (the default).
    All,
    /// These actions may skip the card when nothing forces one. P3b never executes without a click: this only
    /// marks a proposal `auto_eligible` for the approvals policy (approvals-concept.md A5).
    Auto(BTreeSet<Action>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConductorConfig {
    /// `provider/model` of the conductor; `None`: the user's default.
    pub model: Option<String>,
    pub confirm: Confirm,
    /// Legacy USD caps, `None` unless the person sets them (no prices are known for custom providers): the engine
    /// refuses an unpriced model under one. Without it the engine's per-task turn limit bounds the session.
    pub max_cost_usd: Option<f64>,
    pub session_max_cost_usd: Option<f64>,
    pub max_sessions: usize,
    pub diagnostics: Vec<String>,
}

pub const DEFAULT_MAX_SESSIONS: usize = 6;
const MAX_SESSIONS_LIMIT: u64 = 20;

impl ConductorConfig {
    /// From the user's config files and the managed file. The project layer is never loaded.
    pub fn load(env: &Env, managed: &Path) -> Result<Self, String> {
        let h = homes(env)?;
        // user config only: no project layer, whatever the folder the window shows
        let model = load_config(env, &h, None, ProjectTrust::Untrusted)?.model;
        let mut docs = vec![];
        for (path, what) in [
            (h.synthesis.join("settings.json"), "user settings"),
            (managed.to_path_buf(), "managed settings"),
        ] {
            docs.push((what, synthesis_settings::read_json(&path, what)?));
        }
        Ok(Self::from_docs(
            model,
            docs.iter().filter_map(|(w, d)| d.as_ref().map(|d| (*w, d))),
        ))
    }

    /// `docs`: the user document, then the managed one; later keys replace earlier ones. Wrong values keep the
    /// safe default and leave a diagnostic that names the key, never the value.
    pub fn from_docs<'a>(
        model: Option<String>,
        docs: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Self {
        let mut c = Self {
            model,
            confirm: Confirm::All,
            max_cost_usd: None,
            session_max_cost_usd: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
            diagnostics: vec![],
        };
        for (what, doc) in docs {
            let Some(o) = doc
                .get("synt")
                .and_then(|s| s.get("conductor"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            let mut say = |m: String| c.diagnostics.push(m);
            let mut bad =
                |key: &str, why: &str| say(format!("{what}: synt.conductor.{key} {why}, ignored"));
            let usd = |v: &Value| v.as_f64().filter(|x| x.is_finite() && *x > 0.0);
            for (k, v) in o {
                match k.as_str() {
                    "model" => match v.as_str().filter(|m| m.contains('/') && m.len() <= 200) {
                        Some(m) => c.model = Some(m.into()),
                        None => bad(k, "must be \"provider/model\""),
                    },
                    "max_cost_usd" => match usd(v) {
                        Some(x) => c.max_cost_usd = Some(x),
                        None => bad(k, "must be a positive number"),
                    },
                    "session_max_cost_usd" => match usd(v) {
                        Some(x) => c.session_max_cost_usd = Some(x),
                        None => bad(k, "must be a positive number"),
                    },
                    "max_sessions" => {
                        match v.as_u64().filter(|n| (1..=MAX_SESSIONS_LIMIT).contains(n)) {
                            Some(n) => c.max_sessions = n as usize,
                            None => bad(k, "must be 1..=20"),
                        }
                    }
                    "confirm" => match parse_confirm(v) {
                        Ok((conf, notes)) => {
                            c.confirm = conf;
                            for n in notes {
                                bad(k, &n);
                            }
                        }
                        Err(why) => bad(k, &why),
                    },
                    other => bad(other, "is not a setting"),
                }
            }
        }
        c
    }
}

/// `"all"` or `{"auto": [...]}`. `message_worker` and `close` are refused with a note; the rest of the list stays.
fn parse_confirm(v: &Value) -> Result<(Confirm, Vec<String>), String> {
    if v == "all" {
        return Ok((Confirm::All, vec![]));
    }
    let list = v
        .get("auto")
        .and_then(Value::as_array)
        .filter(|_| v.as_object().is_some_and(|o| o.len() == 1))
        .ok_or("must be \"all\" or {\"auto\": [...]}")?;
    let (mut set, mut notes) = (BTreeSet::new(), vec![]);
    for a in list {
        match a.as_str() {
            Some("message_worker") => notes.push(
                "message_worker can never be automatic (a worker message always needs the person's card), refused".into(),
            ),
            Some("close") => notes.push("close can never be automatic, refused".into()),
            Some(n) => match Action::parse(n) {
                Some(x) => {
                    set.insert(x);
                }
                None => notes.push("unknown action refused".into()),
            },
            None => return Err("must list action names".into()),
        }
    }
    Ok((
        if set.is_empty() {
            Confirm::All
        } else {
            Confirm::Auto(set)
        },
        notes,
    ))
}
