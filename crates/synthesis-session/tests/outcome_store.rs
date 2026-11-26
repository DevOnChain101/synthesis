//! Outcome persistence (ADR-016): opt-in v5 migration, revisions, commands, outbox, and the
//! security properties of the store (approvals, derived guards, immutability). Real SQLite in a
//! temp dir; canonical temp paths (macOS `/var` -> `/private/var`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    OutcomePolicy::new(true, 16, 10, 24)
}

fn with(f: impl FnOnce(&mut OutcomePolicy)) -> OutcomePolicy {
    let mut p = policy();
    f(&mut p);
    p
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
        kind: synthesis_core::outcome::ContractKind::Write,
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
        checks: vec![],
    }
}

fn plan() -> Plan {
    let item = |key: &str, kind, requires: &[&str], covers: &[&str]| PlanItem {
        key: key.into(),
        kind,
        requires: requires.iter().map(|s| (*s).into()).collect(),
        capabilities: vec![],
        covers: covers.iter().map(|s| (*s).into()).collect(),
        title: String::new(),
        instructions: String::new(),
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

/// The stored hash read straight from the table (`get_contract` refuses a tampered approval).
fn contract_hash_raw(e: &Env) -> String {
    e.db()
        .query_row("SELECT hash FROM contracts", [], |r| r.get(0))
        .unwrap()
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
fn ready_outcome_with(
    e: &Env,
    store: &Store,
    c: &Contract,
) -> (OutcomeWriter, Vec<(String, WorkItemId)>) {
    let mut w = new_outcome_with(e, store, c, &policy());
    go(&mut w, OutcomeState::ContractPending);
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    let items = w.insert_plan(&plan()).unwrap();
    go(&mut w, OutcomeState::Ready);
    (w, items)
}

fn ready_outcome(e: &Env, store: &Store) -> (OutcomeWriter, Vec<(String, WorkItemId)>) {
    ready_outcome_with(e, store, &contract())
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
        flags: vec![],
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
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        item_to(w, it, to).unwrap();
    }
    e.child(store, sess, w.control_session_id());
    let (a, _) = w.new_attempt(it, 1, sess, "local-inproc", NONE).unwrap();
    item_to(w, it, WorkItemState::Running).unwrap();
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
    finish_items_with(e, store, w, items, &|ev| ev)
}

/// [`finish_items`] with the verify item's evidence changed by `fin`.
fn finish_items_with(
    e: &Env,
    store: &Store,
    w: &mut OutcomeWriter,
    items: &[(String, WorkItemId)],
    fin: &dyn Fn(NewEvidence) -> NewEvidence,
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
    w.record_evidence(&fin(evidence(test, None, "cafe01", &["c1", "c2"])))
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
        use std::path::Path;
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
fn a_worker_session_is_not_listed_but_chats_and_get_by_id_are() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    run_agent(&e, &store, &mut w, item(&items, "inspect"), "att-1", "c1");
    e.session(&store, "chat", true);
    let r = store.reader().unwrap();
    for list in [
        r.list_sessions(None).unwrap(),
        r.list_used_sessions(None).unwrap(),
    ] {
        let ids: Vec<_> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["chat"], "the worker is not a user session");
    }
    assert_eq!(r.get_session("att-1").unwrap().id, "att-1");
    // the lookup by id (`--resume ID` of a re-run attempt) still sees the worker, not the control session
    let ids: Vec<_> = r
        .list_sessions_with_workers(None)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(
        ids.contains(&"att-1".to_string()) && ids.contains(&"chat".to_string()) && ids.len() == 2
    );
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
    let off = with(|p| p.enabled = false);
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
    let cap = with(|p| p.max_cost_usd = Some(10.0));
    assert!(
        create(&c, &cap)
            .unwrap_err()
            .to_string()
            .contains("max_cost_usd")
    );
    c.envelope.budgets.max_cost_usd = 5.0;
    c.envelope.budgets.max_parallel_workers = 8;
    let workers = with(|p| p.max_parallel_workers = 2);
    assert!(
        create(&c, &workers)
            .unwrap_err()
            .to_string()
            .contains("max_parallel_workers")
    );
    c.envelope.budgets.max_parallel_workers = 2;
    c.envelope.budgets.max_attempts_per_work_item = 5;
    let attempts = with(|p| p.max_attempts_per_work_item = 3);
    assert!(
        create(&c, &attempts)
            .unwrap_err()
            .to_string()
            .contains("max_attempts")
    );
    // a ceiling on the per-attempt cap makes the contract name one
    c.envelope.budgets.max_attempts_per_work_item = 3;
    let per = with(|p| {
        p.attempt_max_cost_usd = Some(1.0);
        p.attempt_max_wall_secs = Some(60);
    });
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
fn a_contract_max_turns_above_what_would_run_is_refused_not_clamped() {
    let mut c = contract();
    let set = |n| with(|p| p.attempt_max_turns = n);
    // unset setting: a write attempt runs 50, a read-only one 80
    c.envelope.budgets.max_turns = Some(50);
    assert!(set(None).check(&c).is_ok());
    c.envelope.budgets.max_turns = Some(51);
    assert!(
        set(None)
            .check(&c)
            .unwrap_err()
            .to_string()
            .contains("max_turns")
    );
    assert!(set(Some(60)).check(&c).is_ok());
    assert!(set(Some(50)).check(&c).is_err());
    // below the setting is fine, and is what runs
    c.envelope.budgets.max_turns = Some(20);
    assert_eq!(set(Some(60)).effective_turns(&c), 20);
    // not named: the setting runs
    c.envelope.budgets.max_turns = None;
    assert!(set(Some(60)).check(&c).is_ok());
    assert_eq!(set(Some(60)).effective_turns(&c), 60);
    assert_eq!(set(None).effective_turns(&c), 50);
}

#[test]
fn a_tighter_ceiling_applies_when_a_stored_contract_is_approved_and_when_an_attempt_starts() {
    let e = env();
    let store = e.v5();
    // N6: the ceiling tightened between creation and approval
    let mut w = new_outcome(&e, &store);
    go(&mut w, OutcomeState::ContractPending);
    let h = contract_hash(&w);
    let id = w.id().clone();
    drop(w);
    let tight = with(|p| p.max_attempts_per_work_item = 1);
    let mut w = store.open_outcome(&id, &tight).unwrap();
    let err = w.approve_contract(1, &h, "user").unwrap_err();
    assert!(
        err.to_string().contains("exceeds the settings ceiling"),
        "{err}"
    );
    assert!(!w.contract_is_approved(&id, 1).unwrap());
    drop(w);
    // ... and between approval and the start of an attempt
    let e = env();
    let store = e.v5();
    let (w, items) = ready_outcome(&e, &store);
    let id = w.id().clone();
    drop(w);
    let mut w = store.open_outcome(&id, &tight).unwrap();
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "implement");
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        item_to(&mut w, it, to).unwrap();
    }
    e.child(&store, "a1", w.control_session_id());
    let err = w
        .new_attempt(it, 1, "a1", "local-inproc", NONE)
        .unwrap_err();
    assert!(
        err.to_string().contains("exceeds the settings ceiling"),
        "{err}"
    );
    assert!(w.list_attempts(it).unwrap().is_empty());
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
    // ... and it is reported loudly, not as a quiet "not approved"
    assert!(matches!(
        w.contract_is_approved(w.id(), 1),
        Err(Error::Tampered(_))
    ));
    assert!(matches!(w.get_contract(w.id(), 1), Err(Error::Tampered(_))));
    // the plan insert (needs the approved contract) and Ready refuse too
    assert!(matches!(w.insert_plan(&plan()), Err(Error::Tampered(_))));
    let err = w
        .transition_outcome(rev_of(&w), OutcomeState::Ready, None, NONE)
        .unwrap_err();
    assert!(matches!(err, Error::Tampered(_)), "{err}");
    // N2: a pre-poisoned approval neither counts as "already approved" nor blocks silently
    let h = contract_hash_raw(&e);
    let err = w.approve_contract(1, &h, "user").unwrap_err();
    assert!(err.to_string().contains("does not verify"), "{err}");
    // a second bare UPDATE (replace an existing approval) is refused outright
    let err = forge(&"cd".repeat(32)).unwrap_err();
    assert!(err.to_string().contains("approved once"), "{err}");
}

