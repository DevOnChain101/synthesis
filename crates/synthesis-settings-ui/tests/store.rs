use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use synthesis_config::{Env, LoadContext, ProjectTrust};
use synthesis_settings_ui::{Edit, Kind, Origin, SetError, SettingsSchema, SettingsStore};
use tempfile::TempDir;

struct Fx {
    home: TempDir,
    proj: TempDir,
}

impl Fx {
    fn new() -> Self {
        let f = Fx {
            home: tempfile::tempdir().unwrap(),
            proj: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(f.proj.path().join(".synt")).unwrap();
        f
    }
    fn user(&self) -> std::path::PathBuf {
        self.home.path().join("settings.json")
    }
    fn managed(&self) -> std::path::PathBuf {
        self.home.path().join("managed.json")
    }
    fn write(&self, p: &Path, v: Value) {
        fs::write(p, v.to_string()).unwrap();
    }
    fn open(&self, trust: ProjectTrust) -> SettingsStore {
        let env = Env::default();
        let ctx = LoadContext {
            cwd: self.proj.path(),
            synthesis_home: self.home.path(),
            user_home: self.home.path(),
            env: &env,
        };
        SettingsStore::open(&ctx, trust, &self.managed(), None)
    }
    fn read(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.user()).unwrap()).unwrap()
    }
}

#[test]
fn every_field_has_a_unique_key_and_a_sound_default() {
    let s = SettingsSchema::new();
    for (i, f) in s.fields.iter().enumerate() {
        assert!(s.fields[..i].iter().all(|g| g.key != f.key), "{}", f.key);
        if let Some(d) = &f.default
            && !matches!(f.kind, Kind::ReadOnly)
        {
            synthesis_settings_ui::validate_kind(f.kind, d)
                .unwrap_or_else(|e| panic!("{}: {e}", f.key));
        }
        if matches!(f.kind, Kind::ReadOnly) {
            assert!(matches!(f.edit, Edit::No(_)), "{}", f.key);
        }
    }
}

#[test]
fn defaults_match_the_loaders() {
    let (tui, _) = synthesis_settings::UiSettings::from_settings(&json!({}));
    let s = SettingsSchema::new();
    assert_eq!(
        s.field("synt.tui.motion").unwrap().default,
        Some(json!(tui.motion))
    );
    assert_eq!(
        s.field("synt.tui.logo").unwrap().default,
        Some(json!("art"))
    );
    assert_eq!(
        s.field("synt.context.auto_compact_threshold")
            .unwrap()
            .default,
        Some(json!(synthesis_harness::AutoCompact::default().threshold))
    );
}

#[test]
fn round_trip_and_origin() {
    let f = Fx::new();
    let mut st = f.open(ProjectTrust::Untrusted);
    assert_eq!(st.entry("synt.tui.motion").unwrap().origin, Origin::Default);
    assert_eq!(st.entry("synt.tui.motion").unwrap().value, json!(true));
    st.set("synt.tui.motion", json!(false)).unwrap();
    st.set("synt.context.auto_compact_threshold", json!(0.7))
        .unwrap();
    st.set("synt.mcp.toolSearch", json!("off")).unwrap();
    st.set("synt.tui.panels.mcp", json!(false)).unwrap();
    let e = st.entry("synt.tui.motion").unwrap();
    assert_eq!((&e.value, e.origin), (&json!(false), Origin::User));
    // a fresh store sees it, and the loaders agree
    let st2 = f.open(ProjectTrust::Untrusted);
    assert_eq!(
        st2.entry("synt.mcp.toolSearch").unwrap().value,
        json!("off")
    );
    let v = f.read();
    assert!(!synthesis_settings::UiSettings::from_settings(&v).0.panels[1]);
    assert_eq!(
        synthesis_harness::AutoCompact::from_settings(&v)
            .0
            .threshold,
        0.7
    );
    // unset goes back to the default
    st.unset("synt.tui.motion").unwrap();
    assert_eq!(st.entry("synt.tui.motion").unwrap().origin, Origin::Default);
}

#[test]
fn unknown_keys_and_order_are_kept() {
    let f = Fx::new();
    f.write(
        &f.user(),
        json!({"zzz": 1, "permissions": {"allow": ["Bash(ls)"]}, "synt": {"future": {"a": 1}, "tui": {"x": 2}}}),
    );
    let mut st = f.open(ProjectTrust::Untrusted);
    st.set("synt.memory.enabled", json!(false)).unwrap();
    let v = f.read();
    assert_eq!(v["zzz"], 1);
    assert_eq!(v["permissions"]["allow"], json!(["Bash(ls)"]));
    assert_eq!(v["synt"]["future"]["a"], 1);
    assert_eq!(v["synt"]["tui"]["x"], 2);
    assert_eq!(v["synt"]["memory"]["enabled"], false);
    let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["zzz", "permissions", "synt"]);
}

