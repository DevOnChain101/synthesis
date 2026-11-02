//! `synt import claude` against the synthetic fixture in tests/fixtures/import (no real data).
//! Every run has an empty environment except PATH/HOME/SYNT_HOME pointing into a temp dir.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const LITERALS: [&str; 6] = [
    "fakefakefake0123456789abcdef",
    "ghp_FAKEFAKEFAKEFAKEFAKE0123",
    "fakeQueryToken0123456789",
    "fakeDbPassw0rd",
    "fake-key-value",
    "fake-pass",
];

struct Fx {
    _t: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let (s, d) = (e.path(), to.join(e.file_name()));
        if e.file_type().unwrap().is_dir() {
            copy(&s, &d);
        } else {
            fs::copy(&s, &d).unwrap();
        }
    }
}

fn fixture() -> Fx {
    let t = tempfile::tempdir().unwrap();
    let root = synthesis_core::canonicalize(t.path()).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/import");
    let (home, project) = (root.join("home"), root.join("project"));
    copy(&src.join("home"), &home);
    copy(&src.join("project"), &project);
    let cj = home.join(".claude.json");
    let s = fs::read_to_string(&cj).unwrap();
    // JSON-escape the path: a Windows path has backslashes
    let quoted = serde_json::to_string(project.to_str().unwrap()).unwrap();
    fs::write(&cj, s.replace("\"__PROJECT__\"", &quoted)).unwrap();
    let key: String = project
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mem = home.join(".claude/projects").join(key).join("memory");
    fs::create_dir_all(&mem).unwrap();
    fs::write(mem.join("MEMORY.md"), "synthetic memory\n").unwrap();
    Fx {
        _t: t,
        home,
        project,
    }
}