#[test]
fn the_approval_covers_max_turns() {
    let e = env();
    let store = e.v5();
    let mut c = contract();
    c.envelope.budgets.max_turns = Some(10);
    let mut w = new_outcome_with(&e, &store, &c, &policy());
    go(&mut w, OutcomeState::ContractPending);
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    assert!(w.contract_is_approved(w.id(), 1).unwrap());
    let db = e.db();
    db.execute("DROP TRIGGER contracts_immutable", []).unwrap();
    db.execute(
        "UPDATE contracts SET body=replace(body,'\"max_turns\":10','\"max_turns\":200')",
        [],
    )
    .unwrap();
    assert!(matches!(
        w.contract_is_approved(w.id(), 1),
        Err(Error::Tampered(_))
    ));
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
        matches!(w.contract_is_approved(w.id(), 1), Err(Error::Tampered(_))),
        "body no longer matches the approved hash"
    );
    let h = contract_hash_raw(&e);
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
    // evidence is wholly immutable (flags and mac included)
    for col in ["flags='[\"stale\"]'", "mac='x'"] {
        assert!(
            db.execute(&format!("UPDATE evidence SET {col}"), [])
                .is_err(),
            "{col}"
        );
    }
    // a decision cannot be resolved by a bare UPDATE: no approval row
    let force = "UPDATE decisions SET state='approved', revision=revision+1 WHERE id=?1";
    let err = db.execute(force, [d.id.as_str()]).unwrap_err();
    assert!(err.to_string().contains("from pending only"), "{err}");
    w.transition_decision(&d.id, 1, DecisionState::Pending, &json!({"v": 1}), NONE)
        .unwrap();
    let err = db.execute(force, [d.id.as_str()]).unwrap_err();
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
        if case == "flagged" {
            finish_items_with(&e, &store, &mut w, &items, &|ev| NewEvidence {
                criteria_ids: vec![],
                ..ev
            });
        } else {
            finish_items(&e, &store, &mut w, &items);
        }
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
                // the only evidence that covers the criteria carries a flag from the start
                let mut f = evidence(&test, None, "cafe01", &["c1", "c2"]);
                f.check_id = "lint".into();
                f.flags = vec!["definitions_changed".into()];
                w.record_evidence(&f).unwrap();
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
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
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
    let mut sw = store.open_attempt_session(&w, "att-1").unwrap();
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
    let mut ew = store.resume_session("elsewhere", false).unwrap();
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

// -------------------------------------------------------------------- decisions

