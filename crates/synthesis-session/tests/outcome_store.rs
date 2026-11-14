//! Outcome persistence (ADR-016): opt-in v5 migration, revisions, commands, outbox, and the
//! security properties of the store (approvals, derived guards, immutability). Real SQLite in a
//! temp dir; canonical temp paths (macOS `/var` -> `/private/var`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synthesis_core::outcome::*;
use synthesis_core::{ParentRef, PermissionMode, TaskStatus, WorkspaceContext};
use synthesis_events::Budget;
use synthesis_session::outcome::*;
use synthesis_session::{
    CleanScope, Error, NewSession, Retention, SCHEMA_VERSION_LATEST, SCHEMA_VERSION_OUTCOMES,
    Store, StoreOptions,
};
use time::OffsetDateTime;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    Env {
        home: base.join("home"),
        repo,
        _tmp: tmp,
    }
}

fn policy() -> OutcomePolicy {
    OutcomePolicy {
        enabled: true,
        max_parallel_workers: 16,
        max_attempts_per_work_item: 10,
        max_cost_usd: None,
        attempt_max_cost_usd: None,
        attempt_max_wall_secs: None,
    }
}

impl Env {
    fn v4(&self) -> Store {
        Store::open(&self.home, StoreOptions::default()).unwrap()
    }
    fn v5(&self) -> Store {
        Store::open(
            &self.home,
            StoreOptions {
                schema_target: SCHEMA_VERSION_OUTCOMES,
                ..StoreOptions::default()
            },
        )
        .unwrap()
    }
    fn ws(&self) -> WorkspaceContext {
        WorkspaceContext::open(
            "w1",
            &self.repo,
            &self.repo,
            &[],
            self.repo.join(".art"),
            "pg",
        )
        .unwrap()
    }
    fn db(&self) -> Connection {
        let c = Connection::open(self.home.join("synt.db")).unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        c
    }
    fn version(&self) -> i64 {
        self.db()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }
    fn new_session<'a>(
        &self,
        id: &str,
        ws: &'a WorkspaceContext,
        parent: Option<&str>,
    ) -> NewSession<'a> {
        NewSession {
            id: Some(id.into()),
            workspace: ws,
            model_config_id: "m".into(),
            permission_mode: PermissionMode::Default,
            permission_policy_id: "default".into(),
            budget: Budget::default(),
            parent: parent.map(|p| ParentRef {
                session_id: p.into(),
                tool_call_id: String::new(),
            }),
            title: None,
        }
    }
    /// A plain chat session (the writer is dropped, so its lock is free).
    fn session(&self, store: &Store, id: &str, with_message: bool) {
        let mut w = store
            .create_session(self.new_session(id, &self.ws(), None))
            .unwrap();
        if with_message {
            w.append_message(synthesis_session::NewMessage {
                role: synthesis_session::Role::User,
                content: json!([{"type": "text", "text": "hi"}]),
                top: Default::default(),
                msg: Default::default(),
                compacts_previous: false,
            })
            .unwrap();
        }
    }
    /// An attempt's session: an ordinary child of the control session.
    fn child(&self, store: &Store, id: &str, control: &str) {
        drop(
            store
                .create_session(self.new_session(id, &self.ws(), Some(control)))
                .unwrap(),
        );
    }
}

fn contract() -> Contract {
    let crit = |id: &str| ContractCriterion {
        id: id.into(),
        text: format!("criterion {id}"),
        mandatory: true,
        manual: false,
        required_trust: TrustClass::IndependentCheck,
        check_ids: vec![],
    };
    let mut caps = BTreeMap::new();
    caps.insert(Capability::RepoRead, CapabilityMode::Allow);
    caps.insert(Capability::RepoWrite, CapabilityMode::Allow);
    caps.insert(Capability::TestsRun, CapabilityMode::Allow);
    Contract {
        version: 1,
        goal: "implement the endpoint".into(),
        criteria: vec![crit("c1"), crit("c2")],
        envelope: Envelope {
            allowed_paths: vec!["src/**".into()],
            denied_paths: vec!["infra/**".into()],
            capabilities: caps,
            budgets: Budgets::new(5.0),
            quality_gates: vec![],
        },
    }
}

fn plan() -> Plan {
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
            item("inspect", WorkItemKind::Agent, &[], &[]),
            item("implement", WorkItemKind::Agent, &["inspect"], &["c1"]),
            item("test", WorkItemKind::Verify, &["implement"], &["c1", "c2"]),
        ],
    }
}

fn new_outcome_with(e: &Env, store: &Store, c: &Contract, p: &OutcomePolicy) -> OutcomeWriter {
    store
        .create_outcome(NewOutcome {
            workspace: &e.ws(),
            base_commit: "c0ffee".into(),
            contract: c,
            policy: p,
        })
        .unwrap()
}

fn new_outcome(e: &Env, store: &Store) -> OutcomeWriter {
    new_outcome_with(e, store, &contract(), &policy())
}

fn contract_hash(w: &OutcomeWriter) -> String {
    w.get_contract(w.id(), 1).unwrap().1
}

const NONE: &Effects = &Effects {
    events: Vec::new(),
    outbox: Vec::new(),
};

fn rev_of(w: &OutcomeWriter) -> i64 {
    w.get_outcome(w.id()).unwrap().revision
}

fn go(w: &mut OutcomeWriter, to: OutcomeState) -> Applied {
    let r = rev_of(w);
    w.transition_outcome(r, to, None, NONE).unwrap()
}

/// Outcome at `contract_pending` with its contract approved (no plan yet).
fn approved_outcome(e: &Env, store: &Store) -> OutcomeWriter {
    let mut w = new_outcome(e, store);
    go(&mut w, OutcomeState::ContractPending);
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    w
}

/// Outcome at `ready` with a plan: contract approved, plan stored, state walked there.
fn ready_outcome(e: &Env, store: &Store) -> (OutcomeWriter, Vec<(String, WorkItemId)>) {
    let mut w = approved_outcome(e, store);
    let items = w.insert_plan(&plan()).unwrap();
    go(&mut w, OutcomeState::Ready);
    (w, items)
}

fn item<'a>(items: &'a [(String, WorkItemId)], key: &str) -> &'a WorkItemId {
    &items.iter().find(|(k, _)| k == key).unwrap().1
}

fn kinds(w: &OutcomeWriter) -> Vec<String> {
    use synthesis_events::Record;
    w.read_events(w.control_session_id(), 0, 1000)
        .unwrap()
        .into_iter()
        .map(|r| match r {
            Record::Unknown(raw) => raw.kind,
            Record::Known(env) => format!("typed:{:?}", env.event),
        })
        .collect()
}

fn item_rev(w: &OutcomeWriter, id: &WorkItemId) -> i64 {
    w.list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.id == *id)
        .unwrap()
        .revision
}

fn item_to(w: &mut OutcomeWriter, id: &WorkItemId, to: WorkItemState) -> Result<Applied, Error> {
    let r = item_rev(w, id);
    w.transition_work_item(id, r, to, NONE)
}

fn evidence(
    item: &WorkItemId,
    attempt: Option<&AttemptRecord>,
    commit: &str,
    crit: &[&str],
) -> NewEvidence {
    NewEvidence {
        work_item_id: item.clone(),
        attempt_id: attempt.map(|a| a.id.clone()),
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: commit.into(),
        check_id: "unit".into(),
        command_hash: None,
        criteria_ids: crit.iter().map(|s| (*s).into()).collect(),
        result: "passed".into(),
        exit_code: Some(0),
        env_fingerprint: "fp".into(),
        artifact: None,
        fence: attempt.map_or(0, |a| a.fence),
    }
}

/// Runs one agent item through an attempt that ends with `commit`, up to `verifying`.
fn run_agent(
    e: &Env,
    store: &Store,
    w: &mut OutcomeWriter,
    it: &WorkItemId,
    sess: &str,
    commit: &str,
) -> AttemptRecord {
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
    ] {
        item_to(w, it, to).unwrap();
    }
    e.child(store, sess, w.control_session_id());
    let (a, _) = w.new_attempt(it, 1, sess, "local-inproc", NONE).unwrap();
    let r = w
        .transition_attempt(
            &a.id,
            a.revision,
            AttemptState::Running,
            &AttemptEnd::default(),
            NONE,
        )
        .unwrap()
        .revision;
    let end = AttemptEnd {
        task_status: Some(TaskStatus::Completed),
        subject_commit: Some(commit.into()),
        retained_reason: None,
    };
    w.transition_attempt(&a.id, r, AttemptState::Finished, &end, NONE)
        .unwrap();
    item_to(w, it, WorkItemState::Verifying).unwrap();
    w.list_attempts(it).unwrap().remove(0)
}

/// The whole MVP flow up to (not including) the outcome's `verifying`: every item `succeeded`
/// with independent evidence. Returns the items and the candidate commit.
fn finish_items(
    e: &Env,
    store: &Store,
    w: &mut OutcomeWriter,
    items: &[(String, WorkItemId)],
) -> String {
    let (inspect, implement, test) = (
        item(items, "inspect"),
        item(items, "implement"),
        item(items, "test"),
    );
    go(w, OutcomeState::Running);
    let a1 = run_agent(e, store, w, inspect, "att-inspect", "base0");
    w.record_evidence(&evidence(inspect, Some(&a1), "base0", &[]))
        .unwrap();
    item_to(w, inspect, WorkItemState::Succeeded).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5)); // ended_at orders the candidate
    let a2 = run_agent(e, store, w, implement, "att-implement", "cafe01");
    w.record_evidence(&evidence(implement, Some(&a2), "cafe01", &["c1"]))
        .unwrap();
    item_to(w, implement, WorkItemState::Succeeded).unwrap();
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
        WorkItemState::Verifying,
    ] {
        item_to(w, test, to).unwrap();
    }
    w.record_evidence(&evidence(test, None, "cafe01", &["c1", "c2"]))
        .unwrap();
    item_to(w, test, WorkItemState::Succeeded).unwrap();
    "cafe01".into()
}

