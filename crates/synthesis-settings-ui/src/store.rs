//! Effective values with their origin, and the one write path: `set` / `unset` change the USER
//! settings file only, through `edit_json_atomic` (temp file + rename, mode kept, 0600 when new).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use synthesis_config::{Env, LoadContext, ProjectTrust};
use synthesis_harness::{AutoCompact, ShellSettings, ToolSearchMode};
use synthesis_settings::{
    UiSettings, edit_json_atomic, load_settings_layers, local_settings_path, read_json,
};

use crate::schema::{Edit, Field, Kind, Scope, SettingsSchema, validate_kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Default,
    User,
    Project,
    Local,
    /// The `--settings <file>` layer.
    Flag,
    Managed,
}

/// One field as the UI shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    /// In force now (the default when no layer sets it; `Null` when there is none).
    pub value: Value,
    pub origin: Origin,
    /// What the USER file itself holds, even when a higher layer wins.
    pub user_value: Option<Value>,
    /// Layers that hold the key but cannot set it, with the reason.
    pub refused: Vec<(Origin, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SetError {
    UnknownKey(String),
    NotEditable {
        key: String,
        reason: String,
    },
    NeedsConfirmation {
        key: String,
        reason: String,
    },
    /// The managed file sets it; a user edit would have no effect.
    Locked {
        key: String,
    },
    Invalid {
        key: String,
        why: String,
    },
    Io {
        key: String,
        msg: String,
    },
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKey(k) => write!(f, "{k}: not a setting"),
            Self::NotEditable { key, reason } => write!(f, "{key}: not editable here: {reason}"),
            Self::NeedsConfirmation { key, reason } => {
                write!(f, "{key}: needs confirmation: {reason}")
            }
            Self::Locked { key } => write!(f, "{key}: set by the managed settings file"),
            Self::Invalid { key, why } => write!(f, "{key}: {why}"),
            Self::Io { key, msg } => write!(f, "{key}: {msg}"),
        }
    }
}
impl std::error::Error for SetError {}

struct Layer {
    origin: Origin,
    doc: Option<Value>,
}

pub struct SettingsStore {
    pub schema: SettingsSchema,
    pub entries: Vec<Entry>,
    /// Loader diagnostics (value-free) of the last load or write.
    pub diagnostics: Vec<String>,
    /// Files that exist but cannot be used (the CLI refuses to start with these).
    pub errors: Vec<String>,
    user_path: PathBuf,
    cwd: PathBuf,
    user_home: PathBuf,
    managed: PathBuf,
    flag: Option<PathBuf>,
    trust: ProjectTrust,
}