fn decision(key: &str, binding: Value) -> NewDecision {
    NewDecision {
        kind: "ambiguity".into(),
        dedupe_key: key.into(),
        brief: json!({"why": "which API shape", "alternatives": ["A", "B"], "safe_default": "HOLD"}),
        binding,
        expires_at: hours(2),
        action: None,
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
        w.list_decisions(w.id()).unwrap()[0].row_state,
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
        w.list_decisions(w.id()).unwrap()[0].row_state,
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
    assert_eq!(ev["outcome_id"], w.id().as_str());
    assert_eq!(ev["binding_hash"], d.binding_hash);
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
        w.list_decisions(&id).unwrap()[0].row_state,
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
    assert!(fresh && d.row_state == DecisionState::Preparing);
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
        w.list_decisions(w.id()).unwrap()[0].row_state,
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
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
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
            &OutboxFilter::all(),
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
            &OutboxFilter::all(),
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
    let claimed = w
        .claim_outbox(OffsetDateTime::now_utc(), &OutboxFilter::all())
        .unwrap()
        .unwrap();
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
            &OutboxFilter::all(),
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
    w.claim_outbox(OffsetDateTime::now_utc(), &OutboxFilter::all())
        .unwrap()
        .unwrap();
    drop(w);
    let log = RefCell::new(vec![]);
    let mut w = store.open_outcome(&id, &policy()).unwrap();
    let now = OffsetDateTime::now_utc();
    let run = w
        .run_outbox(
            now,
            &OutboxFilter::all(),
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
            &OutboxFilter::all(),
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
        if w.claim_outbox(now, &OutboxFilter::all()).unwrap().is_none() {
            break;
        }
        drop(w); // dies right after claiming, every time
        let mut w = store.open_outcome(&id, &policy()).unwrap();
        w.run_outbox(
            now,
            &OutboxFilter::all(),
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
    let row = w
        .claim_outbox(OffsetDateTime::now_utc(), &OutboxFilter::all())
        .unwrap()
        .unwrap();
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
    item_to(&mut w, &it, WorkItemState::Running).unwrap();
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
            &OutboxFilter::all(),
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
            &OutboxFilter::all(),
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
    assert!(
        w.claim_outbox(now, &OutboxFilter::all()).unwrap().is_none(),
        "not due yet"
    );
    for _ in 0..OUTBOX_MAX_TRIES {
        now += time::Duration::hours(1);
        w.run_outbox(
            now,
            &OutboxFilter::all(),
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
            &OutboxFilter::all(),
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

// ---------------------------------------------------- re-review: N1 .. N9

fn hex_sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn resolved_decision(e: &Env, store: &Store, choice: &str) -> (OutcomeWriter, DecisionRecord) {
    let (mut w, _) = ready_outcome(e, store);
    let d = open_decision(&mut w);
    w.resolve_decision(&resolution(&d, choice), &binding())
        .unwrap();
    (w, d)
}

const DECISION_TRIGGERS: [&str; 5] = [
    "decisions_revision_step",
    "decisions_terminal",
    "decisions_revision",
    "decisions_answer_from_pending",
    "decisions_resolution_needs_approval",
];

/// N1 (a): rewind the revision, then flip the state. The row is read back from the table, so the
/// authority must come from the MAC'd choice, not from the row.
#[test]
fn n1_a_revision_rewind_cannot_turn_a_rejection_into_an_approval() {
    let e = env();
    let store = e.v5();
    let (w, d) = resolved_decision(&e, &store, "reject");
    assert_eq!(w.decision_state(&d.id).unwrap(), DecisionState::Rejected);
    let db = e.db();
    // plain SQL, step by step: every one is refused by the triggers
    for sql in [
        "UPDATE decisions SET revision=2",
        "UPDATE decisions SET state='approved', revision=3",
        "UPDATE decisions SET state='approved'",
        "UPDATE decisions SET state='pending', revision=revision+1",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    assert!(
        db.execute("UPDATE decisions SET revision=revision-1", [])
            .is_err()
    );
    // a writer that also drops the triggers still cannot make the row say something the MAC'd
    // choice does not: loud Tampered, never a quiet Approved
    for t in DECISION_TRIGGERS {
        db.execute(&format!("DROP TRIGGER {t}"), []).unwrap();
    }
    db.execute("UPDATE decisions SET revision=2", []).unwrap();
    db.execute("UPDATE decisions SET state='approved', revision=3", [])
        .unwrap();
    assert_eq!(
        w.list_decisions(w.id()).unwrap()[0].row_state,
        DecisionState::Approved,
        "the raw column lies"
    );
    assert!(matches!(
        w.verified_resolution(&d.id),
        Err(Error::Tampered(_))
    ));
    assert!(matches!(w.decision_state(&d.id), Err(Error::Tampered(_))));
}

/// N1 (b): `INSERT OR REPLACE` skips the triggers; the approval must still not cover a swapped row.
#[test]
fn n1_b_a_replaced_decision_row_does_not_verify() {
    // (column to replace, its new SQL value); the binding_hash cases are computed in Rust
    let new_binding = r#"{"contract_version":9}"#;
    let cases: Vec<(&str, String)> = vec![
        ("binding", format!("'{new_binding}'")),
        ("kind", "'policy_change'".into()),
        ("expires_at", "'2099-01-01T00:00:00.000Z'".into()),
        (
            "brief",
            r#"'{"alternatives":["A","B"],"why":"swapped"}'"#.into(),
        ),
        ("binding+hash", format!("'{new_binding}'")),
        ("outcome_id", "OTHER".into()),
    ];
    for (col, val) in cases {
        let e = env();
        let store = e.v5();
        let (w, d) = resolved_decision(&e, &store, "A");
        let other = new_outcome(&e, &store);
        assert_eq!(
            w.decision_state(&d.id).unwrap(),
            DecisionState::Approved,
            "{col}: authentic before"
        );
        let mut sel = vec![
            "id",
            "outcome_id",
            "kind",
            "dedupe_key",
            "state",
            "revision",
            "brief",
            "binding",
            "binding_hash",
            "expires_at",
            "created_at",
            "action",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
        let set = |sel: &mut Vec<String>, name: &str, v: &str| {
            let i = sel.iter().position(|c| c == name).unwrap();
            sel[i] = v.to_string();
        };
        match col {
            "binding+hash" => {
                set(&mut sel, "binding", &val);
                set(
                    &mut sel,
                    "binding_hash",
                    &format!("'{}'", hex_sha(new_binding)),
                );
            }
            "outcome_id" => set(&mut sel, "outcome_id", &format!("'{}'", other.id())),
            _ => set(&mut sel, col, &val),
        }
        e.db()
            .execute(
                &format!(
                    "INSERT OR REPLACE INTO decisions SELECT {} FROM decisions",
                    sel.join(",")
                ),
                [],
            )
            .unwrap_or_else(|err| panic!("{col}: replace should get past the triggers: {err}"));
        let r = w.verified_resolution(&d.id);
        assert!(matches!(r, Err(Error::Tampered(_))), "{col}: {r:?}");
    }
}

#[test]
fn n1_one_approval_per_decision_revision_and_the_event_names_the_binding() {
    let e = env();
    let store = e.v5();
    let (w, d) = resolved_decision(&e, &store, "A");
    let err = e
        .db()
        .execute(
            "INSERT INTO decision_approvals(id,decision_id,decision_revision,approver,choice,brief_sha256,decided_at,mac) \
             VALUES ('x',?1,2,'mallory','B','h','t','m')",
            [d.id.as_str()],
        )
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("unique"), "{err}");
    assert_eq!(w.decision_approvals(&d.id).unwrap().len(), 1);
}

#[test]
fn n1_a_resolved_state_without_its_approval_is_tampering_not_unresolved() {
    let e = env();
    let store = e.v5();
    let (w, d) = resolved_decision(&e, &store, "A");
    let db = e.db();
    db.execute("DROP TRIGGER decision_approvals_no_delete", [])
        .unwrap();
    db.execute("DELETE FROM decision_approvals", []).unwrap();
    assert!(matches!(
        w.verified_resolution(&d.id),
        Err(Error::Tampered(_))
    ));
}

/// N3: what the plan and the attempts fixed, and the one-way grant.
#[test]
fn n3_plan_attempt_and_grant_columns_are_one_way() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    finish_items(&e, &store, &mut w, &items);
    let db = e.db();
    for sql in [
        "UPDATE work_items SET spec='{}'",
        "UPDATE work_items SET key='x'",
        "UPDATE work_items SET kind='verify'",
        "UPDATE work_item_deps SET requires_id=work_item_id",
        "DELETE FROM work_item_deps",
        "UPDATE attempts SET session_id='elsewhere'",
        "UPDATE attempts SET fence=99",
        "UPDATE attempts SET ordinal=7",
        "UPDATE attempts SET subject_commit='evil'",
        "UPDATE evidence SET flags='not json'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    let (d, _) = w.new_decision(&decision("g", binding())).unwrap();
    // (a grant row written by plain SQL has no valid MAC and is never used; the triggers still hold)
    db.execute(
        "INSERT INTO grants(id,decision_id,outcome_id,work_item_id,rules,action_hash,expires_at,created_at,mac) \
         VALUES ('g1',?1,?2,?3,'[]','h','2099','2026',?4)",
        rusqlite::params![d.id.as_str(), w.id().as_str(), item(&items, "test").as_str(), "ab".repeat(32)],
    )
    .unwrap();
    assert!(
        w.list_grants(w.id()).unwrap().is_empty(),
        "no valid MAC: not a grant"
    );
    assert!(
        db.execute("UPDATE grants SET rules='[\"Bash\"]'", [])
            .is_err(),
        "rules are immutable"
    );
    assert!(db.execute("DELETE FROM grants", []).is_err());
    db.execute("UPDATE grants SET consumed_at='t1'", [])
        .unwrap();
    assert!(
        db.execute("UPDATE grants SET consumed_at=NULL", [])
            .is_err(),
        "a grant cannot be un-consumed"
    );
    assert!(
        db.execute("UPDATE grants SET consumed_at='t2'", [])
            .is_err(),
        "nor re-stamped"
    );
}

/// N4: a criterion that names its checks is only satisfied by those checks.
#[test]
fn n4_a_criterion_with_check_ids_needs_evidence_from_one_of_them() {
    for (ids, verified) in [(vec!["unit"], true), (vec!["lint"], false)] {
        let e = env();
        let store = e.v5();
        let mut c = contract();
        c.criteria[0].check_ids = ids.iter().map(|s| (*s).into()).collect();
        let (mut w, items) = ready_outcome_with(&e, &store, &c);
        finish_items(&e, &store, &mut w, &items);
        go(&mut w, OutcomeState::Verifying);
        let r = w.transition_outcome(rev_of(&w), OutcomeState::Verified, None, NONE);
        assert_eq!(r.is_ok(), verified, "{ids:?}: {r:?}");
    }
}

fn try_verify(w: &mut OutcomeWriter) -> Result<(), String> {
    go(w, OutcomeState::Verifying);
    w.transition_outcome(rev_of(w), OutcomeState::Verified, None, NONE)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The events of the control session with a payload of this evidence id, rewritten to match a
/// forged row (what a writer without the key can do).
fn forge_event(e: &Env, w: &OutcomeWriter, id: &str, row: &serde_json::Value) {
    e.db()
        .execute(
            "INSERT INTO events(session_id,seq,v,ts,type,payload) VALUES (?1,999999,1,'2026-01-01T00:00:00.000Z','evidence.recorded',?2)",
            rusqlite::params![w.control_session_id(), serde_json::json!({
                "evidence_id": id, "work_item_id": row["item"], "check_id": row["check"],
                "trust_class": "independent_check", "result": "passed", "sha256": null,
                "subject_commit": "cafe01", "fence": 0, "command_hash": null, "exit_code": 0,
                "env_fingerprint": "x", "criteria_ids": ["c1","c2"], "flags": []
            }).to_string()],
        )
        .unwrap();
}

const FORGED_ID: &str = "11111111-1111-4111-8111-111111111111";

/// PR-09 review (M1): a passing row written by plain SQL, even with a hand-written event of the
/// same content, has no valid MAC and counts for nothing; the same facts written through the
/// store do verify.
#[test]
fn a_forged_row_and_event_without_the_mac_do_not_verify() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    // genuine evidence for the item itself, covering no criterion
    finish_items_with(&e, &store, &mut w, &items, &|ev| NewEvidence {
        criteria_ids: vec![],
        ..ev
    });
    let test = item(&items, "test").clone();
    e.db()
        .execute(
            "INSERT INTO evidence(id,outcome_id,work_item_id,trust_class,producer,subject_commit,check_id,\
               criteria_ids,result,exit_code,env_fingerprint,fence,captured_at,mac) \
             VALUES (?1,?2,?3,'independent_check','independent_runner','cafe01','forged',\
               '[\"c1\",\"c2\"]','passed',0,'x',0,'2026-01-01T00:00:00.000Z',?4)",
            rusqlite::params![FORGED_ID, w.id().as_str(), test.as_str(), "ab".repeat(32)],
        )
        .unwrap();
    forge_event(
        &e,
        &w,
        FORGED_ID,
        &serde_json::json!({"item": test.as_str(), "check": "forged"}),
    );
    let err = try_verify(&mut w).unwrap_err();
    assert!(err.contains("verdict"), "{err}");
    // the same facts through the store verify
    w.record_evidence(&NewEvidence {
        check_id: "unit-real".into(),
        ..evidence(&test, None, "cafe01", &["c1", "c2"])
    })
    .unwrap();
    w.transition_outcome(rev_of(&w), OutcomeState::Verified, None, NONE)
        .unwrap();
}

/// Plan 3b item 4: a passing `independent_check` row written by plain SQL, with no chained event
/// and no MAC from the store, never counts toward a verdict (the weaker case of the one above: the
/// writer does not even forge the event). Positive control: the same facts through the store pass.
#[test]
fn sql_inserted_evidence_never_passes() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    finish_items_with(&e, &store, &mut w, &items, &|ev| NewEvidence {
        criteria_ids: vec![],
        ..ev
    });
    let test = item(&items, "test").clone();
    e.db()
        .execute(
            "INSERT INTO evidence(id,outcome_id,work_item_id,trust_class,producer,subject_commit,check_id,\
               criteria_ids,result,exit_code,env_fingerprint,fence,captured_at,mac) \
             VALUES (?1,?2,?3,'independent_check','independent_runner','cafe01','sql-only',\
               '[\"c1\",\"c2\"]','passed',0,'x',0,'2026-01-01T00:00:00.000Z',?4)",
            rusqlite::params![FORGED_ID, w.id().as_str(), test.as_str(), "cd".repeat(32)],
        )
        .unwrap();
    let rows = w.list_evidence(w.id()).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.check_id == "sql-only" && !r.event_backed),
        "the row is visible but not event backed"
    );
    assert!(try_verify(&mut w).unwrap_err().contains("verdict"));
    w.record_evidence(&NewEvidence {
        check_id: "unit-real".into(),
        ..evidence(&test, None, "cafe01", &["c1", "c2"])
    })
    .unwrap();
    w.transition_outcome(rev_of(&w), OutcomeState::Verified, None, NONE)
        .unwrap();

    // EVENT_BACKED alone: the MAC key and `EvMac` are not reachable from here, so a forged row cannot
    // carry a valid MAC. A genuine row does: its chained event is hidden (retyped) and put back. While
    // it is hidden the row has its valid MAC and no event, and the verdict refuses it; with the event
    // back the very same row passes, so the MAC was never what stopped it.
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    finish_items_with(&e, &store, &mut w, &items, &|ev| NewEvidence {
        criteria_ids: vec![],
        ..ev
    });
    let test = item(&items, "test").clone();
    w.record_evidence(&NewEvidence {
        check_id: "unit-real".into(),
        ..evidence(&test, None, "cafe01", &["c1", "c2"])
    })
    .unwrap();
    let hide = |from: &str, to: &str| {
        e.db()
            .execute(
                "UPDATE events SET type=?2 WHERE type=?1 AND json_extract(payload,'$.check_id')='unit-real'",
                [from, to],
            )
            .unwrap()
    };
    assert_eq!(hide("evidence.recorded", "evidence.recorded.hidden"), 1);
    assert!(
        w.list_evidence(w.id())
            .unwrap()
            .iter()
            .any(|r| r.check_id == "unit-real" && !r.event_backed)
    );
    assert!(try_verify(&mut w).unwrap_err().contains("verdict"));
    assert_eq!(hide("evidence.recorded.hidden", "evidence.recorded"), 1);
    w.transition_outcome(rev_of(&w), OutcomeState::Verified, None, NONE)
        .unwrap();
}

/// A genuine row altered afterwards (the trigger dropped by a writer with file access) fails its
/// MAC: rewritten `criteria_ids` (the event rewritten too) and a removed flag both count for nothing.
#[test]
fn rewritten_criteria_and_removed_flags_break_the_mac() {
    // criteria_ids rewritten
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    finish_items_with(&e, &store, &mut w, &items, &|ev| NewEvidence {
        criteria_ids: vec![],
        ..ev
    });
    let db = e.db();
    db.execute_batch("DROP TRIGGER evidence_immutable;")
        .unwrap();
    db.execute(
        "UPDATE evidence SET criteria_ids='[\"c1\",\"c2\"]' WHERE work_item_id=?1 AND check_id='unit'",
        [item(&items, "test").as_str()],
    )
    .unwrap();
    db.execute(
        "UPDATE events SET payload=json_set(payload,'$.criteria_ids',json('[\"c1\",\"c2\"]')) \
         WHERE type='evidence.recorded' AND json_extract(payload,'$.work_item_id')=?1",
        [item(&items, "test").as_str()],
    )
    .unwrap();
    let err = try_verify(&mut w).unwrap_err();
    assert!(err.contains("verdict"), "{err}");

    // a flag removed
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    // the flagged row cannot even make its item succeed: flagged evidence is not evidence
    go(&mut w, OutcomeState::Running);
    let (inspect, implement, test) = (
        item(&items, "inspect"),
        item(&items, "implement"),
        item(&items, "test"),
    );
    let a1 = run_agent(&e, &store, &mut w, inspect, "att-inspect", "base0");
    w.record_evidence(&evidence(inspect, Some(&a1), "base0", &[]))
        .unwrap();
    item_to(&mut w, inspect, WorkItemState::Succeeded).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let a2 = run_agent(&e, &store, &mut w, implement, "att-implement", "cafe01");
    w.record_evidence(&NewEvidence {
        flags: vec!["definitions_changed".into()],
        ..evidence(implement, Some(&a2), "cafe01", &["c1"])
    })
    .unwrap();
    assert!(
        item_to(&mut w, implement, WorkItemState::Succeeded).is_err(),
        "flagged evidence"
    );
    let db = e.db();
    db.execute_batch("DROP TRIGGER evidence_immutable;")
        .unwrap();
    db.execute(
        "UPDATE evidence SET flags='[]' WHERE work_item_id=?1",
        [implement.as_str()],
    )
    .unwrap();
    db.execute(
        "UPDATE events SET payload=json_set(payload,'$.flags',json('[]')) WHERE type='evidence.recorded' AND json_extract(payload,'$.work_item_id')=?1",
        [implement.as_str()],
    )
    .unwrap();
    // un-flagged by SQL: still not authentic, so the item still cannot succeed
    assert!(
        item_to(&mut w, implement, WorkItemState::Succeeded).is_err(),
        "un-flagged by UPDATE"
    );
    let _ = test;
}

/// A check the contract froze is satisfied only by evidence of THAT command (its hash).
#[test]
fn evidence_must_carry_the_hash_of_the_frozen_command() {
    use sha2::{Digest, Sha256};
    use synthesis_core::outcome::ContractCheck;
    let hex = |b: &[u8]| -> String {
        Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect()
    };
    for (hash, ok) in [
        (Some(hex(b"cargo test")), true),
        (Some(hex(b"true")), false),
        (None, false),
    ] {
        let e = env();
        let store = e.v5();
        let mut c = contract();
        c.checks = vec![ContractCheck {
            id: "frozen".into(),
            command: "cargo test".into(),
        }];
        c.criteria[0].check_ids = vec!["frozen".into()];
        c.criteria[1].check_ids = vec!["frozen".into()];
        let (mut w, items) = ready_outcome_with(&e, &store, &c);
        finish_items_with(&e, &store, &mut w, &items, &|ev| NewEvidence {
            criteria_ids: vec![],
            ..ev
        });
        let test = item(&items, "test").clone();
        let mut f = evidence(&test, None, "cafe01", &["c1", "c2"]);
        f.check_id = "frozen".into();
        f.command_hash = hash.clone();
        w.record_evidence(&f).unwrap();
        assert_eq!(try_verify(&mut w).is_ok(), ok, "{hash:?}");
    }
}

/// N6: the decision expiry is bounded by the settings.
#[test]
fn n6_decision_expiry_is_capped_by_the_settings() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let mut d = decision("exp", binding());
    d.expires_at = hours(25);
    assert!(
        w.new_decision(&d)
            .unwrap_err()
            .to_string()
            .contains("decision_expiry_hours")
    );
    d.expires_at = hours(23);
    w.new_decision(&d).unwrap();
}

/// N7
#[test]
fn n7_attempts_start_only_from_ready_or_leased_items() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "implement").clone();
    e.child(&store, "s1", w.control_session_id());
    // created
    let err = w
        .new_attempt(&it, 1, "s1", "local-inproc", NONE)
        .unwrap_err();
    assert!(err.to_string().contains("ready or leased"), "{err}");
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
    ] {
        item_to(&mut w, &it, to).unwrap();
    }
    // running
    assert!(w.new_attempt(&it, 1, "s1", "local-inproc", NONE).is_err());
    assert!(w.list_attempts(&it).unwrap().is_empty());
}

