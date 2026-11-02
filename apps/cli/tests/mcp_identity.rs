//! "Writes show up as anonymous", end to end through the real binary with a scripted model and
//! `synthesis-mcp/examples/atlassian_like_server`: the credentials in the server's own `env` (user
//! scope, and project `.mcp.json` trusted + approved by hash) must reach the server, the write must
//! be attributed to that identity, and a missing credential must be visible (warning, `mcp test`).
#![cfg(all(feature = "test-provider", unix))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

fn server() -> PathBuf {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let exe = Path::new(env!("CARGO_BIN_EXE_synt"));
        let profile = exe.parent().unwrap();
        let p = profile.join("examples").join("atlassian_like_server");
        let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        c.args([
            "build",
            "-p",
            "synthesis-mcp",
            "--example",
            "atlassian_like_server",
        ]);
        if profile.file_name().is_some_and(|n| n == "release") {
            c.arg("--release");
        }
        assert!(c.status().unwrap().success(), "cannot build the example");
        p
    })
    .clone()
}

struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    /// `user_servers` is the `mcpServers` object of the user synt.json, `settings` the user
    /// settings.json, `project` an optional `.mcp.json` `mcpServers` object for the workspace.
    fn new(user_servers: Value, settings: Value, project: Option<Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        std::fs::create_dir_all(dir.path().join("synthesis")).unwrap();
        let cfg = json!({"$schema": "config.json/v1", "mcpServers": user_servers});
        std::fs::write(dir.path().join("synthesis/synt.json"), cfg.to_string()).unwrap();
        std::fs::write(
            dir.path().join("synthesis/settings.json"),
            settings.to_string(),
        )
        .unwrap();
        if let Some(p) = project {
            std::fs::write(
                dir.path().join("ws/.mcp.json"),
                json!({"mcpServers": p}).to_string(),
            )
            .unwrap();
        }
        Rig { dir }
    }

    fn cmd(&self, extra_env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_synt"));
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("SYNT_HOME", self.dir.path().join("synthesis"))
            .stdin(Stdio::null());
        for (k, v) in extra_env {
            c.env(k, v);
        }
        c
    }

    fn ws(&self) -> PathBuf {
        self.dir.path().join("ws")
    }

    /// Returns the tool results (`name`, `is_error`, full text from the transcript mirror).
    fn run(&self, script: Value, flags: &[&str], extra_env: &[(&str, &str)]) -> (Output, String) {
        let s = self.dir.path().join("script.json");
        std::fs::write(&s, script.to_string()).unwrap();
        let o = self
            .cmd(extra_env)
            .args(["run", "--cwd"])
            .arg(self.ws())
            .arg("--provider")
            .arg(format!("fake:{}", s.display()))
            .args(["--prompt", "update the page", "--sandbox", "best-effort"])
            .args(flags)
            .output()
            .unwrap();
        let mut mirror = String::new();
        if let Ok(rd) = std::fs::read_dir(self.dir.path().join("synthesis/projects")) {
            for d in rd.flatten() {
                for f in std::fs::read_dir(d.path()).unwrap().flatten() {
                    mirror += &std::fs::read_to_string(f.path()).unwrap();
                }
            }
        }
        (o, mirror.replace("\\\"", "\"").replace("\\n", "\n"))
    }

    fn synthesis(&self, args: &[&str], extra_env: &[(&str, &str)]) -> (i32, String) {
        let o = self.cmd(extra_env).args(args).output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        )
    }
}

fn atl(env: Value) -> Value {
    json!({"atl": {"command": server(), "args": ["--no-annotations"], "env": env}})
}

fn allow() -> Value {
    json!({"permissions": {"allow": ["mcp__atl"]}})
}

fn call(name: &str, input: Value) -> Value {
    json!({"tools": [{"name": format!("mcp__atl__{name}"), "input": input}]})
}