fn hours(h: i64) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    (OffsetDateTime::now_utc() + time::Duration::hours(h))
        .format(&fmt)
        .unwrap()
}

// ------------------------------------------------------------ opt-in migration

/// Digest of `sqlite_master` (type, name, table, sql) minus SQLite's own tables.
fn schema_digest(c: &Connection) -> String {
    let mut st = c
        .prepare(
            "SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    let rows: Vec<String> = st
        .query_map([], |r| {
            Ok(format!(
                "{}|{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    Sha256::digest(rows.join("\n").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The same digest computed over migrations 1-4 as they were at origin/main 66e2c424 (an
/// independent SQLite ran the old DDL). A change to an applied migration, or any v5 leak into a
/// default open, changes it.
const V4_SCHEMA_DIGEST: &str = "2161f25b9584825d6dba77cd63f81864b9d024182f09c6e1c48d1b72505ef0a7";

#[test]
fn a_default_open_stays_at_v4_with_the_exact_old_schema_and_existing_flows_work() {
    let e = env();
    let store = e.v4();
    assert_eq!(e.version(), SCHEMA_VERSION_LATEST);
    assert_eq!(e.version(), 4);
    assert_eq!(schema_digest(&e.db()), V4_SCHEMA_DIGEST);
    assert!(store.migration().is_none());

    // the existing flows, on a v4 store
    e.session(&store, "s-used", true);
    e.session(&store, "s-empty", false);
    let r = store.reader().unwrap();
    assert_eq!(r.list_sessions(None).unwrap().len(), 2);
    let used: Vec<_> = r
        .list_used_sessions(None)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(used, ["s-used"]);
    assert_eq!(r.verify_chain("s-used").unwrap().broken, None);
    let far = OffsetDateTime::now_utc() + time::Duration::days(400);
    let plan = store.clean_plan(CleanScope::ClosedEmpty, &[], far).unwrap();
    assert_eq!(plan.ids, ["s-empty"]);
    assert_eq!(
        store
            .prune_empty_sessions("none", std::time::Duration::ZERO, far)
            .unwrap(),
        1
    );
    let backup = store.backup_db(far).unwrap();
    assert!(backup.to_string_lossy().contains("clean-"), "{backup:?}");

    // none of it touched the schema, and a second default open does not either
    drop(store);
    e.v4();
    assert_eq!(e.version(), 4);
    assert_eq!(schema_digest(&e.db()), V4_SCHEMA_DIGEST);
    // outcomes are unavailable and say how to enable them
    let s = e.v4();
    assert!(matches!(
        s.create_outcome(NewOutcome {
            workspace: &e.ws(),
            base_commit: "x".into(),
            contract: &contract(),
            policy: &policy(),
        }),
        Err(Error::OutcomesNotEnabled)
    ));
    assert!(matches!(
        s.reader().unwrap().list_outcomes(),
        Err(Error::OutcomesNotEnabled)
    ));
    assert!(
        Error::OutcomesNotEnabled
            .to_string()
            .contains("synt outcome enable")
    );
}

#[test]
fn enabling_backs_up_first_then_migrates_once() {
    let e = env();
    let store = e.v4();
    e.session(&store, "s1", true);
    drop(store);

    let store = e.v5();
    let m = store
        .migration()
        .expect("an existing v4 database is migrated with a backup");
    assert_eq!((m.from, m.to), (4, 5));
    assert!(m.backup.is_file());
    let dir = m
        .backup
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(dir.starts_with("pre-v5-"), "{dir}");
    // the copy is the v4 database as it was: old version, old schema, the data
    let b = Connection::open(&m.backup).unwrap();
    assert_eq!(
        b.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(schema_digest(&b), V4_SCHEMA_DIGEST);
    assert_eq!(
        b.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&m.backup), 0o600);
        assert_eq!(mode(m.backup.parent().unwrap()), 0o700);
    }
    assert_eq!(e.version(), 5);

    // opening again (either way) migrates nothing, backs up nothing and keeps v5
    drop(store);
    assert!(e.v5().migration().is_none());
    let flag_off = e.v4();
    assert!(flag_off.migration().is_none());
    assert_eq!(
        e.version(),
        5,
        "a feature-off open on a v5 database works and leaves it"
    );
    let used = flag_off.reader().unwrap().list_used_sessions(None).unwrap();
    assert_eq!(used.len(), 1);
    assert_eq!(
        std::fs::read_dir(e.home.join("backups")).unwrap().count(),
        1
    );
}

#[test]
fn a_fresh_home_enabled_at_once_needs_no_backup() {
    let e = env();
    let store = e.v5();
    assert!(store.migration().is_none());
    assert_eq!(e.version(), 5);
    assert!(!e.home.join("backups").exists());
}

#[test]
fn a_failed_backup_stops_the_migration() {
    let e = env();
    drop(e.v4());
    // `backups` is a file: the directory cannot be made
    std::fs::write(e.home.join("backups"), b"in the way").unwrap();
    let r = Store::open(
        &e.home,
        StoreOptions {
            schema_target: SCHEMA_VERSION_OUTCOMES,
            ..StoreOptions::default()
        },
    );
    assert!(r.is_err());
    assert_eq!(e.version(), 4, "no backup, no change");
}

// ------------------------------------------------------- control session, lists

#[test]
fn the_control_session_is_hidden_from_listers_and_cleaners() {
    let e = env();
    let store = e.v5();
    e.session(&store, "chat-1", true);
    let w = new_outcome(&e, &store);
    let control = w.control_session_id().to_string();
    let kind: String = e
        .db()
        .query_row("SELECT kind FROM sessions WHERE id=?1", [&control], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kind, "outcome");
    let r = store.reader().unwrap();
    for list in [
        r.list_sessions(None).unwrap(),
        r.list_used_sessions(None).unwrap(),
    ] {
        let ids: Vec<_> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["chat-1"], "the control session is not a chat");
    }
    assert_eq!(r.get_session(&control).unwrap().id, control);
    drop(w); // lock released: only the filter protects it now
    let far = OffsetDateTime::now_utc() + time::Duration::days(400);
    assert_eq!(
        store
            .prune_empty_sessions("keep", std::time::Duration::ZERO, far)
            .unwrap(),
        0,
        "an empty control session is not an empty chat"
    );
    for scope in [
        CleanScope::ClosedEmpty,
        CleanScope::Inactive,
        CleanScope::All,
    ] {
        let plan = store.clean_plan(scope, &["chat-1".into()], far).unwrap();
        assert!(plan.ids.iter().all(|i| *i != control), "{scope:?}");
    }
}

/// F10: a user path must not resume, close or overwrite the orchestrator's sessions.
#[test]
fn user_paths_cannot_touch_control_or_attempt_sessions() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    let control = w.control_session_id().to_string();
    go(&mut w, OutcomeState::Running);
    run_agent(&e, &store, &mut w, item(&items, "inspect"), "att-1", "c1");
    for id in [control.as_str(), "att-1"] {
        let r = store.resume_session(id, false);
        assert!(
            matches!(r, Err(Error::Invalid(ref m)) if m.contains("belongs to an outcome")),
            "{id}"
        );
        assert!(store.close_session(id).is_err(), "{id}");
    }
    // importing a transcript under such an id cannot replace the session either
    let f = e.repo.join("x.jsonl");
    std::fs::write(&f, "").unwrap();
    for id in [control.as_str(), "att-1"] {
        let ws = e.ws();
        assert!(
            store
                .import_node_transcript(&f, e.new_session(id, &ws, None))
                .is_err()
        );
    }
    // an ordinary chat is unaffected
    e.session(&store, "chat", true);
    drop(store.resume_session("chat", false).unwrap());
    assert!(store.close_session("chat").unwrap());
}

#[test]
fn an_attempt_session_survives_the_cleaners() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    run_agent(&e, &store, &mut w, item(&items, "implement"), "att-1", "c1");
    e.session(&store, "other", true);
    let far = OffsetDateTime::now_utc() + time::Duration::days(400);
    let plan = store.clean_plan(CleanScope::All, &[], far).unwrap();
    assert_eq!(plan.ids, ["other"], "evidence and attempts point at att-1");
}

#[test]
fn outcome_events_are_hash_chained_and_a_tampered_one_is_found() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    let control = w.control_session_id().to_string();
    item_to(&mut w, &items[0].1, WorkItemState::Ready).unwrap();
    let rep = store.reader().unwrap().verify_chain(&control).unwrap();
    assert!(
        !rep.legacy && rep.broken.is_none() && rep.chained >= 6,
        "{rep:?}"
    );
    let ks = kinds(&w);
    for k in [
        "outcome.created",
        "outcome.contract_approved",
        "plan.created",
        "outcome.state_changed",
        "work_item.state_changed",
    ] {
        assert!(ks.iter().any(|x| x == k), "{k} in {ks:?}");
    }
    e.db()
        .execute(
            "UPDATE events SET payload='{\"x\":1}' WHERE session_id=?1 AND type='plan.created'",
            [&control],
        )
        .unwrap();
    assert!(
        store
            .reader()
            .unwrap()
            .verify_chain(&control)
            .unwrap()
            .broken
            .is_some()
    );
}

// ----------------------------------------------------------- F11 policy gate

#[test]
fn the_store_refuses_when_the_settings_turn_outcomes_off_or_the_contract_exceeds_them() {
    let e = env();
    let store = e.v5();
    let off = OutcomePolicy {
        enabled: false,
        ..policy()
    };
    let r = store.create_outcome(NewOutcome {
        workspace: &e.ws(),
        base_commit: "x".into(),
        contract: &contract(),
        policy: &off,
    });
    assert!(matches!(r, Err(Error::OutcomesDisabled)));
    let w = new_outcome(&e, &store);
    let id = w.id().clone();
    drop(w);
    // a managed `enabled = false` also stops an outcome that already exists
    assert!(matches!(
        store.open_outcome(&id, &off),
        Err(Error::OutcomesDisabled)
    ));
    drop(store.open_outcome(&id, &policy()).unwrap());

    let create = |c: &Contract, p: &OutcomePolicy| {
        store
            .create_outcome(NewOutcome {
                workspace: &e.ws(),
                base_commit: "x".into(),
                contract: c,
                policy: p,
            })
            .map(drop)
    };
    let mut c = contract();
    c.envelope.budgets.max_cost_usd = 50.0;
    let cap = OutcomePolicy {
        max_cost_usd: Some(10.0),
        ..policy()
    };
    assert!(
        create(&c, &cap)
            .unwrap_err()
            .to_string()
            .contains("max_cost_usd")
    );
    c.envelope.budgets.max_cost_usd = 5.0;
    c.envelope.budgets.max_parallel_workers = 8;
    let workers = OutcomePolicy {
        max_parallel_workers: 2,
        ..policy()
    };
    assert!(
        create(&c, &workers)
            .unwrap_err()
            .to_string()
            .contains("max_parallel_workers")
    );
    c.envelope.budgets.max_parallel_workers = 2;
    c.envelope.budgets.max_attempts_per_work_item = 5;
    let attempts = OutcomePolicy {
        max_attempts_per_work_item: 3,
        ..policy()
    };
    assert!(
        create(&c, &attempts)
            .unwrap_err()
            .to_string()
            .contains("max_attempts")
    );
    // a ceiling on the per-attempt cap makes the contract name one
    c.envelope.budgets.max_attempts_per_work_item = 3;
    let per = OutcomePolicy {
        attempt_max_cost_usd: Some(1.0),
        attempt_max_wall_secs: Some(60),
        ..policy()
    };
    assert!(
        create(&c, &per)
            .unwrap_err()
            .to_string()
            .contains("attempt_max_cost_usd")
    );
    c.envelope.budgets.attempt_max_cost_usd = Some(1.0);
    assert!(
        create(&c, &per)
            .unwrap_err()
            .to_string()
            .contains("attempt_max_wall_secs")
    );
    c.envelope.budgets.attempt_max_wall_secs = Some(600);
    assert!(create(&c, &per).is_err());
    c.envelope.budgets.attempt_max_wall_secs = Some(60);
    create(&c, &per).unwrap();
}

#[test]
fn a_tighter_ceiling_applies_to_a_stored_contracts_attempt_budget() {
    let e = env();
    let store = e.v5();
    let (w, items) = ready_outcome(&e, &store);
    let id = w.id().clone();
    drop(w);
    // the organisation lowers the cap to 1 attempt per item after approval
    let tight = OutcomePolicy {
        max_attempts_per_work_item: 1,
        ..policy()
    };
    let mut w = store.open_outcome(&id, &tight).unwrap();
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "implement");
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
    ] {
        item_to(&mut w, it, to).unwrap();
    }
    e.child(&store, "a1", w.control_session_id());
    let (a, _) = w.new_attempt(it, 1, "a1", "local-inproc", NONE).unwrap();
    let r = w
        .transition_attempt(
            &a.id,
            1,
            AttemptState::Running,
            &AttemptEnd::default(),
            NONE,
        )
        .unwrap();
    w.transition_attempt(
        &a.id,
        r.revision,
        AttemptState::Lost,
        &AttemptEnd::default(),
        NONE,
    )
    .unwrap();
    e.child(&store, "a2", w.control_session_id());
    let err = w
        .new_attempt(it, 2, "a2", "local-inproc", NONE)
        .unwrap_err();
    assert!(err.to_string().contains("attempt budget"), "{err}");
}

