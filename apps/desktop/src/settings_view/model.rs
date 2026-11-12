//! The Settings view's model: no gpui. It maps the schema of `synthesis-settings-ui` to controls, runs
//! the edit / confirm / reset flow against the USER settings file, and reads the read-only info
//! (providers' key status, MCP servers, AGENTS.md files, skills). Everything is reached through the
//! injected [`Cfg`], so tests run on a temporary `SYNT_HOME` and never touch `~/.synt`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use synthesis_app::mcp::{AddArgs, ScopeArg, add_server, plan, remove_server};
use synthesis_app::{Homes, load_config};
use synthesis_config::{Env, LoadContext, ProjectTrust, resolve_credential};
use synthesis_settings::load_settings_with_managed;
use synthesis_settings_ui::{Edit, Field, Kind, Origin, Section, SetError, SettingsStore};

/// Everything the model reads from the outside.
#[derive(Clone)]
pub struct Cfg {
    pub env: Env,
    /// `SYNT_HOME`.
    pub home: PathBuf,
    pub user_home: PathBuf,
    pub cwd: PathBuf,
    /// The organisation file (`synthesis_settings::managed_settings_path()` in the app).
    pub managed: PathBuf,
    pub trust: ProjectTrust,
}

impl Cfg {
    pub fn from_env(env: &Env, cwd: PathBuf, trust: ProjectTrust) -> Result<Self, String> {
        let h = synthesis_app::homes(env)?;
        Ok(Cfg {
            env: env.clone(),
            home: h.synthesis,
            user_home: h.user,
            cwd,
            managed: synthesis_settings::managed_settings_path(),
            trust,
        })
    }
    fn homes(&self) -> Homes {
        Homes {
            user: self.user_home.clone(),
            synthesis: self.home.clone(),
        }
    }
}

/// What the person did to a field.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Bool(bool),
    Choice(String),
    /// The text of a number input.
    Text(String),
    /// One item typed into a list editor.
    Add(String),
    Remove(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Saved,
    /// Waiting for [`Model::confirm`] / [`Model::cancel`].
    NeedsConfirm,
    /// The reason is in [`Model::errors`].
    Rejected,
}

/// A write that waits for the person to accept the risk.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub key: String,
    pub label: String,
    pub value: Value,
    /// One sentence from the schema (`Edit::Confirm`).
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Control {
    Switch(bool),
    Choice {
        opts: &'static [&'static str],
        cur: String,
    },
    Number {
        text: String,
        range: String,
    },
    List(Vec<String>),
    /// A whole percentage dragged on a slider.
    Slider {
        min: u64,
        max: u64,
        value: u64,
    },
    /// A value shown, never written (summary only: no object contents).
    Static(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    /// The card heading inside the tab.
    pub group: &'static str,
    pub label: String,
    pub help: String,
    pub control: Control,
    pub origin: Origin,
    pub restart: bool,
    /// The default, as text.
    pub default: String,
    /// Read by the CLI only: the desktop view has nothing to apply.
    pub terminal_only: bool,
    /// Written in this run and a running session keeps the old value.
    pub changed: bool,
    pub resettable: bool,
    /// Why the control is disabled.
    pub locked: Option<String>,
    pub confirm: bool,
    /// Higher layers that hold the key but may not set it.
    pub refused: Vec<String>,
    pub error: Option<String>,
}

pub struct Group {
    pub section: Section,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpRow {
    pub name: String,
    pub transport: String,
    pub scope: String,
    /// Host of the URL or the command name; never arguments, query strings or headers.
    pub target: String,
    /// Known only for a running session; the desktop does not hold the engine here.
    pub tools: Option<usize>,
}

/// Provider key status: names of variables only, never a value (ADR-011).
#[derive(Clone, Debug, PartialEq)]
pub struct KeyStatus {
    pub provider: String,
    pub label: String,
    pub set: bool,
}

#[derive(Clone, Debug, Default)]
pub struct McpForm {
    pub name: String,
    /// `stdio`, `http` or `sse`.
    pub transport: String,
    pub url: String,
    pub command: String,
    pub env: String,
    pub header: String,
}

/// Where the doctor thread leaves its rows (or its timeout).
type DoctorSlot = Arc<Mutex<Option<Result<Vec<synthesis_app::doctor::Row>, String>>>>;

pub struct Model {
    pub cfg: Cfg,
    pub store: SettingsStore,
    pub filter: String,
    pub errors: BTreeMap<String, String>,
    pub pending: Option<Pending>,
    /// Loader diagnostics after the last write.
    pub diag: Vec<String>,
    /// Keys written in this run whose change needs a new session.
    pub changed: BTreeSet<String>,
    pub mcp: Vec<McpRow>,
    /// Folders remembered as trusted (user store), oldest first, each with whether its trusted configuration
    /// still matches the fingerprint taken when it was trusted (`None`: a parent grant or a missing folder).
    pub trusted: Vec<(synthesis_session::trust::TrustRecord, Option<bool>)>,
    /// Rows of the trust store ignored because their MAC does not verify.
    pub trust_unverified: usize,
    /// Why the trust store could not be read (the list is then empty).
    pub trust_error: Option<String>,
    pub mcp_error: Option<String>,
    pub providers: Vec<KeyStatus>,
    pub agents_md: Vec<(PathBuf, bool)>,
    /// The checks of `/doctor`, read-only; `None` while they run (or after they timed out: `doctor_error`).
    pub doctor: Option<Vec<synthesis_app::doctor::Row>>,
    pub doctor_error: Option<String>,
    doctor_slot: DoctorSlot,
    /// `(name, description, source)`.
    pub skills: Vec<(String, String, String)>,
}

/// `synt.tui` keys the CLI reads and the desktop view does not (it has no mouse mode or queue lines,
/// and its welcome and cards are the design's).
const TERMINAL_ONLY: [&str; 5] = [
    "synt.tui.logo",
    "synt.tui.diffs_expanded",
    "synt.tui.queue_indicators",
    "synt.tui.mouse_capture",
    "synt.tui.copy_on_select",
];

/// Why the window cannot blur what is behind it on this platform (`None`: it can). GPUI blurs on macOS
/// (NSVisualEffectView) and Windows (acrylic); on Linux only some Wayland compositors do, and X11 never.
pub fn blur_unsupported() -> Option<&'static str> {
    (!cfg!(any(target_os = "macos", target_os = "windows")))
        .then_some("window blur is not available on Linux (it depends on the compositor)")
}