/// whoami, read, then a WRITE (version 4 is current + 1 for the seeded page).
fn write_script() -> Value {
    json!([
        call("whoami", json!({})),
        call("getPage", json!({"id": "100"})),
        call("updatePage", json!({"id": "100", "title": "Runbook", "version": {"number": 4},
            "body": "# Runbook\n\n## Rollback\nStep 1: stop the deploy.\nStep 2: restore the snapshot.\n\n## Contacts\nOn call: alice@example.com\nEscalation: bob@example.com\n\n## History\n2024-01-05 first version\n2024-02-11 added contacts\n"})),
        {"text": "done"}
    ])
}

#[test]
fn user_scope_env_credentials_attribute_the_write_to_the_configured_user() {
    // literal username, token from the process env through the user's passthrough list
    let rig = Rig::new(
        atl(json!({"JIRA_USERNAME": "ana@example.com", "JIRA_API_TOKEN": "${ATL_TOKEN}"})),
        json!({"permissions": {"allow": ["mcp__atl"]}, "synt": {"mcp": {"env_passthrough": ["ATL_TOKEN"]}}}),
        None,
    );
    let (o, mirror) = rig.run(write_script(), &[], &[("ATL_TOKEN", "api-token-123")]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        mirror.contains(r#""principal":"ana@example.com""#),
        "{mirror}"
    );
    assert!(mirror.contains(r#""author":"ana@example.com""#), "{mirror}");
    assert!(!mirror.contains("anonymous"), "{mirror}");
    assert!(
        !mirror.contains("api-token-123"),
        "the token never reaches the transcript"
    );
}

#[test]
fn a_missing_credential_is_loud_the_server_does_not_start_unauthenticated() {
    let rig = Rig::new(
        atl(json!({"JIRA_USERNAME": "ana@example.com", "JIRA_API_TOKEN": "${ATL_TOKEN}"})),
        allow(),
        None,
    );
    // ATL_TOKEN is exported but NOT listed in env_passthrough
    let (o, mirror) = rig.run(write_script(), &[], &[("ATL_TOKEN", "api-token-123")]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("ATL_TOKEN") && err.contains("synt.mcp.env_passthrough"),
        "{err}"
    );
    assert!(
        mirror.contains(r#"Unknown tool "mcp__atl__whoami""#) && !mirror.contains("author"),
        "no server, no unauthenticated write: {mirror}"
    );
}

#[test]
fn an_anonymous_write_is_flagged_next_to_the_result() {
    // no credentials in the env: the server answers anonymous and synthesis says so
    let rig = Rig::new(atl(json!({})), allow(), None);
    let (o, mirror) = rig.run(write_script(), &[], &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(mirror.contains(r#""author":"anonymous""#), "{mirror}");
    assert!(
        mirror.contains("reports an anonymous principal") && mirror.contains("synt mcp test atl"),
        "{mirror}"
    );
}

#[test]
fn project_mcp_json_trusted_and_approved_by_hash_keeps_its_env_credentials() {
    let rig = Rig::new(
        json!({}),
        allow(),
        Some(atl(
            json!({"JIRA_USERNAME": "proj@example.com", "JIRA_API_TOKEN": "tok-9"}),
        )),
    );
    // `mcp get` prints the hash of the (not yet approved) definition
    let (code, got) = rig.synthesis(
        &["mcp", "get", "atl", "--cwd", rig.ws().to_str().unwrap()],
        &[],
    );
    assert_eq!(code, 0, "{got}");
    assert!(!got.contains("tok-9"), "{got}");
    let hash = got
        .split("hash=")
        .nth(1)
        .and_then(|h| h.get(..24))
        .expect("hash in the definition")
        .to_string();
    let approve = format!("atl@{hash}");
    let (o, mirror) = rig.run(
        write_script(),
        &["--trust-project", "--approve-mcp", &approve],
        &[],
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        mirror.contains(r#""author":"proj@example.com""#),
        "{mirror}"
    );
    assert!(!mirror.contains("anonymous"), "{mirror}");

    // the user's own definition of the same name replaces the project one entirely
    let user = atl(json!({"JIRA_USERNAME": "ana@example.com", "JIRA_API_TOKEN": "tok-1"}));
    std::fs::write(
        rig.dir.path().join("synthesis/synt.json"),
        json!({"$schema": "config.json/v1", "mcpServers": user}).to_string(),
    )
    .unwrap();
    std::fs::remove_dir_all(rig.dir.path().join("synthesis/projects")).unwrap(); // first run's transcript
    let (_, mirror) = rig.run(
        write_script(),
        &["--trust-project", "--approve-mcp", &approve],
        &[],
    );
    assert!(mirror.contains(r#""author":"ana@example.com""#), "{mirror}");
    assert!(!mirror.contains("proj@example.com"), "{mirror}");
}

#[test]
fn mcp_get_doctor_and_test_show_the_identity_without_values() {
    let rig = Rig::new(
        atl(
            json!({"JIRA_USERNAME": "ana@example.com", "JIRA_API_TOKEN": "${ATL_TOKEN}",
            "CONFLUENCE_TOKEN": "${CONFLUENCE_TOKEN}"}),
        ),
        json!({"synt": {"mcp": {"env_passthrough": ["ATL_TOKEN"]}}}),
        None,
    );
    let ws = rig.ws();
    let env = [("ATL_TOKEN", "api-token-123")];
    let (_, get) = rig.synthesis(&["mcp", "get", "atl"], &env);
    assert!(
        get.contains("JIRA_API_TOKEN=set") && get.contains("JIRA_USERNAME=set(15 chars)"),
        "{get}"
    );
    assert!(
        get.contains(
            "CONFLUENCE_TOKEN -> NOT AVAILABLE (export it and add it to synt.mcp.env_passthrough"
        ),
        "{get}"
    );
    assert!(get.contains("ATL_TOKEN -> available"), "{get}");
    assert!(!get.contains("api-token-123"), "{get}");
    let (_, doc) = rig.synthesis(&["doctor", "--cwd", ws.to_str().unwrap()], &env);
    assert!(
        doc.contains("JIRA_API_TOKEN=set") && !doc.contains("api-token-123"),
        "{doc}"
    );

    // `mcp test` against a server whose CONFLUENCE_TOKEN cannot resolve: refuses to start it
    let (code, t) = rig.synthesis(&["mcp", "test", "atl"], &env);
    assert_eq!(code, 1, "{t}");
    assert!(
        t.contains("FAILED") && t.contains("CONFLUENCE_TOKEN"),
        "{t}"
    );

    // a working definition: connect, list, whoami
    let ok = atl(json!({"JIRA_USERNAME": "ana@example.com", "JIRA_API_TOKEN": "${ATL_TOKEN}"}));
    std::fs::write(
        rig.dir.path().join("synthesis/synt.json"),
        json!({"$schema": "config.json/v1", "mcpServers": ok}).to_string(),
    )
    .unwrap();
    let (code, t) = rig.synthesis(&["mcp", "test", "atl"], &env);
    assert_eq!(code, 0, "{t}");
    assert!(
        t.contains("connected via stdio") && t.contains("identity (whoami)"),
        "{t}"
    );
    assert!(
        t.contains("ana@example.com") && !t.contains("api-token-123"),
        "{t}"
    );

    // no credentials: the principal is anonymous and the command says so (exit 1)
    let bare = atl(json!({}));
    std::fs::write(
        rig.dir.path().join("synthesis/synt.json"),
        json!({"$schema": "config.json/v1", "mcpServers": bare}).to_string(),
    )
    .unwrap();
    let (code, t) = rig.synthesis(&["mcp", "test", "atl"], &env);
    assert_eq!(code, 1, "{t}");
    assert!(t.contains("WARNING") && t.contains("anonymous"), "{t}");
}