// -------------------------------------------------------------- revisions, CAS

#[test]
fn transitions_are_compare_and_swap_and_follow_the_tables() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let id = w.id().clone();
    let o = w.get_outcome(&id).unwrap();
    assert_eq!((o.state, o.plan_version), (OutcomeState::Ready, Some(1)));
    let e1 = w
        .transition_outcome(o.revision - 1, OutcomeState::Running, None, NONE)
        .unwrap_err();
    assert!(
        matches!(
            e1,
            Error::RevisionConflict {
                aggregate: "outcome",
                ..
            }
        ),
        "{e1}"
    );
    assert_eq!(w.get_outcome(&id).unwrap().revision, o.revision);
    assert!(
        w.transition_outcome(o.revision, OutcomeState::Verified, None, NONE)
            .is_err()
    );
    let r = w
        .transition_outcome(o.revision, OutcomeState::Running, None, NONE)
        .unwrap();
    assert_eq!(r.revision, o.revision + 1);
    // a work item of another outcome is not reachable from this writer
    drop(w);
    let (_other, other_items) = ready_outcome(&e, &store);
    let mut w = store.open_outcome(&id, &policy()).unwrap();
    let err = w
        .transition_work_item(&other_items[0].1, 1, WorkItemState::Ready, NONE)
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
}

#[test]
fn a_failed_transaction_leaves_no_row_and_no_event() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let before = (kinds(&w).len(), w.get_outcome(w.id()).unwrap());
    let rev = before.1.revision;
    let r: Result<(), Error> = w.atomically(|tx| {
        tx.transition_outcome(rev, OutcomeState::Running, None, NONE)?;
        tx.enqueue(&NewOutbox {
            idempotency_key: "k".into(),
            kind: OutboxKind::WorktreeCreate,
            payload: json!({}),
        })?;
        Err(Error::Invalid("boom after the writes".into()))
    });
    assert!(r.is_err());
    assert_eq!(
        kinds(&w).len(),
        before.0,
        "event rolled back with the state"
    );
    assert_eq!(w.get_outcome(w.id()).unwrap(), before.1);
    assert!(w.list_outbox(w.id()).unwrap().is_empty());
}

// ------------------------------------------------- F2 approval and immutability

#[test]
fn ready_needs_an_authentic_approval_and_a_stored_plan() {
    let e = env();
    let store = e.v5();
    let mut w = new_outcome(&e, &store);
    go(&mut w, OutcomeState::ContractPending);
    let err = w
        .transition_outcome(rev_of(&w), OutcomeState::Ready, None, NONE)
        .unwrap_err();
    assert!(err.to_string().contains("not approved"), "{err}");
    assert!(
        w.approve_contract(1, &"0".repeat(64), "user").is_err(),
        "wrong hash"
    );
    assert!(
        w.approve_contract(1, &contract_hash(&w), "").is_err(),
        "no approver"
    );
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    w.approve_contract(1, &h, "user").unwrap(); // again: no second approval
    assert_eq!(
        kinds(&w)
            .iter()
            .filter(|k| *k == "outcome.contract_approved")
            .count(),
        1
    );
    assert!(w.contract_is_approved(w.id(), 1).unwrap());
    // approved but no plan yet
    assert!(
        w.transition_outcome(rev_of(&w), OutcomeState::Ready, None, NONE)
            .is_err()
    );
    w.insert_plan(&plan()).unwrap();
    go(&mut w, OutcomeState::Ready);
}

/// F2: one bare UPDATE (what a same-user process or a bug could do) must not forge approval.
#[test]
fn a_forged_contract_approval_does_not_open_ready() {
    let e = env();
    let store = e.v5();
    let mut w = new_outcome(&e, &store);
    go(&mut w, OutcomeState::ContractPending);
    let forge = |mac: &str| {
        e.db().execute(
            "UPDATE contracts SET approved_by='user', approved_at='2026-01-01T00:00:00.000Z', approved_mac=?1",
            [mac],
        )
    };
    // no mac at all: the trigger refuses
    let err = e
        .db()
        .execute("UPDATE contracts SET approved_by='x', approved_at='t'", [])
        .unwrap_err();
    assert!(err.to_string().contains("approved once"), "{err}");
    // a made-up mac gets past the trigger, not past the check on use
    forge(&"ab".repeat(32)).unwrap();
    assert!(!w.contract_is_approved(w.id(), 1).unwrap());
    // and the plan insert (needs the approved contract) refuses too
    assert!(w.insert_plan(&plan()).is_err());
    let err = w
        .transition_outcome(rev_of(&w), OutcomeState::Ready, None, NONE)
        .unwrap_err();
    assert!(err.to_string().contains("not approved"), "{err}");
    // a second bare UPDATE (replace an existing approval) is refused outright
    let err = forge(&"cd".repeat(32)).unwrap_err();
    assert!(err.to_string().contains("approved once"), "{err}");
}

#[test]
fn an_approval_for_another_contract_body_or_outcome_does_not_verify() {
    let e = env();
    let store = e.v5();
    let mut w = approved_outcome(&e, &store);
    let db = e.db();
    // swapping the stored body (the trigger blocks UPDATE; a REPLACE-style rewrite is simulated
    // by dropping the trigger as a writer with raw access could) breaks hash and mac
    db.execute("DROP TRIGGER contracts_immutable", []).unwrap();
    db.execute(
        "UPDATE contracts SET body=replace(body,'endpoint','deploy to prod')",
        [],
    )
    .unwrap();
    assert!(
        !w.contract_is_approved(w.id(), 1).unwrap(),
        "body no longer matches the approved hash"
    );
    let h = contract_hash(&w);
    assert!(w.approve_contract(1, &h, "user").is_err());
}