fn matches(f: &Field, q: &str) -> bool {
    q.is_empty()
        || [&f.key, &f.label, &f.help]
            .iter()
            .any(|s| s.to_lowercase().contains(q))
}

/// A value for display. Objects and lists of non-strings are summarised, never printed.
pub fn show_value(v: &Value) -> String {
    match v {
        Value::Null => "(none)".into(),
        Value::String(s) => s.clone(),
        Value::Object(o) => format!("{} entries", o.len()),
        Value::Array(a) if a.iter().all(Value::is_string) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Array(a) => format!("{} entries", a.len()),
        v => v.to_string(),
    }
}

pub fn origin_label(o: Origin) -> &'static str {
    match o {
        Origin::Default => "default",
        Origin::User => "user",
        Origin::Project => "project",
        Origin::Local => "local",
        Origin::Flag => "flag",
        Origin::Managed => "managed",
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    auth.rsplit('@').next().unwrap_or("").to_string()
}

impl Model {
    pub fn open(cfg: Cfg) -> Self {
        let store = SettingsStore::open(
            &LoadContext {
                cwd: &cfg.cwd,
                synthesis_home: &cfg.home,
                user_home: &cfg.user_home,
                env: &cfg.env,
            },
            cfg.trust,
            &cfg.managed,
            None,
        );
        let mut m = Model {
            cfg,
            store,
            filter: String::new(),
            errors: BTreeMap::new(),
            pending: None,
            diag: vec![],
            changed: BTreeSet::new(),
            mcp: vec![],
            trusted: vec![],
            trust_unverified: 0,
            trust_error: None,
            mcp_error: None,
            providers: vec![],
            agents_md: vec![],
            doctor: None,
            doctor_error: None,
            doctor_slot: Arc::default(),
            skills: vec![],
        };
        m.refresh_info();
        m
    }

    /// Starts the doctor checks on their own thread (capped); [`Model::poll_doctor`] collects them.
    pub fn start_doctor(&mut self) {
        let (env, cwd, trusted) = (
            self.cfg.env.clone(),
            self.cfg.cwd.clone(),
            self.cfg.trust == ProjectTrust::Trusted,
        );
        let slot = self.doctor_slot.clone();
        std::thread::spawn(move || {
            let r = synthesis_app::doctor::rows_capped(
                &env,
                &cwd,
                trusted,
                std::time::Duration::from_secs(20),
            );
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        });
    }

    /// True once the checks finished (their rows or their error are set).
    pub fn poll_doctor(&mut self) -> bool {
        let done = self
            .doctor_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match done {
            Some(Ok(r)) => self.doctor = Some(r),
            Some(Err(e)) => self.doctor_error = Some(e),
            None => return false,
        }
        true
    }