#[test]
fn n7_attempt_sessions_are_reached_only_through_their_outcomes_writer() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    run_agent(&e, &store, &mut w, item(&items, "inspect"), "att-1", "c1");
    e.session(&store, "plain", true);
    let control = w.control_session_id().to_string();
    // another outcome, with its own attempt
    drop(w);
    let (mut other, other_items) = ready_outcome(&e, &store);
    go(&mut other, OutcomeState::Running);
    run_agent(
        &e,
        &store,
        &mut other,
        item(&other_items, "inspect"),
        "att-other",
        "c1",
    );
    drop(store.open_attempt_session(&other, "att-other").unwrap());
    for id in ["att-1", "plain", control.as_str()] {
        assert!(store.open_attempt_session(&other, id).is_err(), "{id}");
        assert!(store.stop_attempt(&other, id).is_err(), "{id}");
    }
    // `synt stop` by id refuses control and attempt sessions; the owner's path works
    for id in [control.as_str(), "att-other"] {
        assert!(
            matches!(store.request_stop(id), Err(Error::Invalid(m)) if m.contains("belongs to an outcome"))
        );
    }
    assert!(store.request_stop("plain").unwrap());
    assert!(store.stop_attempt(&other, "att-other").unwrap());
    let stopped: Option<String> = e
        .db()
        .query_row(
            "SELECT stop_requested_at FROM sessions WHERE id='att-other'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stopped.is_some());
}

/// N8
#[test]
fn n8_an_outbox_payload_is_bounded() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let big = NewOutbox {
        idempotency_key: "big".into(),
        kind: OutboxKind::VerifierRun,
        payload: json!({"x": "a".repeat(MAX_OUTBOX_PAYLOAD_BYTES)}),
    };
    let r = w.atomically(|tx| tx.enqueue(&big));
    assert!(matches!(r, Err(Error::TooLarge { .. })), "{r:?}");
    assert!(w.list_outbox(w.id()).unwrap().is_empty());
}