#[test]
fn history_cannot_be_deleted_or_rewritten_with_plain_sql() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    finish_items(&e, &store, &mut w, &items);
    let (d, _) = w.new_decision(&decision("k", json!({"v": 1}))).unwrap();
    let db = e.db();
    for t in ["contracts", "plans", "evidence", "decisions", "outcomes"] {
        let err = db
            .execute(&format!("DELETE FROM {t}"), [])
            .unwrap_err()
            .to_string();
        assert!(err.contains("never deleted"), "{t}: {err}");
    }
    for sql in [
        "UPDATE evidence SET result='passed'",
        "UPDATE evidence SET trust_class='independent_check'",
        "UPDATE evidence SET sha256='x'",
        "UPDATE decisions SET brief='{}'",
        "UPDATE decisions SET binding='{}'",
        "UPDATE decisions SET expires_at='2099-01-01T00:00:00.000Z'",
        "UPDATE outcomes SET base_commit='evil'",
        "UPDATE outcomes SET control_session_id='x'",
        "UPDATE outcomes SET state='verified'",
        "UPDATE work_items SET state='succeeded'",
        "UPDATE attempts SET state='lost'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    // evidence flags (stale marking) are the one allowed change
    db.execute("UPDATE evidence SET flags='[\"stale\"]'", [])
        .unwrap();
    // a decision cannot be resolved by a bare UPDATE: no approval row
    let err = db
        .execute(
            "UPDATE decisions SET state='approved', revision=revision+1 WHERE id=?1",
            [d.id.as_str()],
        )
        .unwrap_err();
    assert!(err.to_string().contains("needs its approval row"), "{err}");
    // finished commands do not change
    let q = store
        .enqueue_command(NewCommand {
            outcome_id: w.id().clone(),
            idempotency_key: "k1".into(),
            kind: "outcome.pause".into(),
            params: json!({}),
            expected_revision: None,
            from_surface: Surface::Cli,
        })
        .unwrap();
    assert!(
        db.execute(
            "UPDATE outcome_commands SET kind='outcome.cancel' WHERE id=?1",
            [q.id.as_str()]
        )
        .is_err()
    );
    w.apply_command("k1", |_, _| Ok((CommandState::Applied, json!({}))))
        .unwrap();
    assert!(
        db.execute("UPDATE outcome_commands SET result='{\"x\":1}'", [])
            .is_err()
    );
}

// --------------------------------------------------------- F3 derived guards

#[test]
fn success_and_verified_come_from_evidence_in_the_rows_not_from_the_caller() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    let (inspect, implement) = (item(&items, "inspect"), item(&items, "implement"));
    go(&mut w, OutcomeState::Running);
    // no evidence: the item cannot succeed
    let a = run_agent(&e, &store, &mut w, inspect, "att-1", "base0");
    let err = item_to(&mut w, inspect, WorkItemState::Succeeded).unwrap_err();
    assert!(err.to_string().contains("independent evidence"), "{err}");
    // evidence of another trust class, a failed one, one for another commit: none count
    let mut weak = evidence(inspect, Some(&a), "base0", &[]);
    weak.trust_class = TrustClass::ObservedInAttempt;
    weak.producer = "gate".into();
    weak.check_id = "gate-unit".into();
    w.record_evidence(&weak).unwrap();
    let mut failed = evidence(inspect, Some(&a), "base0", &[]);
    failed.check_id = "lint".into();
    failed.result = "failed".into();
    w.record_evidence(&failed).unwrap();
    let mut other = evidence(inspect, Some(&a), "ffff", &[]);
    other.check_id = "other".into();
    w.record_evidence(&other).unwrap();
    assert!(item_to(&mut w, inspect, WorkItemState::Succeeded).is_err());
    w.record_evidence(&evidence(inspect, Some(&a), "base0", &[]))
        .unwrap();
    item_to(&mut w, inspect, WorkItemState::Succeeded).unwrap();
    // a definitions_changed flag withdraws it from the verdict inputs
    let _ = implement;
}

#[test]
fn verified_needs_the_stores_verdict_over_every_item_and_criterion() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    go(&mut w, OutcomeState::Verifying);
    // nothing ran: no caller-side flag can make it verified
    let err = w
        .transition_outcome(rev_of(&w), OutcomeState::Verified, None, NONE)
        .unwrap_err();
    assert!(err.to_string().contains("verdict"), "{err}");
    // (a fresh outcome so the item flow starts from Running)
    let (mut w, items2) = ready_outcome(&e, &store);
    let _ = (items, items2);
    let items = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .map(|i| (i.key, i.id))
        .collect::<Vec<_>>();
    let commit = finish_items(&e, &store, &mut w, &items);
    assert_eq!(commit, "cafe01");
    go(&mut w, OutcomeState::Verifying);
    // Completed from Verifying is not in the table; Verified now holds
    go(&mut w, OutcomeState::Verified);
    go(&mut w, OutcomeState::Completed);
    assert_eq!(
        w.get_outcome(w.id()).unwrap().state,
        OutcomeState::Completed
    );
}

#[test]
fn a_failed_stale_or_flagged_check_blocks_verified_even_when_every_item_succeeded() {
    for case in ["failed", "flagged", "other_commit"] {
        let e = env();
        let store = e.v5();
        let (mut w, items) = ready_outcome(&e, &store);
        finish_items(&e, &store, &mut w, &items);
        let test = item(&items, "test").clone();
        match case {
            // a failing check of the required class for the same commit wins over the pass
            "failed" => {
                let mut f = evidence(&test, None, "cafe01", &["c1", "c2"]);
                f.check_id = "lint".into();
                f.result = "failed".into();
                w.record_evidence(&f).unwrap();
            }
            // the runner flagged changed definitions: that evidence does not count
            "flagged" => {
                e.db().execute("UPDATE evidence SET flags='[\"definitions_changed\"]' WHERE work_item_id=?1", [test.as_str()]).unwrap();
            }
            // the only evidence for c2 names a commit that is not the candidate
            _ => {
                e.db()
                    .execute("DROP TRIGGER evidence_immutable", [])
                    .unwrap();
                e.db()
                    .execute(
                        "UPDATE evidence SET subject_commit='beef02' WHERE work_item_id=?1",
                        [test.as_str()],
                    )
                    .unwrap();
            }
        }
        go(&mut w, OutcomeState::Verifying);
        let err = w
            .transition_outcome(rev_of(&w), OutcomeState::Verified, None, NONE)
            .unwrap_err();
        assert!(err.to_string().contains("verdict"), "{case}: {err}");
    }
}

#[test]
fn the_retry_budget_comes_from_the_stored_contract() {
    let e = env();
    let store = e.v5();
    let mut c = contract();
    c.envelope.budgets.max_attempts_per_work_item = 2; // one retry
    let mut w = new_outcome_with(&e, &store, &c, &policy());
    go(&mut w, OutcomeState::ContractPending);
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    let items = w.insert_plan(&plan()).unwrap();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "inspect").clone();
    use WorkItemState::*;
    for to in [Ready, Leased, Running] {
        item_to(&mut w, &it, to).unwrap();
    }
    item_to(&mut w, &it, RetryableFailure).unwrap();
    item_to(&mut w, &it, Ready).unwrap();
    assert_eq!(
        w.list_work_items(w.id())
            .unwrap()
            .into_iter()
            .find(|i| i.id == it)
            .unwrap()
            .retries_used,
        1
    );
    item_to(&mut w, &it, Leased).unwrap();
    item_to(&mut w, &it, Running).unwrap();
    let err = item_to(&mut w, &it, RetryableFailure).unwrap_err();
    assert!(err.to_string().contains("retry budget"), "{err}");
    item_to(&mut w, &it, TerminalFailure).unwrap();
}

// --------------------------------------------------------------------- attempts

#[test]
fn attempts_follow_the_ordinal_budget_liveness_and_session_rules() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "implement").clone();
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
    ] {
        item_to(&mut w, &it, to).unwrap();
    }
    let control = w.control_session_id().to_string();
    e.child(&store, "s1", &control);
    e.child(&store, "s2", &control);
    e.session(&store, "plain", true); // not a child of the control session
    // not the next ordinal
    assert!(
        w.new_attempt(&it, 2, "s1", "local-inproc", NONE)
            .unwrap_err()
            .to_string()
            .contains("next attempt")
    );
    assert!(w.new_attempt(&it, 0, "s1", "local-inproc", NONE).is_err());
    // sessions: a plain chat, the control session itself, a missing one, a bad worker kind
    for sess in ["plain", control.as_str(), "no-such"] {
        assert!(
            w.new_attempt(&it, 1, sess, "local-inproc", NONE)
                .unwrap_err()
                .to_string()
                .contains("ordinary child"),
            "{sess}"
        );
    }
    assert!(w.new_attempt(&it, 1, "s1", "remote", NONE).is_err());
    // a verify item runs no attempts
    assert!(
        w.new_attempt(item(&items, "test"), 1, "s1", "local-inproc", NONE)
            .is_err()
    );
    let (a1, _) = w.new_attempt(&it, 1, "s1", "local-inproc", NONE).unwrap();
    assert_eq!((a1.state, a1.fence), (AttemptState::Starting, 1));
    // replay: refused, the caller adopts the row
    assert!(matches!(
        w.new_attempt(&it, 1, "s2", "local-inproc", NONE),
        Err(Error::AlreadyExists(_))
    ));
    // a second live attempt is refused
    assert!(
        w.new_attempt(&it, 2, "s2", "local-inproc", NONE)
            .unwrap_err()
            .to_string()
            .contains("still live")
    );
    let r = w
        .transition_attempt(
            &a1.id,
            1,
            AttemptState::Running,
            &AttemptEnd::default(),
            NONE,
        )
        .unwrap();
    w.transition_attempt(
        &a1.id,
        r.revision,
        AttemptState::Lost,
        &AttemptEnd::default(),
        NONE,
    )
    .unwrap();
    let (a2, _) = w.new_attempt(&it, 2, "s2", "local-inproc", NONE).unwrap();
    assert_eq!(a2.fence, 2);
    // finish details are stored
    let got = w.list_attempts(&it).unwrap();
    assert_eq!(got[0].state, AttemptState::Lost);
    assert!(got[0].ended_at.is_some());
}