    /// Reads again the parts that are not in the store.
    pub fn refresh_info(&mut self) {
        let c = &self.cfg;
        self.agents_md = [
            c.home.join("AGENTS.md"),
            c.cwd.join("AGENTS.md"),
            c.cwd.join(".synt").join("AGENTS.md"),
        ]
        .into_iter()
        .map(|p| {
            let there = p.is_file();
            (p, there)
        })
        .collect();
        self.skills = synthesis_tools::discover_skills(&c.env, &c.cwd)
            .into_iter()
            .map(|s| {
                let src = format!("{:?}", s.source).to_lowercase();
                (s.name, s.description, src)
            })
            .collect();
        let listing = match synthesis_session::trust::list(&c.home) {
            Ok(l) => {
                self.trust_error = None;
                l
            }
            Err(e) => {
                self.trust_error = Some(e.to_string());
                Default::default()
            }
        };
        self.trust_unverified = listing.unverified;
        self.trusted = listing
            .records
            .into_iter()
            .filter(|r| r.scope != synthesis_session::trust::CHILD)
            .map(|r| {
                let same = (r.scope == synthesis_session::trust::FOLDER
                    && std::path::Path::new(&r.root).is_dir())
                .then(|| {
                    synthesis_session::trust::fingerprint_root(std::path::Path::new(&r.root))
                        .is_ok_and(|f| f == r.fingerprint)
                });
                (r, same)
            })
            .collect();
        self.mcp.clear();
        self.providers.clear();
        self.mcp_error = None;
        let h = c.homes();
        match load_config(&c.env, &h, Some(&c.cwd), ProjectTrust::Untrusted) {
            Err(e) => self.mcp_error = Some(e),
            Ok(cfg) => {
                let st = load_settings_with_managed(
                    &LoadContext {
                        cwd: &c.cwd,
                        synthesis_home: &c.home,
                        user_home: &c.user_home,
                        env: &c.env,
                    },
                    ProjectTrust::Untrusted,
                    &c.managed,
                );
                let p = plan(&cfg, &st, &st, &c.cwd, ProjectTrust::Untrusted, &[]);
                self.mcp = p
                    .servers
                    .iter()
                    .map(|s| McpRow {
                        name: s.name.clone(),
                        transport: s.transport.map_or("?", |t| t.as_str()).into(),
                        scope: s.scope.clone().unwrap_or_else(|| "?".into()),
                        target: match (&s.url, &s.command) {
                            (Some(u), _) => host_of(u),
                            (None, Some(c)) => c.clone(),
                            _ => String::new(),
                        },
                        tools: None,
                    })
                    .collect();
                self.providers = cfg
                    .providers
                    .iter()
                    .filter(|(_, p)| !p.is_no_auth())
                    .map(|(id, p)| {
                        let cred = resolve_credential(id, Some(p), &c.home, &c.env);
                        let missing = cfg.warnings.iter().find_map(|w| {
                            let rest = w.strip_prefix(&format!("provider \"{id}\" "))?;
                            let (_, tail) = rest.split_once("missing env var \"")?;
                            tail.split_once('"').map(|(n, _)| n.to_string())
                        });
                        let (label, set) = match cred {
                            Some(cr) => {
                                let s = cr.source.as_str();
                                let name = s
                                    .strip_prefix("env ")
                                    .or_else(|| s.strip_prefix("config {env:")?.strip_suffix('}'))
                                    .or_else(|| s.strip_prefix("{env:")?.strip_suffix('}'));
                                (format!("{} set", name.unwrap_or(s)), true)
                            }
                            None => (
                                match missing {
                                    Some(n) => format!("{n} unset"),
                                    None => "unset (SYNT_API_KEY, or an {env:NAME} reference in synt.json)".into(),
                                },
                                false,
                            ),
                        };
                        KeyStatus {
                            provider: id.clone(),
                            label,
                            set,
                        }
                    })
                    .collect();
            }
        }
    }