#[test]
fn wrong_values_are_errors_naming_the_key_and_write_nothing() {
    let f = Fx::new();
    f.write(&f.user(), json!({"a": 1}));
    let before = fs::read(f.user()).unwrap();
    let mut st = f.open(ProjectTrust::Untrusted);
    for (k, v) in [
        ("synt.tui.motion", json!("yes")),
        ("synt.tui.logo", json!("big")),
        ("synt.context.auto_compact_threshold", json!(0.99)),
        ("synt.context.auto_compact_threshold", json!("x")),
        ("synt.images.retentionDays", json!(3651)),
        ("synt.images.retentionDays", json!(-1)),
        ("synt.mcp.toolSearch", json!("maybe")),
        ("disabledMcpjsonServers", json!(["bad name"])),
        ("synt.memory.disabled_workspaces", json!("not a list")),
    ] {
        let e = st.set(k, v).unwrap_err();
        assert!(matches!(e, SetError::Invalid { .. }), "{k}: {e:?}");
        assert!(e.to_string().starts_with(k), "{e}");
    }
    assert_eq!(fs::read(f.user()).unwrap(), before);
    assert_eq!(
        st.set("nope.nothing", json!(1)),
        Err(SetError::UnknownKey("nope.nothing".into()))
    );
}

#[test]
fn a_parent_that_is_not_an_object_is_not_overwritten() {
    let f = Fx::new();
    f.write(&f.user(), json!({"synt": "oops"}));
    let before = fs::read(f.user()).unwrap();
    let mut st = f.open(ProjectTrust::Untrusted);
    assert!(st.set("synt.tui.motion", json!(false)).is_err());
    assert_eq!(fs::read(f.user()).unwrap(), before);
    // a malformed file is never replaced either
    fs::write(f.user(), "{ not json").unwrap();
    assert!(matches!(
        st.set("synt.tui.motion", json!(false)),
        Err(SetError::Io { .. })
    ));
    assert_eq!(fs::read_to_string(f.user()).unwrap(), "{ not json");
}

#[test]
fn no_temp_file_is_left_behind() {
    let f = Fx::new();
    let mut st = f.open(ProjectTrust::Untrusted);
    st.set("synt.tui.verbose", json!(true)).unwrap();
    let _ = st.set("synt.tui.verbose", json!(3));
    let names: Vec<_> = fs::read_dir(f.home.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["settings.json"]);
}