#[test]
fn evidence_is_attributed_fenced_deduplicated_and_its_artifact_is_real() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "implement").clone();
    let a1 = run_agent(&e, &store, &mut w, &it, "att-1", "abc");
    for (class, producer) in [
        (TrustClass::IndependentCheck, "worker"),
        (TrustClass::IndependentCheck, "gate"),
        (TrustClass::HumanReview, "independent_runner"),
        (TrustClass::ClaimedByWorker, "independent_runner"),
    ] {
        let mut ev = evidence(&it, Some(&a1), "abc", &["c1"]);
        ev.trust_class = class;
        ev.producer = producer.into();
        let err = w.record_evidence(&ev).unwrap_err();
        assert!(
            err.to_string().contains("cannot write"),
            "{class:?} {producer}: {err}"
        );
    }
    let mut bad = evidence(&it, Some(&a1), "abc", &["c1"]);
    bad.result = "ok?".into();
    assert!(w.record_evidence(&bad).is_err());
    let good = evidence(&it, Some(&a1), "abc", &["c1"]);
    let (id, fresh) = w.record_evidence(&good).unwrap();
    assert!(fresh);
    assert_eq!(w.record_evidence(&good).unwrap(), (id, false));
    // the same check and commit with a different result is an error, not a silent (id, false)
    let mut replay = good.clone();
    replay.result = "failed".into();
    let err = w.record_evidence(&replay).unwrap_err();
    assert!(err.to_string().contains("different content"), "{err}");
    let mut replay = good.clone();
    replay.criteria_ids = vec!["c2".into()];
    assert!(w.record_evidence(&replay).is_err());
    assert_eq!(w.list_evidence(w.id()).unwrap().len(), 1);
    // a fence that does not match the attempt
    let mut wrong = good.clone();
    wrong.check_id = "lint".into();
    wrong.fence = 7;
    assert!(w.record_evidence(&wrong).is_err());

    // human evidence without an attempt dedupes too (NULL attempt ids)
    let mut human = evidence(&it, None, "abc", &["c1"]);
    human.trust_class = TrustClass::HumanReview;
    human.producer = "human".into();
    human.fence = 1;
    let (hid, fresh) = w.record_evidence(&human).unwrap();
    assert!(fresh);
    assert_eq!(w.record_evidence(&human).unwrap(), (hid, false));
    let n: i64 = e
        .db()
        .query_row(
            "SELECT COUNT(*) FROM evidence WHERE attempt_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);

    // artifacts: malformed, unknown, wrong hash, someone else's session: refused; the real one passes
    let mut sw = store.resume_session_for_test("att-1");
    let art = sw
        .put_artifact("report", "text/plain", b"all green", Retention::Session)
        .unwrap();
    drop(sw);
    let with = |s: &str, a: &str, h: &str| {
        let mut ev = evidence(&it, Some(&a1), "abc", &["c1"]);
        ev.check_id = format!("art-{s}-{a}-{}", &h[..h.len().min(4)]);
        ev.artifact = Some((s.into(), a.into(), h.into()));
        ev
    };
    let real = art.sha256.clone();
    for (s, a, h) in [
        ("att-1", art.artifact_id.as_str(), "zz"),
        ("../x", art.artifact_id.as_str(), real.as_str()),
        ("att-1", "no-such", real.as_str()),
        ("att-1", art.artifact_id.as_str(), &"0".repeat(64)),
    ] {
        assert!(w.record_evidence(&with(s, a, h)).is_err(), "{s} {a} {h}");
    }
    e.session(&store, "elsewhere", false);
    let mut ew = store.resume_session_for_test("elsewhere");
    let other = ew
        .put_artifact("report", "text/plain", b"x", Retention::Session)
        .unwrap();
    drop(ew);
    assert!(
        w.record_evidence(&with("elsewhere", &other.artifact_id, &other.sha256))
            .is_err(),
        "not this outcome's session"
    );
    w.record_evidence(&with("att-1", &art.artifact_id, &real))
        .unwrap();
}

/// `resume_session` refuses attempt sessions (F10), so the test opens the writer through the
/// orchestrator-side path the crate keeps for itself.
trait ResumeForTest {
    fn resume_session_for_test(&self, id: &str) -> synthesis_session::SessionWriter;
}
impl ResumeForTest for Store {
    fn resume_session_for_test(&self, id: &str) -> synthesis_session::SessionWriter {
        self.open_session_for_orchestrator(id).unwrap()
    }
}

// -------------------------------------------------------------------- decisions

fn decision(key: &str, binding: Value) -> NewDecision {
    NewDecision {
        kind: "ambiguity".into(),
        dedupe_key: key.into(),
        brief: json!({"why": "which API shape", "alternatives": ["A", "B"], "safe_default": "HOLD"}),
        binding,
        expires_at: hours(2),
    }
}

fn binding() -> Value {
    json!({"contract_version": 1, "plan_version": 1, "subject_commit": "abc", "policy_hash": "p1"})
}

fn open_decision(w: &mut OutcomeWriter) -> DecisionRecord {
    let (d, _) = w.new_decision(&decision("k", binding())).unwrap();
    let r = w
        .transition_decision(&d.id, 1, DecisionState::Pending, &binding(), NONE)
        .unwrap();
    assert_eq!(r.revision, 2);
    w.list_decisions(w.id()).unwrap().remove(0)
}

fn resolution(d: &DecisionRecord, choice: &str) -> Resolution {
    Resolution {
        decision_id: d.id.clone(),
        decision_revision: d.revision,
        approver: "user:cli".into(),
        choice: choice.into(),
        brief_sha256: d.brief_sha256.clone(),
    }
}

#[test]
fn a_decision_is_resolved_only_by_resolve_decision_and_the_approval_is_authentic() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let d = open_decision(&mut w);
    // the generic transition refuses every resolving state
    for to in [
        DecisionState::Approved,
        DecisionState::Rejected,
        DecisionState::NeedsMoreInfo,
    ] {
        let err = w
            .transition_decision(&d.id, d.revision, to, &binding(), NONE)
            .unwrap_err();
        assert!(
            err.to_string().contains("resolve_decision"),
            "{to:?}: {err}"
        );
    }
    assert_eq!(
        w.list_decisions(w.id()).unwrap()[0].state,
        DecisionState::Pending
    );
    // what the person saw must be what is stored
    let mut stale = resolution(&d, "A");
    stale.brief_sha256 = "0".repeat(64);
    assert!(matches!(
        w.resolve_decision(&stale, &binding()),
        Err(Error::Stale(_))
    ));
    let mut moved = binding();
    moved["subject_commit"] = json!("def");
    assert!(matches!(
        w.resolve_decision(&resolution(&d, "A"), &moved),
        Err(Error::Stale(_))
    ));
    let mut old_rev = resolution(&d, "A");
    old_rev.decision_revision = 1;
    assert!(matches!(
        w.resolve_decision(&old_rev, &binding()),
        Err(Error::RevisionConflict { .. })
    ));
    // choices: none, unknown, a default-sounding word: refused
    for c in ["", "C", "approve", "default"] {
        assert!(
            w.resolve_decision(&resolution(&d, c), &binding()).is_err(),
            "{c:?}"
        );
    }
    let mut anon = resolution(&d, "A");
    anon.approver = String::new();
    assert!(w.resolve_decision(&anon, &binding()).is_err());
    assert_eq!(
        w.list_decisions(w.id()).unwrap()[0].state,
        DecisionState::Pending,
        "nothing changed"
    );
    assert!(w.decision_approvals(&d.id).unwrap().is_empty());
    assert_eq!(w.verified_resolution(&d.id).unwrap(), None);

    // the real answer: approval row, state, and a chained event in one transaction
    assert_eq!(
        w.resolve_decision(&resolution(&d, "A"), &binding())
            .unwrap(),
        DecisionState::Approved
    );
    assert_eq!(
        w.decision_approvals(&d.id).unwrap(),
        [(2, "user:cli".to_string(), "A".to_string())]
    );
    let got = w.verified_resolution(&d.id).unwrap().unwrap();
    assert_eq!(
        (got.state, got.approver.as_str(), got.choice.as_str()),
        (DecisionState::Approved, "user:cli", "A")
    );
    let ev = w
        .read_events(w.control_session_id(), 0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|r| match r {
            synthesis_events::Record::Unknown(raw) if raw.kind == "decision.resolved" => {
                Some(raw.payload)
            }
            _ => None,
        })
        .next()
        .expect("decision.resolved event");
    assert_eq!(ev["approver"], "user:cli");
    assert_eq!(ev["brief_sha256"], d.brief_sha256);
    assert_eq!(ev["revision"], 3);
    assert!(
        store
            .reader()
            .unwrap()
            .verify_chain(w.control_session_id())
            .unwrap()
            .broken
            .is_none()
    );
    // a second answer finds a resolved decision
    assert!(
        w.resolve_decision(&resolution(&d, "B"), &binding())
            .is_err()
    );
}