impl SettingsStore {
    /// `managed`: the organisation file (`synthesis_settings::managed_settings_path()` in the
    /// app); `flag`: the `--settings` file when the session was started with one.
    pub fn open(
        ctx: &LoadContext<'_>,
        trust: ProjectTrust,
        managed: &Path,
        flag: Option<&Path>,
    ) -> Self {
        let mut s = SettingsStore {
            schema: SettingsSchema::new(),
            entries: vec![],
            diagnostics: vec![],
            errors: vec![],
            user_path: ctx.synthesis_home.join("settings.json"),
            cwd: ctx.cwd.to_path_buf(),
            user_home: ctx.user_home.to_path_buf(),
            managed: managed.to_path_buf(),
            flag: flag.map(Path::to_path_buf),
            trust,
        };
        s.reload();
        s
    }

    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key == key)
    }

    /// Reads every layer again (after a write, or when a file changed on disk).
    pub fn reload(&mut self) {
        let env = Env::default();
        let ctx = LoadContext {
            cwd: &self.cwd,
            synthesis_home: self.user_path.parent().unwrap_or(Path::new(".")),
            user_home: &self.user_home,
            env: &env,
        };
        let loaded = load_settings_layers(&ctx, self.trust, &self.managed, self.flag.as_deref());
        self.diagnostics = loaded.diagnostics;
        self.errors = loaded.errors;
        let root = loaded.project_root;
        let mut paths = vec![
            (Origin::User, self.user_path.clone()),
            (Origin::Project, root.join(".synt").join("settings.json")),
            (Origin::Local, local_settings_path(&root)),
        ];
        if let Some(f) = &self.flag {
            paths.push((Origin::Flag, f.clone()));
        }
        paths.push((Origin::Managed, self.managed.clone()));
        let layers: Vec<Layer> = paths
            .into_iter()
            .map(|(origin, p)| Layer {
                origin,
                doc: read_json(&p, "settings").ok().flatten(),
            })
            .collect();
        let trusted = self.trust == ProjectTrust::Trusted;
        self.entries = self
            .schema
            .fields
            .iter()
            .map(|f| entry_of(f, &layers, trusted))
            .collect();
    }

    /// Why the UI must not offer an edit, or `None` when `set` can be tried.
    pub fn read_only_reason(&self, key: &str) -> Option<String> {
        let f = self.schema.field(key)?;
        match f.edit {
            Edit::No(r) => Some(r.into()),
            _ if self.entry(key).is_some_and(|e| e.origin == Origin::Managed) => {
                Some("set by the managed settings file".into())
            }
            _ => None,
        }
    }

    /// Writes `value` at `key` in the USER settings file. Returns the loader diagnostics after the
    /// write. Keys that need confirmation are refused here; see [`Self::set_confirmed`].
    pub fn set(&mut self, key: &str, value: Value) -> Result<Vec<String>, SetError> {
        self.write(key, Some(value), false)
    }

    /// [`Self::set`] after the person confirmed the [`Edit::Confirm`] reason.
    pub fn set_confirmed(&mut self, key: &str, value: Value) -> Result<Vec<String>, SetError> {
        self.write(key, Some(value), true)
    }

    /// Removes `key` from the USER file (back to the default or a lower layer).
    pub fn unset(&mut self, key: &str) -> Result<Vec<String>, SetError> {
        self.write(key, None, true)
    }

    fn write(
        &mut self,
        key: &str,
        value: Option<Value>,
        confirmed: bool,
    ) -> Result<Vec<String>, SetError> {
        let bad = |why: String| SetError::Invalid {
            key: key.into(),
            why,
        };
        let f = self
            .schema
            .field(key)
            .ok_or_else(|| SetError::UnknownKey(key.into()))?
            .clone();
        match f.edit {
            Edit::No(r) => {
                return Err(SetError::NotEditable {
                    key: key.into(),
                    reason: r.into(),
                });
            }
            Edit::Confirm(r) if !confirmed => {
                return Err(SetError::NeedsConfirmation {
                    key: key.into(),
                    reason: r.into(),
                });
            }
            _ => {}
        }
        if self.entry(key).is_some_and(|e| e.origin == Origin::Managed) {
            return Err(SetError::Locked { key: key.into() });
        }
        if let Some(v) = &value {
            validate_kind(f.kind, v).map_err(bad)?;
            if has_secret_literal(v) {
                return Err(bad(
                    "looks like a credential; settings never hold credentials".into(),
                ));
            }
        }
        let path: Vec<&str> = f.path().collect();
        edit_json_atomic(&self.user_path, "user settings", |doc| {
            apply(doc, &path, value.clone())?;
            if value.is_some() {
                check_loaders(doc, key)?;
            }
            Ok(())
        })
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidInput => bad(e.to_string()),
            _ => SetError::Io {
                key: key.into(),
                msg: e.to_string(),
            },
        })?;
        self.reload();
        Ok(self.diagnostics.clone())
    }
}

fn invalid(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
}

/// Sets (`Some`) or removes (`None`) the value at `path`, creating objects on the way. A parent
/// that exists but is not an object is an error: nothing is overwritten.
fn apply(doc: &mut Value, path: &[&str], value: Option<Value>) -> std::io::Result<()> {
    let (last, parents) = path.split_last().expect("a key has a segment");
    let mut cur = doc;
    for seg in parents {
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| invalid(format!("{seg}: the parent is not an object")))?;
        if value.is_none() && !obj.contains_key(*seg) {
            return Ok(());
        }
        cur = obj.entry(*seg).or_insert_with(|| Value::Object(Map::new()));
    }
    let obj = cur
        .as_object_mut()
        .ok_or_else(|| invalid(format!("{last}: the parent is not an object")))?;
    match value {
        Some(v) => obj.insert((*last).into(), v),
        None => obj.shift_remove(*last),
    };
    Ok(())
}

