//! The file-edit tools (not only the shell sandbox) cannot touch Synthesis's own state: the session database
//! (where remembered trust lives), its journal and the `run/` folder with the trust key. Built the way
//! `run.rs` builds the session's permission environment: `synthesis_state_paths` as both the read-deny list and
//! the protected paths.

use serde_json::json;
use synthesis_permissions::{Decision, Env, Mode, PermissionRules, Permissions};

#[test]
fn write_edit_multiedit_and_notebookedit_are_denied_on_the_trust_store_and_key_in_every_mode() {
    let d = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(d.path()).unwrap();
    let (cwd, user, state) = (base.join("ws"), base.join("user"), base.join("user/.synt"));
    for p in [&cwd, &user, &state.join("run")] {
        std::fs::create_dir_all(p).unwrap();
    }
    let paths = synthesis_sandbox::synthesis_state_paths(&state);
    let targets = [
        state.join("synt.db"),
        state.join("synt.db-wal"),
        state.join("synt.db-journal"),
        state.join("chain.key"),
        state.join("run/trust.key"),
        state.join("run/new-file"),
    ];
    for mode in Mode::ALL {
        let env = Env::new(&cwd, &user, mode)
            .interactive(true)
            .deny_reads(paths.clone())
            .protect(paths.clone());
        let p = Permissions::new(PermissionRules::default(), env);
        for t in &targets {
            let f = t.to_string_lossy().into_owned();
            for (tool, input) in [
                ("Write", json!({"file_path": f, "content": "x"})),
                (
                    "Edit",
                    json!({"file_path": f, "old_string": "a", "new_string": "b"}),
                ),
                (
                    "MultiEdit",
                    json!({"file_path": f, "edits": [{"old_string": "a", "new_string": "b"}]}),
                ),
                (
                    "NotebookEdit",
                    json!({"notebook_path": f, "new_source": "x"}),
                ),
                ("Read", json!({"file_path": f})),
            ] {
                let got = p.check_current(tool, &input).decision;
                assert_eq!(got, Decision::Deny, "{tool} on {} in {mode:?}", t.display());
            }
        }
    }
}