#[test]
fn reject_and_request_evidence_are_resolutions_too_and_expiry_blocks_them() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let d = open_decision(&mut w);
    assert_eq!(
        w.resolve_decision(&resolution(&d, "request_evidence"), &binding())
            .unwrap(),
        DecisionState::NeedsMoreInfo
    );
    assert!(w.verified_resolution(&d.id).unwrap().is_some());
    drop(w);
    // an expired decision cannot be answered; the orchestrator expires it, never approves it
    let e = env();
    let store = e.v5();
    let (w, _) = ready_outcome(&e, &store);
    let id = w.id().clone();
    drop(w);
    let mut w = store
        .open_outcome(&id, &policy())
        .unwrap()
        .with_clock(|| OffsetDateTime::now_utc() + time::Duration::hours(3));
    let (d, _) = {
        // created "in the past" relative to the late clock: build it with the real clock first
        drop(w);
        let mut w0 = store.open_outcome(&id, &policy()).unwrap();
        let r = w0.new_decision(&decision("late", binding())).unwrap();
        drop(w0);
        w = store
            .open_outcome(&id, &policy())
            .unwrap()
            .with_clock(|| OffsetDateTime::now_utc() + time::Duration::hours(3));
        r
    };
    w.transition_decision(&d.id, 1, DecisionState::Pending, &binding(), NONE)
        .unwrap();
    let d = w.list_decisions(&id).unwrap().remove(0);
    let err = w
        .resolve_decision(&resolution(&d, "A"), &binding())
        .unwrap_err();
    assert!(err.to_string().contains("expired"), "{err}");
    w.transition_decision(&d.id, d.revision, DecisionState::Expired, &binding(), NONE)
        .unwrap();
    assert_eq!(
        w.list_decisions(&id).unwrap()[0].state,
        DecisionState::Expired
    );
    assert!(
        w.resolve_decision(&resolution(&d, "A"), &binding())
            .is_err()
    );
}

#[test]
fn the_orchestrator_expires_and_invalidates_only_when_the_facts_say_so() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let d = open_decision(&mut w);
    // not expired yet, binding unchanged: neither transition is allowed
    assert!(
        w.transition_decision(&d.id, d.revision, DecisionState::Expired, &binding(), NONE)
            .is_err()
    );
    assert!(
        w.transition_decision(
            &d.id,
            d.revision,
            DecisionState::Invalidated,
            &binding(),
            NONE
        )
        .is_err()
    );
    let mut moved = binding();
    moved["policy_hash"] = json!("p2");
    w.transition_decision(&d.id, d.revision, DecisionState::Invalidated, &moved, NONE)
        .unwrap();
    assert!(w.resolve_decision(&resolution(&d, "A"), &moved).is_err());
}

/// F7
#[test]
fn decision_dedupe_follows_the_binding_and_rejects_conflicting_reuse() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let (d, fresh) = w.new_decision(&decision("k", binding())).unwrap();
    assert!(fresh && d.state == DecisionState::Preparing);
    let (again, fresh) = w.new_decision(&decision("k", binding())).unwrap();
    assert!(!fresh && again.id == d.id);
    // same key and binding but another brief is an error, not a silent reuse
    let mut other = decision("k", binding());
    other.brief["why"] = json!("something else");
    assert!(
        w.new_decision(&other)
            .unwrap_err()
            .to_string()
            .contains("different brief")
    );
    // a new binding is a new decision
    let mut b2 = binding();
    b2["subject_commit"] = json!("def");
    let (d2, fresh) = w.new_decision(&decision("k", b2.clone())).unwrap();
    assert!(fresh && d2.id != d.id);
    assert_eq!(w.list_decisions(w.id()).unwrap().len(), 2);
    assert_eq!(
        kinds(&w)
            .iter()
            .filter(|k| *k == "decision.created")
            .count(),
        2
    );
    // an invalidated decision does not block asking again for the same binding
    w.transition_decision(&d2.id, 1, DecisionState::Pending, &b2, NONE)
        .unwrap();
    let mut b3 = b2.clone();
    b3["policy_hash"] = json!("x");
    w.transition_decision(&d2.id, 2, DecisionState::Invalidated, &b3, NONE)
        .unwrap();
    let (d3, fresh) = w.new_decision(&decision("k", b2)).unwrap();
    assert!(fresh && d3.id != d2.id);
    // shape and time
    assert!(
        w.new_decision(&NewDecision {
            brief: json!("text"),
            ..decision("x", binding())
        })
        .is_err()
    );
    for field in [
        "selected",
        "recommended",
        "default",
        "default_choice",
        "preselected",
    ] {
        let mut b = decision("y", binding());
        b.brief[field] = json!("A");
        assert!(
            w.new_decision(&b)
                .unwrap_err()
                .to_string()
                .contains("no default"),
            "{field}"
        );
    }
    for bad in [
        "tomorrow",
        "2000-01-01T00:00:00.000Z",
        &hours(24 * 40),
        "2099-01-01 00:00:00",
        "",
    ] {
        let mut b = decision("z", binding());
        b.expires_at = bad.into();
        assert!(w.new_decision(&b).is_err(), "{bad:?}");
    }
}

// --------------------------------------------------------------------- commands

fn resolve_cmd(w: &OutcomeWriter, key: &str, d: &DecisionRecord, surface: Surface) -> NewCommand {
    NewCommand {
        outcome_id: w.id().clone(),
        idempotency_key: key.into(),
        kind: "decision.resolve".into(),
        params: json!({"decision_id": d.id, "choice": "A", "brief_sha256": d.brief_sha256}),
        expected_revision: Some(d.revision),
        from_surface: surface,
    }
}

fn handler(tx: &mut OutcomeTx<'_, '_>, c: &CommandRecord) -> Result<(CommandState, Value), Error> {
    let r = Resolution::from_command(c)?;
    tx.resolve_decision(&r, &binding())?;
    Ok((
        CommandState::Applied,
        json!({"revision": r.decision_revision + 1}),
    ))
}

#[test]
fn a_resolve_command_must_carry_everything_that_was_shown() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let d = open_decision(&mut w);
    let base = resolve_cmd(&w, "c", &d, Surface::Cli);
    let mut cases: Vec<(&str, NewCommand)> = vec![];
    let mut c = base.clone();
    c.expected_revision = None;
    cases.push(("no expected_revision", c));
    let mut c = base.clone();
    c.params.as_object_mut().unwrap().remove("brief_sha256");
    cases.push(("no brief hash", c));
    let mut c = base.clone();
    c.params["brief_sha256"] = json!("nothex");
    cases.push(("bad brief hash", c));
    let mut c = base.clone();
    c.params["choice"] = json!("");
    cases.push(("empty choice", c));
    let mut c = base.clone();
    c.params.as_object_mut().unwrap().remove("choice");
    cases.push(("no choice", c));
    let mut c = base.clone();
    c.params["decision_id"] = json!("../../x");
    cases.push(("bad decision id", c));
    for (what, c) in cases {
        assert!(store.enqueue_command(c).is_err(), "{what}");
    }
    store.enqueue_command(base).unwrap();
}

#[test]
fn two_surfaces_resolving_one_decision_get_one_effect_and_one_loser() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let d = open_decision(&mut w);
    let c1 = store
        .enqueue_command(resolve_cmd(&w, "cli-1", &d, Surface::Cli))
        .unwrap();
    let c2 = store
        .enqueue_command(resolve_cmd(&w, "tui-1", &d, Surface::Tui))
        .unwrap();
    assert_eq!(
        (c1.state, c2.state),
        (CommandState::Queued, CommandState::Queued)
    );
    assert_eq!(w.queued_commands(w.id()).unwrap().len(), 2);
    let a = w.apply_command("cli-1", handler).unwrap();
    let b = w.apply_command("tui-1", handler).unwrap();
    assert_eq!(a.state, CommandState::Applied);
    assert_eq!(
        b.state,
        CommandState::Stale,
        "the second saw a newer revision"
    );
    assert_eq!(b.result.as_ref().unwrap()["aggregate"], "decision");
    assert_eq!(
        w.decision_approvals(&d.id).unwrap().len(),
        1,
        "one approval row"
    );
    // the approver is derived from the surface, not from params
    assert_eq!(
        w.verified_resolution(&d.id).unwrap().unwrap().approver,
        "user:cli"
    );
    assert!(w.queued_commands(w.id()).unwrap().is_empty());
    assert_eq!(
        kinds(&w)
            .iter()
            .filter(|k| *k == "outcome.command_finished")
            .count(),
        2
    );
}

#[test]
fn a_stale_brief_or_binding_becomes_a_stale_command_result_with_no_effect() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let d = open_decision(&mut w);
    let mut c = resolve_cmd(&w, "old-brief", &d, Surface::Cli);
    c.params["brief_sha256"] = json!("0".repeat(64));
    store.enqueue_command(c).unwrap();
    let r = w.apply_command("old-brief", handler).unwrap();
    assert_eq!(r.state, CommandState::Stale);
    assert!(
        r.result.unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("brief")
    );
    assert!(w.decision_approvals(&d.id).unwrap().is_empty());
    assert_eq!(
        w.list_decisions(w.id()).unwrap()[0].state,
        DecisionState::Pending
    );
}

#[test]
fn a_retried_command_returns_the_stored_result_with_no_second_effect() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let d = open_decision(&mut w);
    let cmd = resolve_cmd(&w, "retry-key", &d, Surface::Cli);
    let q = store.enqueue_command(cmd.clone()).unwrap();
    assert_eq!(store.enqueue_command(cmd.clone()).unwrap().id, q.id);
    let calls = RefCell::new(0);
    let run = |w: &mut OutcomeWriter| {
        w.apply_command("retry-key", |tx, c| {
            *calls.borrow_mut() += 1;
            handler(tx, c)
        })
        .unwrap()
    };
    let first = run(&mut w);
    let again = run(&mut w);
    assert_eq!(*calls.borrow(), 1);
    assert_eq!(first, again);
    assert_eq!(
        store
            .reader()
            .unwrap()
            .get_command(w.id(), "retry-key")
            .unwrap()
            .unwrap(),
        first
    );
    let mut other = cmd.clone();
    other.params["choice"] = json!("B");
    assert!(
        store
            .enqueue_command(other)
            .unwrap_err()
            .to_string()
            .contains("different request")
    );
    let mut other = cmd.clone();
    other.from_surface = Surface::Tui;
    assert!(store.enqueue_command(other).is_err());
}