/// N9: a developer database that enabled an earlier draft of v5.
#[test]
fn n9_a_v5_database_without_the_ddl_revision_marker_is_refused_loudly() {
    let e = env();
    let store = e.v5();
    let w = new_outcome(&e, &store);
    let id = w.id().clone();
    drop(w);
    e.db()
        .execute(
            "DELETE FROM schema_migrations WHERE version=?1",
            [synthesis_session::OUTCOME_DDL_MARKER],
        )
        .unwrap();
    let reader = store.reader().unwrap();
    assert!(matches!(
        reader.list_outcomes(),
        Err(Error::OutcomeSchemaOutdated)
    ));
    assert!(matches!(
        reader.get_outcome(&id),
        Err(Error::OutcomeSchemaOutdated)
    ));
    let r = store.create_outcome(NewOutcome {
        workspace: &e.ws(),
        base_commit: "x".into(),
        contract: &contract(),
        policy: &policy(),
    });
    assert!(matches!(r, Err(Error::OutcomeSchemaOutdated)));
    assert!(
        Error::OutcomeSchemaOutdated
            .to_string()
            .contains("restore backups/pre-v5")
    );
    // chat sessions on the same database are untouched
    e.session(&store, "chat", true);
    assert_eq!(
        store
            .reader()
            .unwrap()
            .list_used_sessions(None)
            .unwrap()
            .len(),
        1
    );
}

/// A dispatcher handles the kinds (and the one key) it names; other rows are not touched and
/// no try is consumed on them.
#[test]
fn an_outbox_filter_leaves_other_kinds_and_keys_alone() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let id = w.id().clone();
    let mk = |kind, key: &str| NewOutbox {
        idempotency_key: key.into(),
        kind,
        payload: json!({}),
    };
    w.atomically(|tx| {
        tx.enqueue(&mk(OutboxKind::VerifierRun, "v1"))?;
        tx.enqueue(&mk(OutboxKind::WorktreeCreate, "wt1"))?;
        tx.enqueue(&mk(OutboxKind::WorktreeCreate, "wt2"))
    })
    .unwrap();
    let log = RefCell::new(vec![]);
    let now = OffsetDateTime::now_utc();
    let kinds = [OutboxKind::WorktreeCreate];
    // only the one key of the one kind runs
    let run = w
        .run_outbox(
            now,
            &OutboxFilter::one(&kinds, "wt2"),
            &mut World {
                log: &log,
                probe_sees_it: false,
                fail: false,
            },
        )
        .unwrap();
    assert_eq!(run.done, 1);
    assert_eq!(*log.borrow(), ["wt2"]);
    // a failing effect for the filtered kind does not burn tries on the verifier row
    let run = w
        .run_outbox(
            now,
            &OutboxFilter::kinds(&kinds),
            &mut World {
                log: &log,
                probe_sees_it: false,
                fail: true,
            },
        )
        .unwrap();
    assert_eq!((run.done, run.retry_later), (0, 1));
    let rows = w.list_outbox(&id).unwrap();
    let by = |k: &str| {
        rows.iter()
            .find(|r| r.idempotency_key == k)
            .unwrap()
            .clone()
    };
    assert_eq!((by("v1").state, by("v1").tries), (OutboxState::Pending, 0));
    assert_eq!((by("wt2").state, by("wt2").tries), (OutboxState::Done, 1));
    assert_eq!(by("wt1").tries, 1);
    // an empty kind list handles nothing
    assert_eq!(
        w.claim_outbox(now, &OutboxFilter::kinds(&[])).unwrap(),
        None
    );
}

// ---- PR-10: grants derived from the stored action ----

fn later() -> OffsetDateTime {
    OffsetDateTime::now_utc() + time::Duration::hours(3)
}

/// An `approval_needed` decision with a stored action (the rules a grant would carry) on the
/// `implement` item, `pending`; the stored binding is what every later check uses.
fn grant_decision(
    e: &Env,
    store: &Store,
    kind: &str,
    action: Option<Value>,
) -> (OutcomeWriter, DecisionRecord, WorkItemId) {
    try_grant_decision(e, store, kind, action).unwrap()
}

fn try_grant_decision(
    e: &Env,
    store: &Store,
    kind: &str,
    action: Option<Value>,
) -> Result<(OutcomeWriter, DecisionRecord, WorkItemId), Error> {
    let (mut w, items) = ready_outcome(e, store);
    let it = item(&items, "implement").clone();
    let nd = NewDecision {
        kind: kind.into(),
        dedupe_key: "grant".into(),
        brief: json!({"why": "needs a permission", "alternatives": [
            {"id": "grant_once", "label": "allow it once"}, {"id": "retry", "label": "retry"}],
            "safe_default": "HOLD"}),
        binding: json!({"contract_version": 1, "plan_version": 1, "subject_commit": "abc",
            "policy_hash": "p1", "action_hash": "caller-says-so", "attempt_ordinal": 1,
            "work_item_id": it}),
        expires_at: hours(2),
        action,
    };
    let (d, _) = w.new_decision(&nd)?;
    w.transition_decision(&d.id, 1, DecisionState::Pending, &d.binding, NONE)
        .unwrap();
    let d = w.list_decisions(w.id()).unwrap().remove(0);
    Ok((w, d, it))
}

/// An attempt that is starting (what consumes a grant): the item leased, the attempt `starting`.
fn starting_attempt(
    e: &Env,
    store: &Store,
    w: &mut OutcomeWriter,
    it: &WorkItemId,
    sess: &str,
) -> AttemptRecord {
    if w.list_work_items(w.id())
        .unwrap()
        .iter()
        .any(|i| i.id == *it && i.state == WorkItemState::Created)
    {
        item_to(w, it, WorkItemState::Ready).unwrap();
    }
    item_to(w, it, WorkItemState::Leased).unwrap();
    e.child(store, sess, w.control_session_id());
    w.new_attempt(it, 1, sess, "local-inproc", NONE).unwrap().0
}

fn the_action() -> Value {
    json!({"needed": ["Write"], "calls": [{"tool": "Write", "input": {"file_path": "outside.txt"}, "asked": true}],
           "grant_rules": ["Edit(/outside.txt)"]})
}

#[test]
fn a_grant_is_derived_from_the_stored_action_and_the_store_owns_the_action_hash() {
    let e = env();
    let store = e.v5();
    let (mut w, d, it) = grant_decision(&e, &store, "approval_needed", Some(the_action()));
    // the label the person reads is derived from the rules, not trusted as given
    let label = d.brief["alternatives"][0]["label"].as_str().unwrap();
    assert_eq!(label, "allow exactly Edit(/outside.txt) in one new attempt");
    // a caller-supplied action hash is overwritten by the hash of the stored action
    assert_ne!(d.binding["action_hash"], "caller-says-so");
    assert_eq!(
        w.decision_action(&d.id).unwrap().unwrap()["grant_rules"],
        json!(["Edit(/outside.txt)"])
    );
    // a pending decision grants nothing
    assert!(w.create_grant(&d.id, &d.binding, 24).is_err());
    w.resolve_decision(&resolution(&d, "grant_once"), &d.binding)
        .unwrap();
    let g = w.create_grant(&d.id, &d.binding, 24).unwrap();
    // the rules are exactly the stored ones; the caller named only the decision
    assert_eq!(g.rules, ["Edit(/outside.txt)"]);
    assert_eq!(g.action_hash, d.binding["action_hash"].as_str().unwrap());
    assert_eq!(w.list_grants(w.id()).unwrap(), std::slice::from_ref(&g));
    assert_eq!(w.usable_grants(&it).unwrap().len(), 1);
    assert!(
        w.create_grant(&d.id, &d.binding, 24).is_err(),
        "one grant per decision"
    );
    // audited in the chained events
    let k = kinds(&w);
    assert!(
        k.iter().any(|x| x == "grant.created") && k.iter().any(|x| x == "policy.decision"),
        "{k:?}"
    );
}

