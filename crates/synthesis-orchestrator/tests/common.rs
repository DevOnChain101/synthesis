#![allow(dead_code)]
//! Shared fixtures of the orchestrator tests: a real git repository, a real v5 store.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use synthesis_config::Env;
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::{AllocSpec, Allocator, Platform};
use synthesis_session::outcome::*;
use synthesis_session::{SCHEMA_VERSION_OUTCOMES, Store, StoreOptions};
use synthesis_workspace::{WorktreeManager, WorktreeOptions};
use time::OffsetDateTime;

#[macro_export]
macro_rules! need_git {
    () => {
        if Command::new("git").arg("--version").output().is_err() {
            println!("SKIP: git not available");
            return;
        }
    };
}

pub fn g(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

pub struct Rig {
    pub _tmp: tempfile::TempDir,
    pub base: PathBuf,
    pub repo: PathBuf,
    pub home: PathBuf,
    pub store: Store,
    pub wm: WorktreeManager,
    pub head: String,
}

pub fn policy() -> OutcomePolicy {
    OutcomePolicy::new(true, 16, 10, 24)
}

pub fn contract() -> Contract {
    let crit = |id: &str| ContractCriterion {
        id: id.into(),
        text: id.into(),
        mandatory: true,
        manual: false,
        required_trust: TrustClass::IndependentCheck,
        check_ids: vec![],
    };
    let mut caps = std::collections::BTreeMap::new();
    caps.insert(Capability::RepoRead, CapabilityMode::Allow);
    caps.insert(Capability::RepoWrite, CapabilityMode::Allow);
    Contract {
        version: 1,
        goal: "goal".into(),
        criteria: vec![crit("c1")],
        envelope: Envelope {
            allowed_paths: vec!["src/**".into()],
            denied_paths: vec![],
            capabilities: caps,
            budgets: Budgets::new(5.0),
            quality_gates: vec![],
        },
    }
}

pub fn plan() -> Plan {
    let item = |key: &str, kind, requires: &[&str], covers: &[&str]| PlanItem {
        key: key.into(),
        kind,
        requires: requires.iter().map(|s| (*s).into()).collect(),
        capabilities: vec![],
        covers: covers.iter().map(|s| (*s).into()).collect(),
    };
    Plan {
        version: 1,
        contract_version: 1,
        items: vec![
            item("one", WorkItemKind::Agent, &[], &[]),
            item("two", WorkItemKind::Agent, &[], &[]),
            item("test", WorkItemKind::Verify, &["one", "two"], &["c1"]),
        ],
    }
}

pub fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    g(&repo, &["init", "-q"]);
    fs::write(repo.join("a.txt"), "one\n").unwrap();
    g(&repo, &["add", "."]);
    g(&repo, &["commit", "-qm", "init"]);
    let head = g(&repo, &["rev-parse", "HEAD"]);
    let home = base.join("home");
    let store = Store::open(
        &home,
        StoreOptions {
            schema_target: SCHEMA_VERSION_OUTCOMES,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let mut o = WorktreeOptions::new(home.join("state"));
    o.worktrees_dir = Some(home.join("worktrees"));
    let wm = WorktreeManager::new(&Env::from_process(), o).unwrap();
    Rig {
        _tmp: tmp,
        base,
        repo,
        home,
        store,
        wm,
        head,
    }
}

impl Rig {
    /// The contract the outcomes of this rig are made with (for building profiles).
    pub fn contract_for_profile(&self) -> Contract {
        contract()
    }

    /// A running outcome with its plan stored and every work item still `created`.
    pub fn fresh(&self) -> (OutcomeWriter, Vec<(String, WorkItemId)>) {
        let ws = WorkspaceContext::open(
            "w",
            &self.repo,
            &self.repo,
            &[],
            self.base.join("cart"),
            "pg",
        )
        .unwrap();
        let mut w = self
            .store
            .create_outcome(NewOutcome {
                workspace: &ws,
                base_commit: self.head.clone(),
                contract: &contract(),
                policy: &policy(),
            })
            .unwrap();
        let go = |w: &mut OutcomeWriter, to| {
            let r = w.get_outcome(w.id()).unwrap().revision;
            w.transition_outcome(r, to, None, &Effects::default())
                .unwrap();
        };
        go(&mut w, OutcomeState::ContractPending);
        let h = w.get_contract(w.id(), 1).unwrap().1;
        w.approve_contract(1, &h, "user").unwrap();
        let items = w.insert_plan(&plan()).unwrap();
        go(&mut w, OutcomeState::Ready);
        go(&mut w, OutcomeState::Running);
        (w, items)
    }

    pub fn allocator(&self) -> Allocator<'_> {
        Allocator {
            store: &self.store,
            worktrees: &self.wm,
            platform: Platform {
                native_windows: false,
            },
            artifact_root: self.base.join("art"),
        }
    }

    /// A running outcome whose work items `one` and `two` are leased.
    pub fn outcome(&self) -> (OutcomeWriter, WorkItemId, WorkItemId) {
        self.outcome_in(&self.repo)
    }

    pub fn outcome_in(&self, root: &Path) -> (OutcomeWriter, WorkItemId, WorkItemId) {
        let ws =
            WorkspaceContext::open("w", root, root, &[], self.base.join("cart"), "pg").unwrap();
        let mut w = self
            .store
            .create_outcome(NewOutcome {
                workspace: &ws,
                base_commit: self.head.clone(),
                contract: &contract(),
                policy: &policy(),
            })
            .unwrap();
        let none = Effects::default();
        let go = |w: &mut OutcomeWriter, to| {
            let r = w.get_outcome(w.id()).unwrap().revision;
            w.transition_outcome(r, to, None, &none).unwrap();
        };
        go(&mut w, OutcomeState::ContractPending);
        let h = w.get_contract(w.id(), 1).unwrap().1;
        w.approve_contract(1, &h, "user").unwrap();
        let items = w.insert_plan(&plan()).unwrap();
        go(&mut w, OutcomeState::Ready);
        go(&mut w, OutcomeState::Running);
        let id = |k: &str| items.iter().find(|(n, _)| n == k).unwrap().1.clone();
        let (one, two) = (id("one"), id("two"));
        for it in [&one, &two] {
            for to in [WorkItemState::Ready, WorkItemState::Leased] {
                let r = w
                    .list_work_items(w.id())
                    .unwrap()
                    .into_iter()
                    .find(|i| i.id == *it)
                    .unwrap()
                    .revision;
                w.transition_work_item(it, r, to, &none).unwrap();
            }
        }
        (w, one, two)
    }

    pub fn spec<'a>(&'a self, item: &'a WorkItemId, ordinal: u32) -> AllocSpec<'a> {
        AllocSpec {
            item,
            ordinal,
            source: &self.repo,
            base_commit: &self.head,
            worker_kind: "local-inproc",
        }
    }

    pub fn worktrees(&self) -> usize {
        g(&self.repo, &["worktree", "list"]).lines().count()
    }

    pub fn branches(&self) -> Vec<String> {
        g(&self.repo, &["branch", "--list", "synthesis/*"])
            .lines()
            .map(|l| l.trim_start_matches(['+', '*', ' ']).to_string())
            .collect()
    }
}

pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

pub fn uuid_like(n: u8) -> String {
    format!(
        "{:08x}-0000-4000-8000-{:012x}",
        u32::from(n) + 0xabc0_0000,
        u64::from(n)
    )
}

pub fn intent(r: &Rig, item: &WorkItemId, ordinal: u32, sid: &str) -> NewOutbox {
    NewOutbox {
        idempotency_key: format!("worktree.create:{item}:{ordinal}"),
        kind: OutboxKind::WorktreeCreate,
        payload: serde_json::json!({
            "session_id": sid, "work_item": item, "ordinal": ordinal,
            "source": r.repo.to_str().unwrap(), "base_commit": r.head,
        }),
    }
}

impl Rig {
    pub fn raw(&self, sql: &str) {
        let c = rusqlite::Connection::open(self.home.join("synt.db")).unwrap();
        c.execute(sql, []).unwrap();
    }
    pub fn outcome_id_of(&self, _item: &WorkItemId) -> OutcomeId {
        self.store.reader().unwrap().list_outcomes().unwrap()[0]
            .id
            .clone()
    }
}