/// F5: the key is scoped to the outcome.
#[test]
fn idempotency_keys_are_scoped_to_their_outcome() {
    let e = env();
    let store = e.v5();
    let w1 = new_outcome(&e, &store);
    let w2 = new_outcome(&e, &store);
    let cmd = |w: &OutcomeWriter, kind: &str| NewCommand {
        outcome_id: w.id().clone(),
        idempotency_key: "same-key".into(),
        kind: kind.into(),
        params: json!({}),
        expected_revision: None,
        from_surface: Surface::Cli,
    };
    let a = store.enqueue_command(cmd(&w1, "outcome.pause")).unwrap();
    // another outcome using the same key neither collides with it nor reads its result
    let b = store.enqueue_command(cmd(&w2, "outcome.cancel")).unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(
        (a.kind.as_str(), b.kind.as_str()),
        ("outcome.pause", "outcome.cancel")
    );
    assert!(
        store
            .reader()
            .unwrap()
            .get_command(w1.id(), "same-key")
            .unwrap()
            .unwrap()
            .kind
            == "outcome.pause"
    );
}

#[test]
fn commands_wait_for_an_owner_and_ownership_is_one_lock() {
    let e = env();
    let store = e.v5();
    let (w, _) = ready_outcome(&e, &store);
    let id = w.id().clone();
    assert!(matches!(
        store.open_outcome(&id, &policy()),
        Err(Error::Locked(_))
    ));
    drop(w);
    let q = store
        .enqueue_command(NewCommand {
            outcome_id: id.clone(),
            idempotency_key: "start-1".into(),
            kind: "outcome.start".into(),
            params: json!({}),
            expected_revision: None,
            from_surface: Surface::Cli,
        })
        .unwrap();
    assert_eq!(q.state, CommandState::Queued);
    let mut w = store.open_outcome(&id, &policy()).unwrap();
    let done = w
        .apply_command("start-1", |tx, _| {
            let rev = tx.outcome_revision()?;
            tx.transition_outcome(rev, OutcomeState::Running, None, NONE)?;
            Ok((CommandState::Applied, json!({"state": "running"})))
        })
        .unwrap();
    assert_eq!(done.state, CommandState::Applied);
    assert_eq!(w.get_outcome(&id).unwrap().state, OutcomeState::Running);
    let ghost = OutcomeId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    let mk = |outcome: OutcomeId, key: &str| NewCommand {
        outcome_id: outcome,
        idempotency_key: key.into(),
        kind: "outcome.start".into(),
        params: json!({}),
        expected_revision: None,
        from_surface: Surface::Cli,
    };
    assert!(store.enqueue_command(mk(ghost, "g")).is_err());
    for key in ["", "has space", &"k".repeat(129)] {
        assert!(
            store.enqueue_command(mk(id.clone(), key)).is_err(),
            "{key:?}"
        );
    }
}

/// F12
#[test]
fn commands_are_bounded_in_size_and_in_queue_length() {
    let e = env();
    let store = e.v5();
    let w = new_outcome(&e, &store);
    let id = w.id().clone();
    let mk = |key: &str, params: Value| NewCommand {
        outcome_id: id.clone(),
        idempotency_key: key.into(),
        kind: "outcome.note".into(),
        params,
        expected_revision: None,
        from_surface: Surface::Cli,
    };
    let big = json!({"x": "a".repeat(MAX_COMMAND_PARAMS_BYTES)});
    assert!(matches!(
        store.enqueue_command(mk("big", big)),
        Err(Error::TooLarge { .. })
    ));
    store
        .enqueue_command(mk("ok", json!({"x": "a".repeat(1000)})))
        .unwrap();
    let db = e.db();
    for i in 0..MAX_QUEUED_COMMANDS {
        db.execute(
            "INSERT INTO outcome_commands(id,outcome_id,idempotency_key,kind,params,from_surface,state,created_at) \
             VALUES (?1,?2,?3,'outcome.note','{}','cli','queued','t')",
            params![format!("{i:08x}-0000-4000-8000-000000000000"), id.as_str(), format!("fill{i}")],
        )
        .unwrap();
    }
    let err = store
        .enqueue_command(mk("one-too-many", json!({})))
        .unwrap_err();
    assert!(matches!(err, Error::QuotaExceeded { .. }), "{err}");
    // a retry of a command that is already stored still answers
    assert_eq!(
        store
            .enqueue_command(mk("ok", json!({"x": "a".repeat(1000)})))
            .unwrap()
            .idempotency_key,
        "ok"
    );
    assert_eq!(
        w.queued_commands(&id).unwrap().len(),
        LIST_LIMIT,
        "every list is bounded"
    );
}

#[test]
fn a_surface_cannot_pose_as_another_in_the_database() {
    let e = env();
    let store = e.v5();
    let w = new_outcome(&e, &store);
    let r = e.db().execute(
        "INSERT INTO outcome_commands(id,outcome_id,idempotency_key,kind,params,from_surface,state,created_at) \
         VALUES ('x',?1,'k','outcome.start','{}','attempt','queued','t')",
        params![w.id().as_str()],
    );
    assert!(r.is_err(), "only cli, tui and superagent exist");
}

// ----------------------------------------------------------------------- outbox

struct World<'a> {
    log: &'a RefCell<Vec<String>>,
    probe_sees_it: bool,
    fail: bool,
}

impl OutboxEffect for World<'_> {
    fn probe(&mut self, row: &OutboxRecord) -> Result<Option<String>, Error> {
        Ok(self
            .log
            .borrow()
            .contains(&row.idempotency_key)
            .then(|| "probed".to_string())
            .filter(|_| self.probe_sees_it))
    }
    fn run(&mut self, row: &OutboxRecord) -> Result<String, String> {
        if self.fail {
            return Err("disk full".into());
        }
        self.log.borrow_mut().push(row.idempotency_key.clone());
        Ok(format!("receipt-{}", row.idempotency_key))
    }
}

fn with_worktree_effect(key: &str) -> Effects {
    Effects {
        events: vec![],
        outbox: vec![NewOutbox {
            idempotency_key: key.into(),
            kind: OutboxKind::WorktreeCreate,
            payload: json!({"path": "wt/1"}),
        }],
    }
}

fn ready_for_attempt(e: &Env, store: &Store) -> (OutcomeWriter, WorkItemId) {
    let (mut w, items) = ready_outcome(e, store);
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "implement").clone();
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
    ] {
        item_to(&mut w, &it, to).unwrap();
    }
    e.child(store, "att-1", w.control_session_id());
    (w, it)
}