#[cfg(unix)]
#[test]
fn a_new_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fx::new();
    let mut st = f.open(ProjectTrust::Untrusted);
    st.set("synt.tui.verbose", json!(true)).unwrap();
    assert_eq!(
        fs::metadata(f.user()).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn secret_literals_are_refused() {
    let f = Fx::new();
    let mut st = f.open(ProjectTrust::Untrusted);
    for v in [
        json!(["sk-abcdefghijkl1234"]),
        json!(["ghp_abcdefghijkl1234"]),
    ] {
        let e = st.set("synt.memory.disabled_workspaces", v).unwrap_err();
        assert!(e.to_string().contains("credential"), "{e}");
    }
    let e = st
        .set("disabledMcpjsonServers", json!(["sk-abcdefghijkl1234"]))
        .unwrap_err();
    assert!(e.to_string().contains("credential"), "{e}");
    assert!(!f.user().exists());
}

#[test]
fn sensitive_keys_need_confirmation_and_some_are_never_editable() {
    let f = Fx::new();
    let mut st = f.open(ProjectTrust::Untrusted);
    assert!(matches!(
        st.set("synt.shell.user_commands_unsandboxed", json!(true)),
        Err(SetError::NeedsConfirmation { .. })
    ));
    assert!(!f.user().exists());
    st.set_confirmed("synt.shell.user_commands_unsandboxed", json!(true))
        .unwrap();
    for k in [
        "permissions.allow",
        "permissions.additionalDirectories",
        "hooks",
        "autoMode",
        "synt.mcp.env_passthrough",
        "enableAllProjectMcpServers",
        "synt.browser.secrets",
        "synt.verification.checks",
    ] {
        assert!(
            matches!(st.set(k, json!([])), Err(SetError::NotEditable { .. })),
            "{k}"
        );
        assert!(st.read_only_reason(k).is_some());
    }
}

#[test]
fn project_and_managed_scope() {
    let f = Fx::new();
    f.write(
        &f.proj.path().join(".synt/settings.json"),
        json!({
            "synt": {"tui": {"motion": false}, "context": {"auto_compact": false},
                     "budget": {"max_turns": 7}},
            "permissions": {"allow": ["Bash(x)"], "defaultMode": "plan"},
            "disabledMcpjsonServers": ["repo"]
        }),
    );
    f.write(
        &f.user(),
        json!({"synt": {"budget": {"max_turns": 3}}, "disabledMcpjsonServers": ["mine"]}),
    );
    f.write(
        &f.managed(),
        json!({"synt": {"mcp": {"toolSearch": "on"}}, "permissions": {"defaultMode": "dontAsk"}}),
    );
    let mut st = f.open(ProjectTrust::Untrusted);
    // user-only keys: the project copy is shown with the reason, the value is the default
    let m = st.entry("synt.tui.motion").unwrap();
    assert_eq!((&m.value, m.origin), (&json!(true), Origin::Default));
    assert_eq!(m.refused[0].0, Origin::Project);
    assert!(m.refused[0].1.contains("user settings file only"));
    // layered key from an untrusted project: refused; trusted: wins over user
    assert_eq!(st.entry("synt.budget.max_turns").unwrap().value, json!(3));
    assert_eq!(st.entry("synt.budget.max_turns").unwrap().refused.len(), 1);
    assert_eq!(st.entry("permissions.allow").unwrap().value, Value::Null);
    // tightening from an untrusted project is fine, lists are unions
    assert_eq!(
        st.entry("disabledMcpjsonServers").unwrap().value,
        json!(["mine", "repo"])
    );
    // managed beats user and locks the key
    let e = st.entry("permissions.defaultMode").unwrap();
    assert_eq!((&e.value, e.origin), (&json!("dontAsk"), Origin::Managed));
    assert_eq!(
        st.set_confirmed("permissions.defaultMode", json!("plan")),
        Err(SetError::Locked {
            key: "permissions.defaultMode".into()
        })
    );
    // a user-only key in the managed file is not read by the loader: shown refused, not locked
    assert_eq!(
        st.entry("synt.mcp.toolSearch").unwrap().refused[0].0,
        Origin::Managed
    );
    st.set("synt.mcp.toolSearch", json!("off")).unwrap();
    let t = f.open(ProjectTrust::Trusted);
    assert_eq!(t.entry("synt.budget.max_turns").unwrap().value, json!(7));
    assert_eq!(
        t.entry("synt.budget.max_turns").unwrap().origin,
        Origin::Project
    );
    assert_eq!(
        t.entry("synt.budget.max_turns").unwrap().user_value,
        Some(json!(3))
    );
    assert_eq!(
        t.entry("permissions.allow").unwrap().value,
        json!(["Bash(x)"])
    );
    // writes always go to the user file, never the project's
    let before = fs::read(f.proj.path().join(".synt/settings.json")).unwrap();
    st.set("synt.budget.max_turns", json!(9)).unwrap();
    assert_eq!(
        fs::read(f.proj.path().join(".synt/settings.json")).unwrap(),
        before
    );
    assert_eq!(f.read()["synt"]["budget"]["max_turns"], 9);
}

#[test]
fn loader_diagnostics_are_returned() {
    let f = Fx::new();
    f.write(
        &f.proj.path().join(".synt/settings.json"),
        json!({"synt": {"tui": {"motion": false}}}),
    );
    let mut st = f.open(ProjectTrust::Trusted);
    let d = st.set("synt.tui.verbose", json!(true)).unwrap();
    assert!(
        d.iter()
            .any(|m| m.contains("synt.tui is read from user settings only")),
        "{d:?}"
    );
}

#[test]
fn a_broken_file_is_reported() {
    let f = Fx::new();
    fs::write(f.user(), "{").unwrap();
    let st = f.open(ProjectTrust::Untrusted);
    assert_eq!(st.errors.len(), 1);
}

#[test]
fn every_key_belongs_to_a_named_group_of_its_tab() {
    let s = synthesis_settings_ui::SettingsSchema::new();
    for f in &s.fields {
        assert!(!f.group.is_empty(), "{} has no group", f.key);
        assert_eq!(f.group, synthesis_settings_ui::group_of(&f.key, f.section));
    }
    // the groups of a tab are few and each has rows (no empty card)
    for sec in synthesis_settings_ui::Section::ALL {
        let mut groups: Vec<&str> = s.section(sec).map(|f| f.group).collect();
        groups.dedup();
        let mut uniq = groups.clone();
        uniq.sort();
        uniq.dedup();
        assert!(uniq.len() <= 7, "{sec:?}: {uniq:?}");
    }
}