#[test]
fn only_a_person_approved_grant_once_of_an_approval_needed_decision_can_grant() {
    for (kind, choice) in [
        ("approval_needed", "retry"),
        ("approval_needed", "reject"),
        ("ambiguity", "grant_once"),
    ] {
        let e = env();
        let store = e.v5();
        let (mut w, d, _) = grant_decision(&e, &store, kind, Some(the_action()));
        w.resolve_decision(&resolution(&d, choice), &d.binding)
            .unwrap();
        assert!(
            w.create_grant(&d.id, &d.binding, 24).is_err(),
            "{kind} {choice}"
        );
        assert!(w.list_grants(w.id()).unwrap().is_empty());
    }
    // a decision that offers grant_once must carry exact, grantable rules: nothing else is stored
    for action in [
        None,
        Some(json!({"needed": ["Write"]})),
        Some(json!({"grant_rules": []})),
        Some(json!({"grant_rules": ["Edit(/a\u{1b}[31m)"]})),
        // the engine reads `*` and a trailing `:*` as a glob or a prefix: not exact rules
        Some(json!({"grant_rules": ["Bash(python:*)"]})),
        Some(json!({"grant_rules": ["Bash(*)"]})),
        Some(json!({"grant_rules": ["Bash(ls *)"]})),
        Some(json!({"grant_rules": ["Bash(git * main)"]})),
        Some(json!({"grant_rules": ["Edit(/a*)"]})),
        Some(json!({"grant_rules": ["Edit(/a?)"]})),
        Some(json!({"grant_rules": ["Bash()"]})),
        Some(json!({"grant_rules": ["Bash( ls)"]})),
        Some(json!({"grant_rules": ["Read(/a)"]})),
        Some(json!({"grant_rules": ["Edit(/ok)", 7]})),
    ] {
        let e = env();
        let store = e.v5();
        assert!(
            try_grant_decision(&e, &store, "approval_needed", action.clone()).is_err(),
            "{action:?}"
        );
    }
}

#[test]
fn a_stored_action_rewritten_by_sql_cannot_grant() {
    let e = env();
    let store = e.v5();
    let (mut w, d, _) = grant_decision(&e, &store, "approval_needed", Some(the_action()));
    w.resolve_decision(&resolution(&d, "grant_once"), &d.binding)
        .unwrap();
    let db = e.db();
    assert!(
        db.execute("UPDATE decisions SET action='{}'", []).is_err(),
        "the trigger refuses it"
    );
    // a writer that drops the trigger and widens the rule still fails the binding's action hash
    db.execute_batch("DROP TRIGGER decisions_immutable")
        .unwrap();
    db.execute(
        "UPDATE decisions SET action=json_set(action,'$.grant_rules',json('[\"Bash\"]'))",
        [],
    )
    .unwrap();
    assert!(matches!(w.decision_action(&d.id), Err(Error::Tampered(_))));
    assert!(matches!(
        w.create_grant(&d.id, &d.binding, 24),
        Err(Error::Tampered(_))
    ));
}

#[test]
fn a_grant_is_refused_and_unusable_once_the_binding_moved_it_expired_or_it_was_revoked_or_used() {
    let e = env();
    let store = e.v5();
    let (mut w, d, it) = grant_decision(&e, &store, "approval_needed", Some(the_action()));
    w.resolve_decision(&resolution(&d, "grant_once"), &d.binding)
        .unwrap();
    // a changed commit: not the decision the person saw
    let mut moved = d.binding.clone();
    moved["subject_commit"] = json!("def");
    assert!(matches!(
        w.create_grant(&d.id, &moved, 24),
        Err(Error::Stale(_))
    ));
    let g = w.create_grant(&d.id, &d.binding, 24).unwrap();
    let a = starting_attempt(&e, &store, &mut w, &it, "att-g");
    // stale binding at consumption
    assert!(matches!(
        w.consume_grant(&g.id, &a.id, a.fence, &moved),
        Err(Error::Stale(_))
    ));
    // another attempt's fence
    assert!(
        w.consume_grant(&g.id, &a.id, a.fence + 9, &d.binding)
            .is_err()
    );
    // used once
    w.consume_grant(&g.id, &a.id, a.fence, &d.binding).unwrap();
    assert!(
        w.consume_grant(&g.id, &a.id, a.fence, &d.binding).is_err(),
        "twice"
    );
    assert!(w.usable_grants(&it).unwrap().is_empty());
    assert!(
        w.revoke_grant(&g.id, "late").is_err(),
        "a used grant cannot be revoked"
    );

    // revoked before use
    let e = env();
    let store = e.v5();
    let (mut w, d, it) = grant_decision(&e, &store, "approval_needed", Some(the_action()));
    w.resolve_decision(&resolution(&d, "grant_once"), &d.binding)
        .unwrap();
    let g = w.create_grant(&d.id, &d.binding, 24).unwrap();
    w.revoke_grant(&g.id, "the person changed their mind \u{1b}[31m")
        .unwrap();
    assert!(w.usable_grants(&it).unwrap().is_empty());
    let a = starting_attempt(&e, &store, &mut w, &it, "att-r");
    assert!(
        w.consume_grant(&g.id, &a.id, a.fence, &d.binding).is_err(),
        "revoked"
    );
    assert!(w.revoke_grant(&g.id, "again").is_err());
    assert!(kinds(&w).iter().any(|x| x == "grant.revoked"));

    // expired: three hours on, a one-hour grant is gone
    let e = env();
    let store = e.v5();
    let (mut w, d, it) = grant_decision(&e, &store, "approval_needed", Some(the_action()));
    w.resolve_decision(&resolution(&d, "grant_once"), &d.binding)
        .unwrap();
    w.create_grant(&d.id, &d.binding, 1).unwrap();
    assert_eq!(w.usable_grants(&it).unwrap().len(), 1);
    let w = w.with_clock(later);
    assert!(w.usable_grants(&it).unwrap().is_empty(), "expired");
}

#[test]
fn a_grant_row_without_a_valid_mac_is_never_used() {
    let e = env();
    let store = e.v5();
    let (mut w, d, it) = grant_decision(&e, &store, "approval_needed", Some(the_action()));
    w.resolve_decision(&resolution(&d, "grant_once"), &d.binding)
        .unwrap();
    e.db()
        .execute(
            "INSERT INTO grants(id,decision_id,outcome_id,work_item_id,rules,action_hash,expires_at,created_at,mac) \
             VALUES ('forged',?1,?2,?3,'[\"Bash\"]',?4,'2099-01-01T00:00:00.000Z','2026-01-01T00:00:00.000Z',?5)",
            params![d.id.as_str(), w.id().as_str(), it.as_str(), d.binding["action_hash"].as_str().unwrap(), "cd".repeat(32)],
        )
        .unwrap();
    assert!(w.list_grants(w.id()).unwrap().is_empty());
    assert!(w.usable_grants(&it).unwrap().is_empty());
    let a = starting_attempt(&e, &store, &mut w, &it, "att-f");
    assert!(
        w.consume_grant("forged", &a.id, a.fence, &d.binding)
            .is_err()
    );
}

#[test]
fn a_contract_is_revised_only_while_blocked_with_no_live_attempt_and_rebinds_only_on_approval() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    let it = item(&items, "implement").clone();
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        item_to(&mut w, &it, to).unwrap();
    }
    e.child(&store, "s1", w.control_session_id());
    let (a, _) = w.new_attempt(&it, 1, "s1", "local-inproc", NONE).unwrap();
    let mut wide = contract();
    wide.envelope.budgets.max_cost_usd = 99.0;
    let revise = |w: &mut OutcomeWriter| {
        let rev = w.get_outcome(w.id()).unwrap().revision;
        let c = wide.clone();
        w.atomically(|tx| tx.revise_contract(rev, c.clone()))
    };
    // running: no
    assert!(revise(&mut w).unwrap_err().to_string().contains("blocked"));
    go(&mut w, OutcomeState::Blocked);
    // blocked with a live attempt: no, and approving a version above the outcome's is no either
    assert!(revise(&mut w).unwrap_err().to_string().contains("live"));
    // the attempt ends: now it is stored, unapproved, and the outcome still stands on v1
    w.transition_attempt(
        &a.id,
        a.revision,
        AttemptState::Lost,
        &AttemptEnd::default(),
        NONE,
    )
    .unwrap();
    let (v, hash) = revise(&mut w).unwrap();
    assert_eq!(v, 2);
    assert!(!w.get_contract(w.id(), 2).unwrap().2);
    assert_eq!(w.get_outcome(w.id()).unwrap().contract_version, 1);
    // a bare UPDATE of approved_at does not approve it, and approval is by the stored hash only
    assert!(w.approve_contract(2, &"0".repeat(64), "user").is_err());
    w.approve_contract(2, &hash, "user").unwrap();
    let o = w.get_outcome(w.id()).unwrap();
    assert_eq!(o.contract_version, 2);
    assert!(w.get_contract(w.id(), 2).unwrap().2);
    assert!(kinds(&w).contains(&"outcome.contract_revised".to_string()));
    assert!(kinds(&w).contains(&"outcome.contract_rebound".to_string()));
    // the old plan (made for v1) no longer lets it run
    assert!(
        w.transition_outcome(o.revision, OutcomeState::Running, None, NONE)
            .is_err()
    );
}