#[test]
fn a_crash_between_commit_and_dispatch_replays_the_effect_exactly_once() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    let (_, new) = w
        .new_attempt(
            &it,
            1,
            "att-1",
            "local-inproc",
            &with_worktree_effect("wt-create:att-1"),
        )
        .unwrap();
    assert_eq!(new, [true]);
    let rows = w.list_outbox(&id).unwrap();
    assert_eq!((rows.len(), rows[0].state), (1, OutboxState::Pending));
    drop(w); // the process dies before dispatching anything

    let log = RefCell::new(vec![]);
    let now = OffsetDateTime::now_utc();
    let mut w = store.open_outcome(&id, &policy()).unwrap();
    let run = w
        .run_outbox(
            now,
            &mut World {
                log: &log,
                probe_sees_it: true,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!((run.done, run.reconciled), (1, 0));
    assert_eq!(*log.borrow(), ["wt-create:att-1"]);
    let row = &w.list_outbox(&id).unwrap()[0];
    assert_eq!(
        (row.state, row.receipt.as_deref()),
        (OutboxState::Done, Some("receipt-wt-create:att-1"))
    );
    let run = w
        .run_outbox(
            now,
            &mut World {
                log: &log,
                probe_sees_it: true,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!(run, OutboxRun::default());
    assert_eq!(log.borrow().len(), 1);
}

#[test]
fn a_crash_after_the_effect_but_before_its_receipt_is_reconciled_not_repeated() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    w.new_attempt(
        &it,
        1,
        "att-1",
        "local-inproc",
        &with_worktree_effect("wt-create:att-1"),
    )
    .unwrap();
    let log = RefCell::new(vec![]);
    let claimed = w.claim_outbox(OffsetDateTime::now_utc()).unwrap().unwrap();
    assert_eq!(
        (claimed.state, claimed.tries),
        (OutboxState::Dispatching, 1)
    );
    log.borrow_mut().push(claimed.idempotency_key.clone());
    drop(w);
    let mut w = store.open_outcome(&id, &policy()).unwrap();
    let run = w
        .run_outbox(
            OffsetDateTime::now_utc(),
            &mut World {
                log: &log,
                probe_sees_it: true,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!(
        (run.done, run.reconciled),
        (0, 1),
        "the probe found the worktree"
    );
    assert_eq!(log.borrow().len(), 1, "the effect did not run twice");
    assert_eq!(w.list_outbox(&id).unwrap()[0].state, OutboxState::Done);
}

/// F8: an interrupted dispatch is a consumed try, with backoff and the cap.
#[test]
fn a_dispatching_row_whose_effect_never_happened_backs_off_then_runs_again() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    w.new_attempt(&it, 1, "att-1", "local-inproc", &with_worktree_effect("k"))
        .unwrap();
    w.claim_outbox(OffsetDateTime::now_utc()).unwrap().unwrap();
    drop(w);
    let log = RefCell::new(vec![]);
    let mut w = store.open_outcome(&id, &policy()).unwrap();
    let now = OffsetDateTime::now_utc();
    let run = w
        .run_outbox(
            now,
            &mut World {
                log: &log,
                probe_sees_it: true,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!(
        (run.done, run.reconciled),
        (0, 0),
        "not due yet: backoff applies to the interrupted try"
    );
    let row = w.list_outbox(&id).unwrap().remove(0);
    assert_eq!((row.state, row.tries), (OutboxState::Pending, 1));
    assert!(row.next_at.is_some());
    let run = w
        .run_outbox(
            now + time::Duration::minutes(5),
            &mut World {
                log: &log,
                probe_sees_it: true,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!(run.done, 1);
    assert_eq!(*log.borrow(), ["k"]);
    assert_eq!(w.list_outbox(&id).unwrap()[0].tries, 2);
}

#[test]
fn restart_applies_the_try_cap_to_rows_that_keep_dying() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    w.new_attempt(&it, 1, "att-1", "local-inproc", &with_worktree_effect("k"))
        .unwrap();
    drop(w);
    let log = RefCell::new(vec![]);
    let mut now = OffsetDateTime::now_utc();
    for _ in 0..OUTBOX_MAX_TRIES {
        let mut w = store.open_outcome(&id, &policy()).unwrap();
        now += time::Duration::hours(1);
        if w.claim_outbox(now).unwrap().is_none() {
            break;
        }
        drop(w); // dies right after claiming, every time
        let mut w = store.open_outcome(&id, &policy()).unwrap();
        w.run_outbox(
            now,
            &mut World {
                log: &log,
                probe_sees_it: false,
                fail: false,
            },
        )
        .unwrap();
        now += time::Duration::hours(1);
        let _ = w;
    }
    let w = store.open_outcome(&id, &policy()).unwrap();
    let row = &w.list_outbox(&id).unwrap()[0];
    assert_eq!(
        (row.state, row.tries),
        (OutboxState::Failed, OUTBOX_MAX_TRIES)
    );
    assert!(log.borrow().is_empty());
}

#[test]
fn only_a_dispatching_row_can_be_completed_or_failed() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    w.new_attempt(&it, 1, "att-1", "local-inproc", &with_worktree_effect("k"))
        .unwrap();
    let rid = w.list_outbox(&id).unwrap()[0].id.clone();
    // pending: neither is allowed
    assert!(w.complete_outbox(&rid, "forged receipt").is_err());
    assert!(w.fail_outbox(&rid, OffsetDateTime::now_utc(), "x").is_err());
    let row = w.claim_outbox(OffsetDateTime::now_utc()).unwrap().unwrap();
    w.complete_outbox(&row.id, "ok").unwrap();
    // done: a late completion or failure changes nothing
    assert!(w.complete_outbox(&rid, "again").is_err());
    assert!(
        w.fail_outbox(&rid, OffsetDateTime::now_utc(), "late")
            .is_err()
    );
    let row = w.list_outbox(&id).unwrap().remove(0);
    assert_eq!(
        (row.state, row.receipt.as_deref()),
        (OutboxState::Done, Some("ok"))
    );
}

/// F6
#[test]
fn an_outbox_key_means_one_effect_and_the_duplicate_is_reported() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    let fx = with_worktree_effect("same");
    let (_, new) = w.new_attempt(&it, 1, "att-1", "local-inproc", &fx).unwrap();
    assert_eq!(new, [true]);
    // the same effect again (a replayed transition) reports `false`, nothing doubles
    let r = w
        .transition_work_item(&it, item_rev(&w, &it), WorkItemState::Verifying, &fx)
        .unwrap();
    assert_eq!(r.outbox_new, [false]);
    assert_eq!(w.list_outbox(&id).unwrap().len(), 1);
    // the same key for a different payload or kind is an error and rolls the transition back
    for other in [
        NewOutbox {
            payload: json!({"path": "elsewhere"}),
            ..fx.outbox[0].clone()
        },
        NewOutbox {
            kind: OutboxKind::WorktreeCleanup,
            ..fx.outbox[0].clone()
        },
    ] {
        let before = item_rev(&w, &it);
        let err = w
            .transition_work_item(
                &it,
                before,
                WorkItemState::RetryableFailure,
                &Effects {
                    events: vec![],
                    outbox: vec![other],
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("different effect"), "{err}");
        assert_eq!(item_rev(&w, &it), before);
    }
    // keys are per outcome: another outcome may use the same key
    let w2 = new_outcome(&e, &store);
    assert!(
        e.db()
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE idempotency_key='same'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
            == 1
    );
    let _ = w2;
}

/// F12
#[test]
fn a_malformed_outbox_row_is_skipped_with_a_diagnostic_not_fatal() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    w.new_attempt(
        &it,
        1,
        "att-1",
        "local-inproc",
        &with_worktree_effect("good"),
    )
    .unwrap();
    let db = e.db();
    // a pending row of an unknown kind, ahead of the good one, and a dispatching one
    db.execute(
        "INSERT INTO outbox(id,outcome_id,idempotency_key,kind,payload,state) VALUES ('bad1',?1,'bad1','bogus.kind','{}','pending')",
        [id.as_str()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO outbox(id,outcome_id,idempotency_key,kind,payload,state,tries) VALUES ('bad2',?1,'bad2','bogus.kind','{}','dispatching',1)",
        [id.as_str()],
    )
    .unwrap();
    db.execute("UPDATE outbox SET rowid=rowid WHERE id='good'", [])
        .unwrap();
    let log = RefCell::new(vec![]);
    let run = w
        .run_outbox(
            OffsetDateTime::now_utc(),
            &mut World {
                log: &log,
                probe_sees_it: false,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!(run.done, 1, "the good row still ran");
    assert!(
        run.skipped.iter().any(|s| s.contains("bad2")),
        "{:?}",
        run.skipped
    );
    let state: String = db
        .query_row("SELECT state FROM outbox WHERE id='bad1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        state, "failed",
        "the unreadable pending row is parked, not retried forever"
    );
    let receipt: String = db
        .query_row("SELECT receipt FROM outbox WHERE id='bad1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(receipt.contains("malformed"), "{receipt}");
}

#[test]
fn failures_back_off_and_stop_after_the_cap() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    w.new_attempt(
        &it,
        1,
        "att-1",
        "local-inproc",
        &with_worktree_effect("same"),
    )
    .unwrap();
    let log = RefCell::new(vec![]);
    let mut now = OffsetDateTime::now_utc();
    let r = w
        .run_outbox(
            now,
            &mut World {
                log: &log,
                probe_sees_it: false,
                fail: true,
            },
        )
        .unwrap();
    assert_eq!((r.done, r.retry_later, r.failed), (0, 1, 0));
    let row = w.list_outbox(&id).unwrap().remove(0);
    assert_eq!(row.state, OutboxState::Pending);
    assert!(row.next_at.is_some());
    assert!(w.claim_outbox(now).unwrap().is_none(), "not due yet");
    for _ in 0..OUTBOX_MAX_TRIES {
        now += time::Duration::hours(1);
        w.run_outbox(
            now,
            &mut World {
                log: &log,
                probe_sees_it: false,
                fail: true,
            },
        )
        .unwrap();
    }
    let row = w.list_outbox(&id).unwrap().remove(0);
    assert_eq!(
        (row.state, row.tries),
        (OutboxState::Failed, OUTBOX_MAX_TRIES)
    );
    now += time::Duration::hours(1);
    let r = w
        .run_outbox(
            now,
            &mut World {
                log: &log,
                probe_sees_it: false,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!(
        r,
        OutboxRun::default(),
        "a failed row is not retried by itself"
    );
    assert!(log.borrow().is_empty());
}

// --------------------------------------------------------------------- F14 plan

#[test]
fn a_plan_is_stored_only_in_the_right_state_as_the_next_version_for_an_approved_contract() {
    let e = env();
    let store = e.v5();
    let mut w = new_outcome(&e, &store);
    // draft: refused
    assert!(
        w.insert_plan(&plan())
            .unwrap_err()
            .to_string()
            .contains("cannot be stored while")
    );
    go(&mut w, OutcomeState::ContractPending);
    // unapproved contract: refused
    assert!(
        w.insert_plan(&plan())
            .unwrap_err()
            .to_string()
            .contains("approved current contract")
    );
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    // version must follow the current one
    let mut v2 = plan();
    v2.version = 2;
    assert!(
        w.insert_plan(&v2)
            .unwrap_err()
            .to_string()
            .contains("follow")
    );
    let mut v0 = plan();
    v0.version = 0;
    assert!(w.insert_plan(&v0).is_err());
    w.insert_plan(&plan()).unwrap();
    assert!(w.insert_plan(&plan()).is_err(), "the same version twice");
    // ready: no more plans
    go(&mut w, OutcomeState::Ready);
    assert!(
        w.insert_plan(&v2)
            .unwrap_err()
            .to_string()
            .contains("cannot be stored while")
    );
    // blocked: a re-plan as the next version is allowed
    go(&mut w, OutcomeState::Running);
    go(&mut w, OutcomeState::Blocked);
    w.insert_plan(&v2).unwrap();
    assert_eq!(w.get_outcome(w.id()).unwrap().plan_version, Some(2));
    // an invalid plan stores nothing
    let mut bad = plan();
    bad.version = 3;
    bad.items[2].requires.push("ghost".into());
    assert!(w.insert_plan(&bad).is_err());
    assert_eq!(w.list_work_items(w.id()).unwrap().len(), 6);
}