/// The loaders' own validators on the document about to be written: a diagnostic that names the
/// key refuses the write.
fn check_loaders(doc: &Value, key: &str) -> std::io::Result<()> {
    let mut diags = UiSettings::from_settings(doc).1;
    diags.extend(AutoCompact::from_settings(doc).1);
    diags.extend(ToolSearchMode::from_settings(doc).1);
    diags.extend(ShellSettings::from_settings(doc).1);
    match diags.into_iter().find(|d| d.contains(key)) {
        Some(d) => Err(invalid(d)),
        None => Ok(()),
    }
}

/// Token-shaped text (same prefixes as the `synt mcp add` guard, which is private to
/// synthesis-app): settings written from here never hold a credential.
fn has_secret_literal(v: &Value) -> bool {
    const PREFIXES: [&str; 14] = [
        "sk-",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "AIza",
        "glpat-",
        "npm_",
        "sk_live_",
        "xoxa-",
        "xoxs-",
        "eyJ",
    ];
    match v {
        Value::String(s) => {
            let s = s.trim();
            PREFIXES
                .iter()
                .any(|p| s.starts_with(p) && s.len() >= p.len() + 8)
        }
        Value::Array(a) => a.iter().any(has_secret_literal),
        Value::Object(o) => o.values().any(has_secret_literal),
        _ => false,
    }
}

fn layer_name(o: Origin) -> &'static str {
    match o {
        Origin::Default => "default",
        Origin::User => "user settings",
        Origin::Project => "project settings",
        Origin::Local => "local settings",
        Origin::Flag => "--settings file",
        Origin::Managed => "managed settings",
    }
}

/// Mirrors the trust rules of `synthesis-settings` for the keys in the schema.
fn refusal(f: &Field, o: Origin, v: &Value, trusted: bool) -> Option<String> {
    let project = matches!(o, Origin::Project | Origin::Local);
    match f.scope {
        Scope::UserOnly if o != Origin::User => {
            Some("this key is read from the user settings file only".into())
        }
        Scope::Layered if project && !trusted => {
            Some("project not trusted: it can only add deny/ask rules and MCP disables".into())
        }
        _ if project && f.key == "permissions.defaultMode" => {
            let m = v.as_str().unwrap_or("");
            if ["auto", "bypassPermissions"].contains(&m) {
                Some(format!("{m} is not allowed from a project"))
            } else if !trusted && !["default", "manual", "plan", "dontAsk"].contains(&m) {
                Some("project not trusted: only default, manual, plan, dontAsk".into())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn entry_of(f: &Field, layers: &[Layer], trusted: bool) -> Entry {
    let is_list = matches!(f.kind, Kind::List(_));
    let mut value: Option<Value> = None;
    let mut origin = Origin::Default;
    let mut refused = vec![];
    let mut user_value = None;
    for l in layers {
        let Some(v) = l.doc.as_ref().and_then(|d| lookup(d, f)) else {
            continue;
        };
        if l.origin == Origin::User {
            user_value = Some(v.clone());
        }
        if let Some(why) = refusal(f, l.origin, v, trusted) {
            refused.push((l.origin, format!("{}: {why}", layer_name(l.origin))));
            continue;
        }
        value = match (is_list, value.take(), v) {
            (true, Some(Value::Array(mut a)), Value::Array(b)) => {
                for x in b {
                    if !a.contains(x) {
                        a.push(x.clone());
                    }
                }
                Some(Value::Array(a))
            }
            _ => Some(v.clone()),
        };
        origin = l.origin;
    }
    Entry {
        key: f.key.clone(),
        value: value.or_else(|| f.default.clone()).unwrap_or(Value::Null),
        origin,
        user_value,
        refused,
    }
}

fn lookup<'a>(doc: &'a Value, f: &Field) -> Option<&'a Value> {
    f.path().try_fold(doc, |d, seg| d.get(seg))
}