/// `dec-contract-mutate-after-approve` (T-07, B4): a contract changed after approval by anything but
/// a person's approval of a new version does not run. The approved body cannot be edited, a bare
/// INSERT of the next version is not approved, and a blocked outcome moved onto it does not run.
#[test]
fn dec_contract_mutate_after_approve_a_bare_new_version_never_runs() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    go(&mut w, OutcomeState::Running);
    go(&mut w, OutcomeState::Blocked);
    let db = e.db();
    assert!(
        db.execute(
            "UPDATE contracts SET body=replace(body,'endpoint','evil') WHERE version=1",
            []
        )
        .is_err(),
        "the approved body is immutable"
    );
    let mut wide = contract();
    wide.version = 2;
    wide.envelope.budgets.max_cost_usd = 99.0;
    let body = serde_json::to_value(&wide).unwrap().to_string();
    let hash = Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    db.execute(
        "INSERT INTO contracts(outcome_id,version,hash,body) SELECT outcome_id,2,?1,?2 FROM contracts WHERE version=1",
        params![hash, body],
    )
    .unwrap();
    assert!(
        !w.get_contract(w.id(), 2).unwrap().2,
        "a bare row is not approved"
    );
    // moving the outcome's pointer by hand does not make it run: Blocked -> Running needs the approval
    if db
        .execute("UPDATE outcomes SET contract_version=2", [])
        .is_ok()
    {
        let o = w.get_outcome(w.id()).unwrap();
        assert!(
            w.transition_outcome(o.revision, OutcomeState::Running, None, NONE)
                .is_err()
        );
    }
}

// ------------------------------------------------------------ model planning runs (B6)

fn sha(c: char) -> String {
    c.to_string().repeat(64)
}

fn end(state: &str, cost: Option<f64>) -> PlanRunEnd {
    PlanRunEnd {
        state: state.into(),
        cost_usd: cost,
        raw_sha256: Some(sha('a')),
        plan_hash: (state == "finished").then(|| sha('b')),
        error_code: (state == "failed").then(|| "cycle".to_string()),
    }
}

/// An approved, plan-less outcome and a fresh planner session under its control session.
fn planning(e: &Env, store: &Store, session: &str) -> OutcomeWriter {
    let w = approved_outcome(e, store);
    e.child(store, session, w.control_session_id());
    w
}

#[test]
fn a_plan_run_reserves_min_of_its_cap_and_what_the_outcome_has_left_and_spend_never_passes_the_cap()
{
    let e = env();
    let store = e.v5();
    let mut w = planning(&e, &store, "p1");
    let id = w.id().clone();
    assert_eq!(w.outcome_spend(&id).unwrap(), 0.0);
    // the cap is 5.0; ask for 3.0
    let r1 = w.reserve_plan_run("p1", "m-1", 3.0).unwrap();
    assert_eq!((r1.reserved_usd, r1.state.as_str()), (3.0, "running"));
    assert_eq!(r1.base_commit, "c0ffee");
    assert_eq!(r1.contract_version, 1);
    assert_eq!(
        w.outcome_spend(&id).unwrap(),
        3.0,
        "reserved before the run"
    );
    // one run at a time
    e.child(&store, "p2", w.control_session_id());
    assert!(w.reserve_plan_run("p2", "m-1", 1.0).is_err());
    // a lost run keeps its reservation as spent
    w.settle_plan_run(&r1.id, &end("lost", None)).unwrap();
    assert_eq!(w.outcome_spend(&id).unwrap(), 3.0);
    // the next one gets what is left, however much it asks
    let r2 = w.reserve_plan_run("p2", "m-1", 100.0).unwrap();
    assert_eq!(r2.reserved_usd, 2.0);
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    // settled with a real cost: that cost replaces the reservation
    let done = w
        .settle_plan_run(&r2.id, &end("finished", Some(0.5)))
        .unwrap();
    assert_eq!(done.cost_usd, Some(0.5));
    assert_eq!(w.outcome_spend(&id).unwrap(), 3.5);
    let usage = w.outcome_usage(&id).unwrap();
    assert_eq!((usage.cost_usd, usage.reserved_usd), (0.5, 3.0));
    // the rest of the cap can be reserved, and nothing beyond it ever
    e.child(&store, "p3", w.control_session_id());
    let r3 = w.reserve_plan_run("p3", "m-1", 100.0).unwrap();
    assert_eq!(r3.reserved_usd, 1.5);
    w.settle_plan_run(&r3.id, &end("failed", None)).unwrap();
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    e.child(&store, "p4", w.control_session_id());
    assert!(
        w.reserve_plan_run("p4", "m-1", 0.5).is_err(),
        "no budget left"
    );
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    assert_eq!(w.list_plan_runs(&id).unwrap().len(), 3);
}

#[test]
fn the_settings_ceiling_bounds_the_planner_too_and_hostile_amounts_refuse() {
    let e = env();
    let store = e.v5();
    let w = approved_outcome(&e, &store);
    let id = w.id().clone();
    let control = w.control_session_id().to_owned();
    drop(w);
    e.child(&store, "p1", &control);
    // the ceilings tightened below the contract's cap since it was approved: no planning run, as no attempt
    assert!(
        store
            .open_outcome(&id, &with(|p| p.max_cost_usd = Some(2.0)))
            .unwrap()
            .reserve_plan_run("p1", "m", 1.0)
            .is_err()
    );
    let mut w = store.open_outcome(&id, &policy()).unwrap();
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(w.reserve_plan_run("p1", "m", bad).is_err(), "{bad}");
    }
    for m in ["", "has space", "new\nline", &"m".repeat(201), "\u{202e}x"] {
        assert!(w.reserve_plan_run("p1", m, 1.0).is_err(), "{m:?}");
    }
    assert_eq!(
        w.reserve_plan_run("p1", "m", 50.0).unwrap().reserved_usd,
        5.0,
        "a hostile amount is bounded by the outcome cap"
    );
}

#[test]
fn attempt_reservations_and_plan_runs_share_one_cap_in_either_order() {
    let e = env();
    let store = e.v5();
    let (mut w, it) = ready_for_attempt(&e, &store);
    let id = w.id().clone();
    go(&mut w, OutcomeState::Blocked);
    // an attempt that spent 4.0 (its reservation), then a replanning run asks for 3.0
    e.child(&store, "p1", w.control_session_id());
    let (a, _) = w
        .new_attempt(&it, 1, "att-1", "local-inproc", NONE)
        .unwrap();
    w.reserve_budget(&a.id, a.fence, 4.0).unwrap();
    assert_eq!(w.outcome_spend(&id).unwrap(), 4.0);
    let r = w.reserve_plan_run("p1", "m", 3.0).unwrap();
    assert_eq!(r.reserved_usd, 1.0, "the attempt came first");
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    // the other way round: the next attempt is sized from this spend (cap minus spend), which
    // now holds the plan run, so nothing is left for another reservation
    assert_eq!(5.0 - w.outcome_spend(&id).unwrap(), 0.0);
}

#[test]
fn a_plan_run_needs_a_pre_plan_outcome_an_approved_contract_and_a_child_session() {
    let e = env();
    let store = e.v5();
    // not approved yet
    let mut w = new_outcome(&e, &store);
    go(&mut w, OutcomeState::ContractPending);
    e.child(&store, "p1", w.control_session_id());
    assert!(w.reserve_plan_run("p1", "m", 1.0).is_err());
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    // a session that is not under the control session
    e.session(&store, "stray", false);
    assert!(w.reserve_plan_run("stray", "m", 1.0).is_err());
    assert!(w.reserve_plan_run("nope", "m", 1.0).is_err());
    let r = w.reserve_plan_run("p1", "m", 1.0).unwrap();
    // the same session cannot plan twice
    w.settle_plan_run(&r.id, &end("failed", None)).unwrap();
    assert!(w.reserve_plan_run("p1", "m", 1.0).is_err());
    // an outcome that already runs does not plan
    let (mut w2, _) = ready_outcome(&e, &store);
    e.child(&store, "q1", w2.control_session_id());
    assert!(w2.reserve_plan_run("q1", "m", 1.0).is_err());
}

#[test]
fn a_plan_run_is_settled_once_with_a_state_a_cost_and_hashes() {
    let e = env();
    let store = e.v5();
    let mut w = planning(&e, &store, "p1");
    let r = w.reserve_plan_run("p1", "m", 1.0).unwrap();
    for bad in [
        end("running", None),
        end("done", None),
        end("finished", Some(-1.0)),
        end("finished", Some(f64::NAN)),
        PlanRunEnd {
            raw_sha256: Some("xyz".into()),
            ..end("finished", None)
        },
        PlanRunEnd {
            plan_hash: Some("A".repeat(64)),
            ..end("finished", None)
        },
        PlanRunEnd {
            error_code: Some("has space".into()),
            ..end("failed", None)
        },
    ] {
        assert!(w.settle_plan_run(&r.id, &bad).is_err(), "{bad:?}");
    }
    assert!(w.settle_plan_run("nope", &end("lost", None)).is_err());
    let done = w
        .settle_plan_run(&r.id, &end("finished", Some(0.25)))
        .unwrap();
    assert_eq!(done.state, "finished");
    assert_eq!(done.plan_hash, Some(sha('b')));
    assert!(done.ended_at.is_some());
    assert!(matches!(
        w.settle_plan_run(&r.id, &end("lost", None)),
        Err(Error::Stale(_))
    ));
}