    fn list_items(&self, key: &str) -> Vec<String> {
        self.store
            .entry(key)
            .and_then(|e| e.value.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn control_of(&self, f: &Field, value: &Value, locked: bool) -> Control {
        if locked && matches!(f.edit, Edit::No(_)) {
            return Control::Static(show_value(value));
        }
        match f.kind {
            Kind::Bool => Control::Switch(value.as_bool().unwrap_or(false)),
            Kind::Enum(opts) => Control::Choice {
                opts,
                cur: value.as_str().unwrap_or_default().into(),
            },
            Kind::Int { min, max } => Control::Number {
                text: value.as_u64().map(|n| n.to_string()).unwrap_or_default(),
                range: format!("{min} to {max}"),
            },
            Kind::Float { min, max } => Control::Number {
                text: value.as_f64().map(|n| n.to_string()).unwrap_or_default(),
                range: format!("{min} to {max}"),
            },
            Kind::Percent { min, max } => Control::Slider {
                min,
                max,
                value: value.as_u64().unwrap_or(max),
            },
            // a text field is drawn like a number field (an input)
            Kind::Text => Control::Number {
                text: value.as_str().unwrap_or_default().into(),
                range: "name".into(),
            },
            Kind::List(_) => Control::List(self.list_items(&f.key)),
            Kind::ReadOnly => Control::Static(show_value(value)),
        }
    }

    /// Why the UI must not edit `key`: the schema, the managed file, or this platform.
    fn lock_reason(&self, key: &str) -> Option<String> {
        self.store.read_only_reason(key).or_else(|| match key {
            "synt.tui.blur" => blur_unsupported().map(Into::into),
            "synt.desktop.wsl.enabled" => crate::wsl::toggle_lock(),
            "synt.desktop.wsl.distro" => (!cfg!(windows)).then(|| "Windows only".into()),
            _ => None,
        })
    }

    pub fn row(&self, f: &Field) -> Row {
        let e = self.store.entry(&f.key);
        let reason = self.lock_reason(&f.key);
        let value = e.map_or(Value::Null, |e| e.value.clone());
        Row {
            key: f.key.clone(),
            group: f.group,
            label: f.label.clone(),
            help: f.help.clone(),
            control: self.control_of(f, &value, reason.is_some()),
            origin: e.map_or(Origin::Default, |e| e.origin),
            restart: f.restart,
            default: f
                .default
                .as_ref()
                .map_or_else(|| "unset".into(), show_value),
            terminal_only: TERMINAL_ONLY.contains(&f.key.as_str()),
            changed: self.changed.contains(&f.key),
            resettable: reason.is_none() && e.is_some_and(|e| e.user_value.is_some()),
            locked: reason,
            confirm: matches!(f.edit, Edit::Confirm(_)),
            refused: e
                .map(|e| {
                    e.refused
                        .iter()
                        .map(|(o, why)| format!("{} copy ignored: {why}", origin_label(*o)))
                        .collect()
                })
                .unwrap_or_default(),
            error: self.errors.get(&f.key).cloned(),
        }
    }

    /// The sections with the rows that match the search box (all rows when it is empty). A section
    /// with no field is kept (it carries read-only info) when nothing is typed.
    pub fn groups(&self) -> Vec<Group> {
        let q = self.filter.trim().to_lowercase();
        Section::ALL
            .iter()
            .map(|&s| Group {
                section: s,
                rows: self
                    .store
                    .schema
                    .section(s)
                    .filter(|f| matches(f, &q))
                    .map(|f| self.row(f))
                    .collect(),
            })
            .filter(|g| {
                !g.rows.is_empty()
                    || (q.is_empty() && self.store.schema.section(g.section).next().is_none())
                    || (!q.is_empty() && g.section.title().to_lowercase().contains(&q))
            })
            .collect()
    }

    pub fn change(&mut self, key: &str, c: Change) -> Outcome {
        let Some(f) = self.store.schema.field(key).cloned() else {
            return Outcome::Rejected;
        };
        if let Some(why) = self
            .lock_reason(key)
            .filter(|_| key == "synt.tui.blur" || key.starts_with("synt.desktop.wsl."))
        {
            self.errors
                .insert(key.into(), format!("{key}: not editable here: {why}"));
            return Outcome::Rejected;
        }
        let value = match (c, f.kind) {
            (Change::Bool(b), _) => json!(b),
            (Change::Choice(s), _) => json!(s),
            // an unparsable text goes on as a string: the schema's own message says what is expected
            (Change::Text(t), Kind::Int { .. } | Kind::Percent { .. }) => t
                .trim()
                .parse::<u64>()
                .map_or(Value::String(t), |n| json!(n)),
            (Change::Text(t), Kind::Text) => Value::String(t.trim().into()),
            (Change::Text(t), Kind::Float { .. }) => t
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .map_or(Value::String(t), |n| json!(n)),
            (Change::Add(item), Kind::List(_)) => {
                let item = item.trim().to_string();
                if item.is_empty() {
                    return Outcome::Rejected;
                }
                let mut l = self.list_items(key);
                l.push(item);
                json!(l)
            }
            (Change::Remove(i), Kind::List(_)) => {
                let mut l = self.list_items(key);
                if i >= l.len() {
                    return Outcome::Rejected;
                }
                l.remove(i);
                json!(l)
            }
            _ => return Outcome::Rejected,
        };
        match self.store.set(key, value.clone()) {
            Err(SetError::NeedsConfirmation { reason, .. }) => {
                self.errors.remove(key);
                let mut chars = reason.chars();
                let reason = chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                    .unwrap_or_default();
                self.pending = Some(Pending {
                    key: key.into(),
                    label: f.label,
                    value,
                    reason,
                });
                Outcome::NeedsConfirm
            }
            r => self.done(key, r),
        }
    }

    /// The person accepted the risk of the pending write.
    pub fn confirm(&mut self) -> Outcome {
        let Some(p) = self.pending.take() else {
            return Outcome::Rejected;
        };
        let r = self.store.set_confirmed(&p.key, p.value);
        self.done(&p.key, r)
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// Back to the default (or the lower layer): removes the key from the USER file.
    pub fn reset(&mut self, key: &str) -> Outcome {
        if let Some(why) = self
            .lock_reason(key)
            .filter(|_| key == "synt.tui.blur" || key.starts_with("synt.desktop.wsl."))
        {
            self.errors
                .insert(key.into(), format!("{key}: not editable here: {why}"));
            return Outcome::Rejected;
        }
        let r = self.store.unset(key);
        self.done(key, r)
    }

    fn done(&mut self, key: &str, r: Result<Vec<String>, SetError>) -> Outcome {
        match r {
            Ok(d) => {
                self.errors.remove(key);
                self.diag = d;
                if self.store.schema.field(key).is_some_and(|f| f.restart) {
                    self.changed.insert(key.into());
                }
                Outcome::Saved
            }
            Err(e) => {
                self.errors.insert(key.into(), e.to_string());
                Outcome::Rejected
            }
        }
    }

    pub fn restart_pending(&self) -> usize {
        self.changed.len()
    }

    /// `synt mcp add` on the USER `synt.json`. The error text is the guard's, verbatim.
    pub fn add_mcp(&mut self, f: &McpForm) -> Result<(), String> {
        let http = f.transport != "stdio";
        let one = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        let a = AddArgs {
            name: f.name.trim().into(),
            scope: ScopeArg::User,
            url: if http { one(&f.url) } else { None },
            transport: if http {
                f.transport.clone()
            } else {
                // `synt mcp add` takes http|sse; a command makes the server stdio
                "http".into()
            },
            env: one(&f.env).into_iter().collect(),
            header: one(&f.header).into_iter().collect(),
            cwd: None,
            force: false,
            command: if http {
                vec![]
            } else {
                f.command.split_whitespace().map(String::from).collect()
            },
        };
        add_server(&self.cfg.home.join("synt.json"), &a)?;
        self.refresh_info();
        Ok(())
    }

    /// Only servers of the user file can be removed here.
    /// Forgets a remembered trusted folder; it takes effect at the next engine start.
    pub fn revoke_trust(&mut self, root: &str) -> Result<(), String> {
        synthesis_session::trust::revoke(&self.cfg.home, root).map_err(|e| e.to_string())?;
        self.refresh_info();
        Ok(())
    }

    pub fn remove_mcp(&mut self, name: &str) -> Result<(), String> {
        remove_server(&self.cfg.home.join("synt.json"), name)?;
        self.refresh_info();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> (tempfile::TempDir, Model) {
        let t = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(t.path()).unwrap();
        let (home, cwd) = (root.join("home"), root.join("ws"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let env = Env::from_pairs([
            ("HOME", home.to_string_lossy().into_owned()),
            ("SYNT_HOME", home.to_string_lossy().into_owned()),
        ]);
        let m = Model::open(Cfg {
            env,
            user_home: home.clone(),
            home,
            cwd,
            managed: root.join("managed.json"),
            trust: ProjectTrust::Untrusted,
        });
        (t, m)
    }

    fn row(m: &Model, k: &str) -> Row {
        m.row(m.store.schema.field(k).unwrap())
    }

    fn user_file(m: &Model) -> Value {
        serde_json::from_slice(&std::fs::read(m.cfg.home.join("settings.json")).unwrap()).unwrap()
    }

    #[test]
    fn kinds_map_to_controls() {
        let (_t, m) = model();
        assert_eq!(
            row(&m, "synt.memory.enabled").control,
            Control::Switch(true)
        );
        assert!(matches!(
            row(&m, "synt.mcp.toolSearch").control,
            Control::Choice { cur, .. } if cur == "auto"
        ));
        assert!(matches!(
            row(&m, "synt.images.retentionDays").control,
            Control::Number { ref text, .. } if text == "30"
        ));
        assert!(matches!(
            row(&m, "synt.memory.disabled_workspaces").control,
            Control::List(ref l) if l.is_empty()
        ));
        let r = row(&m, "hooks");
        assert!(matches!(r.control, Control::Static(_)));
        assert!(r.locked.as_deref().unwrap().contains("by hand"));
        assert_eq!(r.origin, Origin::Default);
    }

    #[test]
    fn every_schema_field_gets_a_row_in_its_section() {
        let (_t, m) = model();
        let n: usize = m.groups().iter().map(|g| g.rows.len()).sum();
        assert_eq!(n, m.store.schema.fields.len());
    }

    #[test]
    fn switch_writes_the_user_file_and_marks_restart() {
        let (_t, mut m) = model();
        assert_eq!(
            m.change("synt.memory.enabled", Change::Bool(false)),
            Outcome::Saved
        );
        assert_eq!(user_file(&m)["synt"]["memory"]["enabled"], json!(false));
        let r = row(&m, "synt.memory.enabled");
        assert_eq!(
            (r.origin, r.changed, r.resettable),
            (Origin::User, true, true)
        );
        // INTERFACE keys apply without a restart
        m.change("synt.tui.panels.mcp", Change::Bool(false));
        assert!(!row(&m, "synt.tui.panels.mcp").changed);
        assert_eq!(m.restart_pending(), 1);
    }

    #[test]
    fn number_validation_names_the_key_and_keeps_the_file() {
        let (_t, mut m) = model();
        for bad in ["abc", "99999", "-1", ""] {
            assert_eq!(
                m.change("synt.images.retentionDays", Change::Text(bad.into())),
                Outcome::Rejected
            );
            let e = row(&m, "synt.images.retentionDays").error.unwrap();
            assert!(e.starts_with("synt.images.retentionDays: "), "{e}");
        }
        assert!(!m.cfg.home.join("settings.json").exists());
        assert_eq!(
            m.change("synt.images.retentionDays", Change::Text(" 7 ".into())),
            Outcome::Saved
        );
        assert_eq!(user_file(&m)["synt"]["images"]["retentionDays"], json!(7));
        assert!(row(&m, "synt.images.retentionDays").error.is_none());
        // float with a range
        assert_eq!(
            m.change(
                "synt.context.auto_compact_threshold",
                Change::Text("0.2".into())
            ),
            Outcome::Rejected
        );
        assert_eq!(
            m.change(
                "synt.context.auto_compact_threshold",
                Change::Text("0.9".into())
            ),
            Outcome::Saved
        );
    }

    #[test]
    fn confirm_flow_writes_only_after_confirm() {
        let (_t, mut m) = model();
        let k = "synt.memory.reflection";
        assert_eq!(m.change(k, Change::Bool(true)), Outcome::NeedsConfirm);
        let p = m.pending.clone().unwrap();
        assert_eq!(p.key, k);
        assert!(
            p.reason.starts_with("This sends task digests"),
            "{}",
            p.reason
        );
        assert!(!m.cfg.home.join("settings.json").exists());
        m.cancel();
        assert!(m.pending.is_none());
        assert!(!m.cfg.home.join("settings.json").exists());
        m.change(k, Change::Bool(true));
        assert_eq!(m.confirm(), Outcome::Saved);
        assert_eq!(user_file(&m)["synt"]["memory"]["reflection"], json!(true));
        assert_eq!(m.confirm(), Outcome::Rejected, "nothing pending");
        assert!(row(&m, k).confirm);
    }

    #[test]
    fn reset_removes_the_key() {
        let (_t, mut m) = model();
        m.change("synt.tui.motion", Change::Bool(false));
        assert!(row(&m, "synt.tui.motion").resettable);
        assert_eq!(m.reset("synt.tui.motion"), Outcome::Saved);
        let r = row(&m, "synt.tui.motion");
        assert_eq!(
            (r.origin, r.resettable, r.control),
            (Origin::Default, false, Control::Switch(true))
        );
        assert!(user_file(&m)["synt"]["tui"].get("motion").is_none());
    }

    #[test]
    fn list_add_and_remove() {
        let (_t, mut m) = model();
        let k = "synt.memory.disabled_workspaces";
        m.change(k, Change::Add("/a/*".into()));
        m.change(k, Change::Add(" /b ".into()));
        assert_eq!(
            row(&m, k).control,
            Control::List(vec!["/a/*".into(), "/b".into()])
        );
        assert_eq!(m.change(k, Change::Add("  ".into())), Outcome::Rejected);
        m.change(k, Change::Remove(0));
        assert_eq!(row(&m, k).control, Control::List(vec!["/b".into()]));
        assert_eq!(m.change(k, Change::Remove(5)), Outcome::Rejected);
    }

    #[test]
    fn locked_rows_refuse_and_say_why() {
        let (_t, mut m) = model();
        // edit = No
        assert_eq!(m.change("hooks", Change::Bool(true)), Outcome::Rejected);
        assert!(m.errors["hooks"].contains("not editable"));
        assert_eq!(m.reset("hooks"), Outcome::Rejected);
        // managed lock
        std::fs::write(&m.cfg.managed, r#"{"synt":{"budget":{"max_turns":5}}}"#).unwrap();
        m.store.reload();
        let r = row(&m, "synt.budget.max_turns");
        assert_eq!(r.origin, Origin::Managed);
        assert_eq!(
            r.locked.as_deref(),
            Some("set by the managed settings file")
        );
        assert!(!r.resettable);
        assert_eq!(
            m.change("synt.budget.max_turns", Change::Text("9".into())),
            Outcome::Rejected
        );
        assert!(m.errors["synt.budget.max_turns"].contains("managed"));
    }

    #[test]
    fn filter_keeps_matching_rows_and_sections() {
        let (_t, mut m) = model();
        m.filter = "panels".into();
        let g = m.groups();
        assert!(g.iter().all(|g| g.section == Section::Interface));
        assert_eq!(g[0].rows.len(), 8);
        m.filter = "no such thing".into();
        assert!(m.groups().is_empty());
        m.filter = "skills".into();
        assert!(m.groups().iter().any(|g| g.section == Section::Skills));
    }

    #[test]
    fn malformed_user_file_is_reported_not_overwritten() {
        let (_t, mut m) = model();
        std::fs::write(m.cfg.home.join("settings.json"), "{ nope").unwrap();
        m.store.reload();
        assert_eq!(
            m.change("synt.tui.motion", Change::Bool(false)),
            Outcome::Rejected
        );
        assert_eq!(
            std::fs::read_to_string(m.cfg.home.join("settings.json")).unwrap(),
            "{ nope"
        );
        assert!(!m.errors["synt.tui.motion"].is_empty());
    }

    #[test]
    fn mcp_add_list_remove_and_credential_guard() {
        let (_t, mut m) = model();
        let form = McpForm {
            name: "docs".into(),
            transport: "http".into(),
            url: "https://example.com/mcp".into(),
            header: "Authorization: Bearer ${DOCS_TOKEN}".into(),
            ..Default::default()
        };
        m.add_mcp(&form).unwrap();
        assert_eq!(m.mcp.len(), 1);
        assert_eq!(
            (m.mcp[0].name.as_str(), m.mcp[0].transport.as_str()),
            ("docs", "http")
        );
        assert_eq!(m.mcp[0].target, "example.com");
        assert_eq!(
            host_of("https://u:p@h.io:8/x?t=1"),
            "h.io:8",
            "no userinfo, path or query"
        );
        // a credential in the URL is refused with the guard's own text
        let e = m
            .add_mcp(&McpForm {
                name: "u".into(),
                url: "https://u:pw@example.com/".into(),
                ..form.clone()
            })
            .unwrap_err();
        assert!(e.contains("credential"), "{e}");
        assert!(!e.contains(":pw@"), "{e}");
        assert!(m.add_mcp(&form).unwrap_err().contains("already exists"));
        // a literal secret is refused by the guard, the text is the guard's own
        let bad = McpForm {
            name: "bad".into(),
            header: "Authorization: Bearer sk-live-123456".into(),
            ..form.clone()
        };
        let e = m.add_mcp(&bad).unwrap_err();
        assert!(e.contains("${"), "{e}");
        assert!(!e.contains("sk-live-123456"), "{e}");
        assert_eq!(m.mcp.len(), 1);
        // stdio
        m.add_mcp(&McpForm {
            name: "fs".into(),
            transport: "stdio".into(),
            command: "npx -y @scope/server".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(m.mcp.iter().any(|s| s.name == "fs" && s.target == "npx"));
        m.remove_mcp("docs").unwrap();
        assert!(m.remove_mcp("docs").is_err());
        assert_eq!(m.mcp.len(), 1);
    }

    #[test]
    fn the_doctor_loads_in_the_background_and_opening_does_not_wait_for_it() {
        let (_t, mut m) = model();
        // opening the model runs no checks: the summary says "checking" until the thread is polled in
        assert!(m.doctor.is_none() && m.doctor_error.is_none());
        m.start_doctor();
        let t0 = std::time::Instant::now();
        while !m.poll_doctor() {
            assert!(t0.elapsed().as_secs() < 30, "the doctor never finished");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(m.doctor.as_ref().is_some_and(|r| !r.is_empty()));
    }

    #[test]
    fn provider_key_status_names_variables_and_never_values() {
        let (_t, mut m) = model();
        std::fs::write(
            m.cfg.home.join("synt.json"),
            r#"{"$schema":"SCHEMA","providers":{"omniroute":{"type":"openai","baseURL":"http://localhost:1/v1","apiKey":"{env:OMNIROUTE_API_KEY}","models":["m"]}}}"#
                .replace("SCHEMA", synthesis_config::SCHEMA_V1),
        )
        .unwrap();
        m.refresh_info();
        assert_eq!(m.mcp_error, None);
        let p = m
            .providers
            .iter()
            .find(|p| p.provider == "omniroute")
            .unwrap();
        assert!(!p.set);
        assert_eq!(p.label, "OMNIROUTE_API_KEY unset");
        m.cfg.env = Env::from_pairs([
            ("HOME", m.cfg.user_home.to_string_lossy().into_owned()),
            ("SYNT_HOME", m.cfg.home.to_string_lossy().into_owned()),
            ("OMNIROUTE_API_KEY", "topsecretvalue".to_string()),
        ]);
        m.refresh_info();
        let all = format!("{:?}", m.providers);
        assert!(!all.contains("topsecretvalue"));
        let p = m
            .providers
            .iter()
            .find(|p| p.provider == "omniroute")
            .unwrap();
        assert!(p.set);
        assert_eq!(p.label, "OMNIROUTE_API_KEY set");
    }

    #[test]
    fn info_lists_agents_md_and_skills() {
        let (_t, mut m) = model();
        std::fs::write(m.cfg.cwd.join("AGENTS.md"), "x").unwrap();
        m.refresh_info();
        assert_eq!(m.agents_md.len(), 3);
        assert_eq!(m.agents_md.iter().filter(|(_, there)| *there).count(), 1);
        assert!(!m.skills.is_empty(), "bundled skills are listed");
    }

    /// The user file of the owner's machine has this shape (`synt.tui` with four panel keys, the rest of the
    /// settings beside it); tests run on a copy, never on `~/.synt`.
    const REAL_SHAPE: &str = r#"{"permissions":{"deny":["Bash(rm:*)"]},"env":{"FOO":"1"},"synt":{"budget":{"max_turns":50},"tui":{"panels":{"session":true,"mcp":true,"tools":true,"activity":true},"logo":"art","show_thinking":true,"verbose":true,"diffs_expanded":true,"queue_indicators":true,"motion":true,"mouse_capture":true,"copy_on_select":true}}}"#;

    #[test]
    fn opacity_on_the_real_file_shape_validates_persists_and_resets() {
        let (_t, mut m) = model();
        std::fs::write(m.cfg.home.join("settings.json"), REAL_SHAPE).unwrap();
        m.store.reload();
        let k = "synt.tui.opacity";
        let r = row(&m, k);
        assert_eq!(
            r.control,
            Control::Slider {
                min: 30,
                max: 100,
                value: 100
            }
        );
        assert_eq!(
            (r.origin, r.default.as_str(), r.resettable, r.restart),
            (Origin::Default, "100", false, false)
        );
        assert_eq!(m.change(k, Change::Text("55".into())), Outcome::Saved);
        let f = user_file(&m);
        assert_eq!(f["synt"]["tui"]["opacity"], json!(55));
        // everything else in the file is as it was
        let mut want: Value = serde_json::from_str(REAL_SHAPE).unwrap();
        want["synt"]["tui"]["opacity"] = json!(55);
        assert_eq!(f, want);
        let r = row(&m, k);
        assert_eq!((r.origin, r.resettable), (Origin::User, true));
        let (tui, diag) = synthesis_settings::read_user_ui(&m.cfg.home, "tui");
        assert_eq!((tui.opacity, diag), (55, vec![]));
        // under the minimum, over 100, not a number: refused with the range, the file keeps 55
        for bad in ["29", "101", "abc", "50.5", ""] {
            assert_eq!(
                m.change(k, Change::Text(bad.into())),
                Outcome::Rejected,
                "{bad}"
            );
            assert!(row(&m, k).error.unwrap().contains("30 to 100"));
        }
        assert_eq!(user_file(&m)["synt"]["tui"]["opacity"], json!(55));
        assert_eq!(m.reset(k), Outcome::Saved);
        assert!(user_file(&m)["synt"]["tui"].get("opacity").is_none());
        assert_eq!(row(&m, k).origin, Origin::Default);
    }

    #[test]
    fn blur_is_a_switch_where_the_platform_blurs_and_locked_with_the_reason_elsewhere() {
        let (_t, mut m) = model();
        let k = "synt.tui.blur";
        match blur_unsupported() {
            None => {
                assert_eq!(row(&m, k).control, Control::Switch(false));
                assert!(row(&m, k).locked.is_none());
                assert_eq!(m.change(k, Change::Bool(true)), Outcome::Saved);
                assert_eq!(user_file(&m)["synt"]["tui"]["blur"], json!(true));
            }
            Some(why) => {
                assert_eq!(row(&m, k).locked.as_deref(), Some(why));
                assert_eq!(m.change(k, Change::Bool(true)), Outcome::Rejected);
                assert!(m.errors[k].contains(why));
                assert!(!m.cfg.home.join("settings.json").exists());
            }
        }
    }

    #[test]
    fn the_wsl_toggle_is_locked_off_windows_and_the_distro_is_a_text_row() {
        let (_t, mut m) = model();
        let (on, distro) = ("synt.desktop.wsl.enabled", "synt.desktop.wsl.distro");
        assert_eq!(row(&m, on).control, Control::Switch(false));
        assert!(matches!(row(&m, distro).control, Control::Number { .. }));
        if cfg!(windows) {
            return; // there it depends on wsl.exe, not testable here
        }
        for k in [on, distro] {
            assert_eq!(row(&m, k).locked.as_deref(), Some("Windows only"));
        }
        assert_eq!(m.change(on, Change::Bool(true)), Outcome::Rejected);
        assert_eq!(
            m.change(distro, Change::Text("U".into())),
            Outcome::Rejected
        );
        assert!(!m.cfg.home.join("settings.json").exists());
    }

    #[test]
    fn rows_say_when_a_change_applies() {
        let (_t, m) = model();
        // the desktop applies these now
        for k in [
            "synt.tui.opacity",
            "synt.tui.verbose",
            "synt.tui.motion",
            "synt.tui.panels.usage",
        ] {
            let r = row(&m, k);
            assert!(!r.restart && !r.terminal_only, "{k}");
        }
        // the CLI reads these; the desktop view has nothing to apply
        for k in [
            "synt.tui.logo",
            "synt.tui.mouse_capture",
            "synt.tui.copy_on_select",
        ] {
            assert!(row(&m, k).terminal_only, "{k}");
        }
        // engine settings wait for a session
        assert!(row(&m, "synt.memory.enabled").restart);
    }

    #[test]
    fn remembered_trust_is_listed_and_revoked_from_the_settings() {
        let (_t, mut m) = model();
        assert!(m.trusted.is_empty());
        let ws = m.cfg.cwd.clone();
        let fp = synthesis_session::trust::fingerprint_root(&ws).unwrap();
        let policy = synthesis_session::trust::Policy {
            home: Some(m.cfg.home.join("uh")),
            synt_home: m.cfg.home.clone(),
        };
        synthesis_session::trust::add(&m.cfg.home, &ws, "folder", &fp, &policy).unwrap();
        m.refresh_info();
        assert_eq!(m.trusted.len(), 1);
        assert_eq!(m.trusted[0].1, Some(true), "config unchanged");
        std::fs::create_dir_all(ws.join(".synt")).unwrap();
        std::fs::write(ws.join(".synt/settings.json"), "{}").unwrap();
        m.refresh_info();
        assert_eq!(m.trusted[0].1, Some(false), "config changed since");
        m.revoke_trust(&ws.to_string_lossy()).unwrap();
        assert!(m.trusted.is_empty());
    }
}