fn synthesis(fx: &Fx, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &fx.home)
        .env("SYNT_HOME", fx.home.join(".synt"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn import(fx: &Fx, extra: &[&str]) -> Output {
    let mut a = vec![
        "import",
        "claude",
        "--project-dir",
        fx.project.to_str().unwrap(),
    ];
    a.extend_from_slice(extra);
    synthesis(fx, &a)
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn json(p: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
}

fn all_files(dir: &Path, v: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            all_files(&p, v);
        } else {
            v.push(p);
        }
    }
}

fn baks(dir: &Path) -> usize {
    let mut v = vec![];
    all_files(dir, &mut v);
    v.iter()
        .filter(|p| p.to_string_lossy().contains(".bak-"))
        .count()
}

#[test]
fn dry_run_is_default_and_writes_nothing_and_prints_no_secret() {
    let fx = fixture();
    let o = import(&fx, &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let s = out(&o);
    assert!(
        s.contains("DRY RUN") && s.contains("nothing written"),
        "{s}"
    );
    assert!(!fx.home.join(".synt").exists());
    assert!(!fx.project.join(".synt").exists() && !fx.project.join("AGENTS.md").exists());
    for l in LITERALS {
        assert!(!s.contains(l), "{l} printed");
    }
    assert!(s.contains("manual steps:") && s.contains("GH_TOKEN"));
}

#[test]
fn apply_is_idempotent_and_makes_no_second_backup() {
    let fx = fixture();
    let o = import(&fx, &["--apply", "--accept-hooks"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(fx.home.join(".synt/AGENTS.md").is_file());
    let first = fs::read_to_string(fx.home.join(".synt/AGENTS.md")).unwrap();
    assert!(first.starts_with("<!-- imported from ") && first.contains("by synt import claude"));
    assert!(first.contains("@imports/notes_md.md") && first.contains("@rules/style.md"));
    let o = import(&fx, &["--apply", "--accept-hooks"]);
    let s = out(&o);
    assert!(s.contains("0 file(s) written"), "{s}");
    assert!(s.contains("0 imported"), "{s}");
    assert_eq!(baks(&fx.home), 0);
    assert_eq!(
        fs::read_to_string(fx.home.join(".synt/AGENTS.md")).unwrap(),
        first
    );
}

#[test]
fn no_literal_secret_reaches_any_file_and_forbidden_env_is_skipped() {
    let fx = fixture();
    import(
        &fx,
        &[
            "--apply",
            "--accept-hooks",
            "--only",
            "instructions,permissions,hooks,env,agents,skills,commands,mcp,memory",
        ],
    );
    let mut files = vec![];
    all_files(&fx.home.join(".synt"), &mut files);
    all_files(&fx.project.join(".synt"), &mut files);
    for f in files {
        let t = fs::read_to_string(&f).unwrap();
        for l in LITERALS {
            // hooks are copied verbatim by design but the printed list masks them
            if !f.ends_with("settings.json") {
                assert!(!t.contains(l), "{l} in {}", f.display());
            }
        }
    }
    let n = json(&fx.home.join(".synt/synt.json"));
    assert_eq!(n["$schema"], "config.json/v1");
    assert_eq!(
        n["mcpServers"]["gh"]["headers"]["Authorization"],
        "Bearer ${GH_TOKEN}"
    );
    assert_eq!(n["mcpServers"]["db"]["env"]["DB_TOKEN"], "${DB_TOKEN}");
    assert_eq!(n["mcpServers"]["db"]["env"]["REF"], "${REF_VAR}");
    assert_eq!(n["mcpServers"]["db"]["args"][1], "${DB_PASSWORD}");
    assert!(
        n["mcpServers"]["legacy"]["url"]
            .as_str()
            .unwrap()
            .contains("token=${LEGACY_TOKEN}")
    );
    assert!(n["mcpServers"]["cloud"].is_object());
    let proj_key = fx.project.to_str().unwrap();
    assert!(n["projects"][proj_key]["mcpServers"]["localsrv"].is_object());
    let st = json(&fx.home.join(".synt/settings.json"));
    let env = st["env"].as_object().unwrap();
    assert_eq!(env.len(), 2, "{env:?}");
    assert!(env.contains_key("SAFE_FLAG") && env.contains_key("EDITOR_THEME"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = fs::metadata(fx.home.join(".synt/synt.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }
}

#[test]
fn bypass_and_auto_modes_are_never_imported_and_broad_allow_is_guarded() {
    let fx = fixture();
    import(&fx, &["--apply"]);
    let st = json(&fx.home.join(".synt/settings.json"));
    assert!(st["permissions"].get("defaultMode").is_none());
    let allow: Vec<&str> = st["permissions"]["allow"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        allow.contains(&"Bash(npm test:*)")
            && allow.contains(&"Read(//tmp/**)")
            && allow.contains(&"mcp__srv__tool")
    );
    assert!(!allow.contains(&"Bash") && !allow.iter().any(|r| r.starts_with("PowerShell")));
    assert_eq!(st["permissions"]["deny"].as_array().unwrap().len(), 2);
    assert_eq!(st["permissions"]["ask"][0], "Bash(git push:*)");
    let ps = json(&fx.project.join(".synt/settings.json"));
    assert!(ps["permissions"].get("defaultMode").is_none());
    assert_eq!(ps["permissions"]["allow"][0], "Bash(make:*)");
    assert!(fx.project.join(".synt/settings.local.json").is_file());
    let gi = fs::read_to_string(fx.project.join(".synt/.gitignore")).unwrap();
    assert!(gi.contains("settings.local.json"));
    // with the flag the broad rule arrives
    let fx2 = fixture();
    import(&fx2, &["--apply", "--accept-broad-allow"]);
    let st = json(&fx2.home.join(".synt/settings.json"));
    assert!(
        st["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "Bash")
    );
}

#[test]
fn user_hooks_need_accept_hooks_and_unsupported_events_are_dropped() {
    let fx = fixture();
    let o = import(&fx, &["--apply"]);
    let s = out(&o);
    assert!(
        s.contains("--accept-hooks") && s.contains("SessionStart: echo start"),
        "{s}"
    );
    assert!(s.contains("check.sh --token ****"));
    assert!(
        json(&fx.home.join(".synt/settings.json"))
            .get("hooks")
            .is_none()
    );
    let fx = fixture();
    import(&fx, &["--apply", "--accept-hooks"]);
    let h = json(&fx.home.join(".synt/settings.json"))["hooks"].clone();
    assert!(h.get("SessionStart").is_some() && h.get("PreToolUse").is_some());
    assert!(h.get("SubagentStart").is_none());
    assert!(
        h["SessionStart"][0]["hooks"][0]
            .get("statusMessage")
            .is_none()
    );
    assert!(json(&fx.project.join(".synt/settings.json"))["hooks"]["PostToolUse"].is_array());
}

#[test]
fn agents_skills_commands_are_converted() {
    let fx = fixture();
    import(&fx, &["--apply"]);
    let a = fs::read_to_string(fx.home.join(".synt/agents/reviewer.md")).unwrap();
    assert!(
        a.contains("tools: Read, Grep, Glob, Edit, Task, Bash(git diff:*)"),
        "{a}"
    );
    assert!(a.contains("model: sonnet") && a.contains("Be strict."));
    assert!(!fx.home.join(".synt/agents/NOTES.txt").exists());
    let s = fs::read_to_string(fx.home.join(".synt/skills/demo/SKILL.md")).unwrap();
    assert!(s.contains("allowed-tools: Read, Bash(ls:*)\n"), "{s}");
    assert!(
        fx.home
            .join(".synt/skills/demo/references/guide.md")
            .is_file()
    );
    assert!(fx.home.join(".synt/commands/team/deploy.md").is_file());
    let c = fs::read_to_string(fx.home.join(".synt/commands/ship.md")).unwrap();
    assert!(c.contains("allowed-tools: Bash(git add:*), Bash(git commit:*)"));
    let r = fs::read_to_string(fx.home.join(".synt/rules/style.md")).unwrap();
    assert_eq!(r.trim(), "Use rustfmt.");
    // memory is opt-in
    assert!(!fx.home.join(".synt/projects").exists());
    import(&fx, &["--apply", "--only", "memory"]);
    assert!(fx.home.join(".synt/projects").is_dir());
}

#[test]
fn project_instructions_and_scopes() {
    let fx = fixture();
    let o = import(&fx, &["--scope", "user", "--apply"]);
    assert!(o.status.success());
    assert!(!fx.project.join(".synt").exists() && !fx.project.join("AGENTS.md").exists());
    let o = import(&fx, &["--scope", "project", "--apply"]);
    assert!(o.status.success());
    let n = fs::read_to_string(fx.project.join("AGENTS.md")).unwrap();
    assert!(n.contains("Run tests with make test.") && n.contains("@.synt/rules/api.md"));
    assert!(fx.project.join(".synt/rules/api.md").is_file());
    // project .mcp.json is only reported: unchanged and approval hint given
    let s = out(&o);
    assert!(
        s.contains("--approve-mcp team@") && s.contains("holds a literal secret"),
        "{s}"
    );
    assert!(
        fs::read_to_string(fx.project.join(".mcp.json"))
            .unwrap()
            .contains("ghp_FAKE")
    );
    let o = synthesis(&fx, &["import", "claude", "--scope", "project"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn merge_keeps_existing_values_and_backs_up() {
    let fx = fixture();
    let nh = fx.home.join(".synt");
    fs::create_dir_all(&nh).unwrap();
    fs::write(
        nh.join("settings.json"),
        r#"{"permissions":{"allow":["Read(./mine)"],"defaultMode":"plan"},"keep":1}"#,
    )
    .unwrap();
    fs::write(nh.join("AGENTS.md"), "my own notes\n").unwrap();
    let o = import(&fx, &["--apply", "--only", "permissions,instructions"]);
    let s = out(&o);
    let st = json(&nh.join("settings.json"));
    assert_eq!(st["keep"], 1);
    assert_eq!(st["permissions"]["defaultMode"], "plan");
    let allow = st["permissions"]["allow"].as_array().unwrap();
    assert_eq!(allow[0], "Read(./mine)");
    assert!(allow.len() > 1);
    assert_eq!(baks(&nh), 1, "{s}");
    let bak = fs::read_dir(&nh)
        .unwrap()
        .flatten()
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("settings.json.bak-")
        })
        .unwrap();
    assert!(
        fs::read_to_string(bak.path())
            .unwrap()
            .contains("\"keep\":1")
    );
    // a hand-written AGENTS.md is not replaced without --overwrite
    assert_eq!(
        fs::read_to_string(nh.join("AGENTS.md")).unwrap(),
        "my own notes\n"
    );
    assert!(s.contains("was not written by an import"), "{s}");
    import(&fx, &["--apply", "--only", "instructions", "--overwrite"]);
    assert!(
        fs::read_to_string(nh.join("AGENTS.md"))
            .unwrap()
            .starts_with("<!-- imported from")
    );
    assert!(baks(&nh) >= 2);
    // skip-existing never touches an existing file
    let before = fs::read_to_string(nh.join("settings.json")).unwrap();
    import(
        &fx,
        &["--apply", "--skip-existing", "--only", "permissions"],
    );
    assert_eq!(
        fs::read_to_string(nh.join("settings.json")).unwrap(),
        before
    );
}

#[test]
fn malformed_claude_settings_fail_clearly_without_partial_writes() {
    let fx = fixture();
    fs::write(fx.project.join(".claude/settings.json"), "{ not json").unwrap();
    let o = import(&fx, &["--apply", "--accept-hooks"]);
    assert_eq!(o.status.code(), Some(2));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(
        e.contains("malformed JSON") && e.contains("settings.json"),
        "{e}"
    );
    assert!(!e.contains("not json"));
    assert!(!fx.home.join(".synt").exists(), "partial write");
    assert!(!fx.project.join(".synt").exists() && !fx.project.join("AGENTS.md").exists());
}

#[test]
fn malformed_existing_synthesis_file_aborts_before_any_write() {
    let fx = fixture();
    let nh = fx.home.join(".synt");
    fs::create_dir_all(&nh).unwrap();
    fs::write(nh.join("synt.json"), "[1]").unwrap();
    let o = import(&fx, &["--apply"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(!nh.join("AGENTS.md").exists() && !nh.join("settings.json").exists());
    assert_eq!(fs::read_to_string(nh.join("synt.json")).unwrap(), "[1]");
}

#[test]
fn doctor_reports_the_import_and_config_stays_valid() {
    let fx = fixture();
    import(&fx, &["--apply", "--accept-hooks"]);
    let o = synthesis(&fx, &["doctor", "--cwd", fx.project.to_str().unwrap()]);
    let s = out(&o);
    assert!(!s.contains("config: INVALID"), "{s}");
    assert!(
        s.contains("import (claude, user)") && s.contains("agents 1"),
        "{s}"
    );
    assert!(s.contains("instruction files: user AGENTS.md"), "{s}");
}