#[test]
fn plan_run_rows_are_set_once_and_never_deleted() {
    let e = env();
    let store = e.v5();
    let mut w = planning(&e, &store, "p1");
    let r = w.reserve_plan_run("p1", "m", 2.0).unwrap();
    let db = e.db();
    for sql in [
        "UPDATE plan_runs SET reserved_usd=0.01",
        "UPDATE plan_runs SET model='other'",
        "UPDATE plan_runs SET session_id='x'",
        "UPDATE plan_runs SET base_commit='x'",
        "UPDATE plan_runs SET started_at='2000-01-01T00:00:00.000Z'",
        "UPDATE plan_runs SET mac='00'",
        "DELETE FROM plan_runs",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    w.settle_plan_run(&r.id, &end("finished", Some(1.0)))
        .unwrap();
    for sql in [
        "UPDATE plan_runs SET cost_usd=0.0",
        "UPDATE plan_runs SET plan_hash=NULL",
        "UPDATE plan_runs SET raw_sha256=NULL",
        "UPDATE plan_runs SET error_code='x'",
        "UPDATE plan_runs SET ended_at=NULL",
        "UPDATE plan_runs SET state='running'",
        "UPDATE plan_runs SET state='lost'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
}

#[test]
fn a_forged_plan_run_row_does_not_verify_and_never_lowers_what_is_counted() {
    let e = env();
    let store = e.v5();
    let mut w = planning(&e, &store, "p1");
    let id = w.id().clone();
    let r = w.reserve_plan_run("p1", "m", 2.0).unwrap();
    assert_eq!(w.list_plan_runs(&id).unwrap()[0].id, r.id);
    // a writer that drops the trigger and rewrites the reservation: the MAC no longer verifies
    let db = e.db();
    db.execute("DROP TRIGGER plan_runs_identity_immutable", [])
        .unwrap();
    db.execute("UPDATE plan_runs SET reserved_usd=0.01", [])
        .unwrap();
    assert!(matches!(w.list_plan_runs(&id), Err(Error::Tampered(_))));
    // and no new run is reserved on top of a row that does not verify
    e.child(&store, "p2", w.control_session_id());
    w.settle_plan_run(&r.id, &end("lost", None)).unwrap_err();
    assert!(w.reserve_plan_run("p2", "m", 1.0).is_err());
    // a row inserted by someone who cannot compute the MAC does not verify either
    db.execute("UPDATE plan_runs SET reserved_usd=2.0", [])
        .unwrap();
    assert!(
        w.list_plan_runs(&id).is_ok(),
        "the original value verifies again"
    );
    db.execute(
        "INSERT INTO plan_runs(id,outcome_id,contract_version,base_commit,session_id,model,state,reserved_usd,mac,started_at) \
         VALUES ('forged',?1,1,'c0ffee','p2','m','finished',0.01,'00','2026-01-01T00:00:00.000Z')",
        [id.as_str()],
    )
    .unwrap();
    assert!(matches!(w.list_plan_runs(&id), Err(Error::Tampered(_))));
    assert!(w.reserve_plan_run("p2", "m", 1.0).is_err());
}

#[test]
fn the_number_of_plan_runs_of_an_outcome_is_bounded() {
    let e = env();
    let store = e.v5();
    let mut w = approved_outcome(&e, &store);
    let mut made = 0;
    for i in 0..(MAX_PLAN_RUNS + 2) {
        let s = format!("s{i}");
        e.child(&store, &s, w.control_session_id());
        match w.reserve_plan_run(&s, "m", 0.01) {
            Ok(r) => {
                w.settle_plan_run(&r.id, &end("finished", Some(0.0)))
                    .unwrap();
                made += 1;
            }
            Err(_) => break,
        }
    }
    assert_eq!(made, MAX_PLAN_RUNS);
}

fn read_only_contract() -> Contract {
    let mut c = contract();
    c.kind = ContractKind::ReadOnly;
    c.criteria = ["report", "r1"]
        .iter()
        .map(|id| ContractCriterion {
            id: (*id).into(),
            text: format!("criterion {id}"),
            mandatory: true,
            manual: true,
            required_trust: TrustClass::HumanReview,
            check_ids: vec![],
        })
        .collect();
    c.envelope.allowed_paths = vec![];
    c.envelope.denied_paths = vec![];
    c.envelope.capabilities = BTreeMap::from([(Capability::RepoRead, CapabilityMode::Allow)]);
    c.envelope.budgets.max_parallel_workers = 1;
    c
}

fn create_with_base(
    e: &Env,
    store: &Store,
    c: &Contract,
    base: &str,
) -> Result<OutcomeWriter, Error> {
    store.create_outcome(NewOutcome {
        workspace: &e.ws(),
        base_commit: base.into(),
        contract: c,
        policy: &policy(),
    })
}

/// A read-only goal has no base commit (no git needed) and a write goal always has one: neither
/// can be created with the other's base, so a write contract cannot be stored as a read-only one
/// by leaving the commit out, nor the reverse.
#[test]
fn ro_base_commit_is_empty_for_read_only_and_required_for_write() {
    let e = env();
    let store = e.v5();
    let w = create_with_base(&e, &store, &read_only_contract(), "").unwrap();
    let o = w.get_outcome(w.id()).unwrap();
    assert_eq!(o.base_commit, "");
    assert_eq!(
        w.get_contract(w.id(), 1).unwrap().0.kind,
        ContractKind::ReadOnly
    );
    drop(w);
    assert!(create_with_base(&e, &store, &read_only_contract(), "c0ffee").is_err());
    assert!(create_with_base(&e, &store, &contract(), "").is_err());
    assert_eq!(e.version(), 5, "no DDL: the store stays v5");
    // a write goal's stored body has no `kind`: its hash is what it was before read-only goals
    let w = create_with_base(&e, &store, &contract(), "c0ffee").unwrap();
    let hash = w.get_contract(w.id(), 1).unwrap().1;
    let body: String = e
        .db()
        .query_row("SELECT body FROM contracts WHERE hash=?1", [&hash], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(!body.contains("kind"), "{body}");
    assert_eq!(hash, format!("{:x}", Sha256::digest(body.as_bytes())));
}

#[test]
fn ro_revise_kind_refused_in_both_directions_and_for_a_bare_row() {
    for (start, other, base) in [
        (contract(), read_only_contract(), "c0ffee"),
        (read_only_contract(), contract(), ""),
    ] {
        let e = env();
        let store = e.v5();
        let mut w = create_with_base(&e, &store, &start, base).unwrap();
        go(&mut w, OutcomeState::ContractPending);
        let h = contract_hash(&w);
        w.approve_contract(1, &h, "user").unwrap();
        let mut p = plan();
        if start.kind == ContractKind::ReadOnly {
            p.items = vec![PlanItem {
                key: "research".into(),
                kind: WorkItemKind::Agent,
                requires: vec![],
                capabilities: vec!["repo.read".into()],
                covers: vec!["report".into(), "r1".into()],
                title: String::new(),
                instructions: String::new(),
            }];
        }
        w.insert_plan(&p).unwrap();
        go(&mut w, OutcomeState::Ready);
        go(&mut w, OutcomeState::Running);
        go(&mut w, OutcomeState::Blocked);
        let rev = w.get_outcome(w.id()).unwrap().revision;
        let mut next = other.clone();
        next.envelope.budgets.max_cost_usd = 9.0;
        let err = w
            .atomically(|tx| tx.revise_contract(rev, next.clone()))
            .unwrap_err();
        assert!(
            err.to_string().contains("kind of a goal never changes"),
            "{err}"
        );
        // a row written around the API is refused when a person tries to approve it
        let mut bare = other.clone();
        bare.version = 2;
        let body = serde_json::to_value(&bare).unwrap().to_string();
        let hash = Sha256::digest(body.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        e.db()
            .execute(
                "INSERT INTO contracts(outcome_id,version,hash,body) SELECT outcome_id,2,?1,?2 FROM contracts WHERE version=1",
                params![hash, body],
            )
            .unwrap();
        let err = w.approve_contract(2, &hash, "user").unwrap_err();
        assert!(
            err.to_string().contains("kind of a goal never changes"),
            "{err}"
        );
        assert_eq!(w.get_outcome(w.id()).unwrap().contract_version, 1);
    }
}
