//! Outcome persistence (ADR-016): opt-in v5 migration, revisions, commands, outbox. Real SQLite in
//! a temp dir; canonical temp paths (macOS `/var` -> `/private/var`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synthesis_core::outcome::*;
use synthesis_core::{PermissionMode, TaskStatus, WorkspaceContext};
use synthesis_events::Budget;
use synthesis_session::outcome::*;
use synthesis_session::{
    CleanScope, Error, NewSession, SCHEMA_VERSION_LATEST, SCHEMA_VERSION_OUTCOMES, Store,
    StoreOptions,
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
    /// A plain chat session (the writer is dropped, so its lock is free).
    fn session(&self, store: &Store, id: &str, with_message: bool) {
        let mut w = store
            .create_session(NewSession {
                id: Some(id.into()),
                workspace: &self.ws(),
                model_config_id: "m".into(),
                permission_mode: PermissionMode::Default,
                permission_policy_id: "default".into(),
                budget: Budget::default(),
                parent: None,
                title: None,
            })
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
            item("test", WorkItemKind::Verify, &["implement"], &["c2"]),
        ],
    }
}

fn new_outcome(e: &Env, store: &Store) -> OutcomeWriter {
    store
        .create_outcome(NewOutcome {
            workspace: &e.ws(),
            base_commit: "c0ffee".into(),
            contract: &contract(),
        })
        .unwrap()
}

fn contract_hash(w: &OutcomeWriter) -> String {
    w.get_contract(w.id(), 1).unwrap().1
}

/// Outcome at `ready` with a plan: contract approved, plan stored, state walked there.
fn ready_outcome(e: &Env, store: &Store) -> (OutcomeWriter, Vec<(String, WorkItemId)>) {
    let mut w = new_outcome(e, store);
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    let items = w.insert_plan(&plan()).unwrap();
    let none = Effects::default();
    let f = OutcomeFacts::default();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    let rev = w
        .transition_outcome(rev, OutcomeState::ContractPending, f, None, &none)
        .unwrap();
    w.transition_outcome(rev, OutcomeState::Ready, f, None, &none)
        .unwrap();
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
            contract: &contract()
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
    // still reachable by id
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
    let stored: i64 = e
        .db()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE id=?1",
            [&control],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 1);
}

#[test]
fn an_attempt_session_survives_the_cleaners() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    e.session(&store, "att-1", true);
    e.session(&store, "other", true);
    w.new_attempt(
        item(&items, "implement"),
        1,
        "att-1",
        "local-inproc",
        &Effects::default(),
    )
    .unwrap();
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
    let rev = w.list_work_items(w.id()).unwrap()[0].revision;
    w.transition_work_item(
        &items[0].1,
        rev,
        WorkItemState::Ready,
        2,
        false,
        &Effects::default(),
    )
    .unwrap();
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
    let rep = store.reader().unwrap().verify_chain(&control).unwrap();
    assert!(rep.broken.is_some(), "{rep:?}");
}

// -------------------------------------------------------------- revisions, CAS

#[test]
fn transitions_are_compare_and_swap_and_follow_the_tables() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    let id = w.id().clone();
    let none = Effects::default();
    let o = w.get_outcome(&id).unwrap();
    assert_eq!((o.state, o.plan_version), (OutcomeState::Ready, Some(1)));
    // a stale revision loses and changes nothing
    let e1 = w
        .transition_outcome(
            o.revision - 1,
            OutcomeState::Running,
            OutcomeFacts::default(),
            None,
            &none,
        )
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
    // a pair outside the table is refused
    assert!(
        w.transition_outcome(
            o.revision,
            OutcomeState::Verified,
            OutcomeFacts::default(),
            None,
            &none
        )
        .is_err()
    );
    let r = w
        .transition_outcome(
            o.revision,
            OutcomeState::Running,
            OutcomeFacts::default(),
            None,
            &none,
        )
        .unwrap();
    assert_eq!(r, o.revision + 1);
    // Verified is reachable only with the verdict fact
    let r = w
        .transition_outcome(
            r,
            OutcomeState::Verifying,
            OutcomeFacts::default(),
            None,
            &none,
        )
        .unwrap();
    assert!(
        w.transition_outcome(
            r,
            OutcomeState::Verified,
            OutcomeFacts::default(),
            None,
            &none
        )
        .is_err()
    );
    let facts = OutcomeFacts {
        verdict_verified: true,
        delivery_ready: false,
    };
    assert_eq!(
        w.transition_outcome(r, OutcomeState::Verified, facts, Some("verdict"), &none)
            .unwrap(),
        r + 1
    );
    // a work item of another outcome is not reachable from this writer
    drop(w);
    let (_other, other_items) = ready_outcome(&e, &store);
    let mut w = store.open_outcome(&id).unwrap();
    let err = w
        .transition_work_item(&other_items[0].1, 1, WorkItemState::Ready, 2, false, &none)
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
    let _ = items;
}

#[test]
fn ready_needs_an_approved_contract_and_a_stored_plan() {
    let e = env();
    let store = e.v5();
    let mut w = new_outcome(&e, &store);
    let none = Effects::default();
    let f = OutcomeFacts::default();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    let rev = w
        .transition_outcome(rev, OutcomeState::ContractPending, f, None, &none)
        .unwrap();
    let err = w
        .transition_outcome(rev, OutcomeState::Ready, f, None, &none)
        .unwrap_err();
    assert!(err.to_string().contains("not approved"), "{err}");
    // approving a different hash than the stored one is refused
    assert!(w.approve_contract(1, &"0".repeat(64), "user").is_err());
    let h = contract_hash(&w);
    w.approve_contract(1, &h, "user").unwrap();
    w.approve_contract(1, &h, "user").unwrap(); // same approval again: no second approval
    let approvals = kinds(&w)
        .iter()
        .filter(|k| *k == "outcome.contract_approved")
        .count();
    assert_eq!(approvals, 1);
    // approved but no plan yet
    let rev = w.get_outcome(w.id()).unwrap().revision;
    assert!(
        w.transition_outcome(rev, OutcomeState::Ready, f, None, &none)
            .is_err()
    );
    w.insert_plan(&plan()).unwrap();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(rev, OutcomeState::Ready, f, None, &none)
        .unwrap();
}

#[test]
fn contracts_and_plans_are_immutable_and_approvals_append_only() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    let db = e.db();
    for sql in [
        "UPDATE contracts SET body='{}'",
        "UPDATE contracts SET hash='x'",
        "UPDATE contracts SET approved_by='mallory'",
        "UPDATE plans SET dag='{}'",
    ] {
        let err = db.execute(sql, []).unwrap_err().to_string();
        assert!(
            err.contains("immutable") || err.contains("approved once"),
            "{sql}: {err}"
        );
    }
    // an invalid plan is refused and stores nothing
    let mut bad = plan();
    bad.version = 2;
    bad.items[2].requires.push("ghost".into());
    assert!(w.insert_plan(&bad).is_err());
    assert_eq!(w.list_work_items(w.id()).unwrap().len(), 3);
    // approvals
    let (d, _) = w.new_decision(&decision("k1")).unwrap();
    w.record_decision_approval(&d.id, 1, "user", "A", &"a".repeat(64))
        .unwrap();
    for sql in [
        "UPDATE decision_approvals SET choice='B'",
        "DELETE FROM decision_approvals",
    ] {
        assert!(
            db.execute(sql, [])
                .unwrap_err()
                .to_string()
                .contains("append-only")
        );
    }
    let _ = items;
}

#[test]
fn a_failed_transaction_leaves_no_row_and_no_event() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let before = (kinds(&w).len(), w.get_outcome(w.id()).unwrap());
    let rev = before.1.revision;
    let r: Result<(), Error> = w.atomically(|tx| {
        tx.transition_outcome(
            rev,
            OutcomeState::Running,
            OutcomeFacts::default(),
            None,
            &Effects::default(),
        )?;
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

#[test]
fn the_work_item_retry_budget_is_read_from_the_row() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    let it = item(&items, "inspect").clone();
    let none = Effects::default();
    let step =
        |w: &mut OutcomeWriter, to: WorkItemState, max: u32, ev: bool| -> Result<(), Error> {
            let rev = w
                .list_work_items(w.id())
                .unwrap()
                .into_iter()
                .find(|i| i.id == it)
                .unwrap()
                .revision;
            w.transition_work_item(&it, rev, to, max, ev, &none)
                .map(|_| ())
        };
    use WorkItemState::*;
    for to in [Ready, Leased, Running] {
        step(&mut w, to, 1, false).unwrap();
    }
    // budget 1: one retry is allowed, the second failure is terminal
    step(&mut w, RetryableFailure, 1, false).unwrap();
    step(&mut w, Ready, 1, false).unwrap();
    let used = |w: &OutcomeWriter| {
        w.list_work_items(w.id())
            .unwrap()
            .into_iter()
            .find(|i| i.id == it)
            .unwrap()
            .retries_used
    };
    assert_eq!(used(&w), 1);
    step(&mut w, Leased, 1, false).unwrap();
    step(&mut w, Running, 1, false).unwrap();
    let err = step(&mut w, RetryableFailure, 1, false).unwrap_err();
    assert!(err.to_string().contains("retry budget"), "{err}");
    step(&mut w, TerminalFailure, 1, false).unwrap();
    assert_eq!(used(&w), 1);
}

// --------------------------------------------------------------------- attempts

#[test]
fn attempts_are_unique_per_ordinal_and_evidence_is_fenced_and_attributed() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    let it = item(&items, "implement").clone();
    e.session(&store, "att-1", false);
    e.session(&store, "att-2", false);
    let none = Effects::default();
    let a1 = w
        .new_attempt(&it, 1, "att-1", "local-inproc", &none)
        .unwrap();
    assert_eq!(
        (a1.state, a1.fence, a1.revision),
        (AttemptState::Starting, 1, 1)
    );
    // a replayed allocation is refused, not doubled
    let dup = w
        .new_attempt(&it, 1, "att-2", "local-inproc", &none)
        .unwrap_err();
    assert!(matches!(dup, Error::AlreadyExists(_)), "{dup}");
    assert_eq!(w.list_attempts(&it).unwrap().len(), 1);
    assert!(
        w.new_attempt(&it, 2, "no-such-session", "local-inproc", &none)
            .is_err()
    );
    assert!(w.new_attempt(&it, 2, "att-2", "remote", &none).is_err());

    let ev = |attempt: &AttemptRecord, class, producer: &str, fence, result: &str| NewEvidence {
        work_item_id: it.clone(),
        attempt_id: Some(attempt.id.clone()),
        trust_class: class,
        producer: producer.into(),
        subject_commit: "abc".into(),
        check_id: "unit".into(),
        command_hash: None,
        criteria_ids: vec!["c1".into()],
        result: result.into(),
        exit_code: Some(0),
        env_fingerprint: "fp".into(),
        artifact: None,
        fence,
    };
    // only the independent runner writes independent evidence; a worker claim cannot pose as it
    for (class, producer) in [
        (TrustClass::IndependentCheck, "worker"),
        (TrustClass::IndependentCheck, "gate"),
        (TrustClass::HumanReview, "independent_runner"),
        (TrustClass::ClaimedByWorker, "independent_runner"),
    ] {
        let err = w
            .record_evidence(&ev(&a1, class, producer, 1, "passed"))
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot write"),
            "{class:?} {producer}: {err}"
        );
    }
    assert!(
        w.record_evidence(&ev(
            &a1,
            TrustClass::IndependentCheck,
            "independent_runner",
            1,
            "ok?"
        ))
        .is_err()
    );
    let good = ev(
        &a1,
        TrustClass::IndependentCheck,
        "independent_runner",
        1,
        "passed",
    );
    let (id, fresh) = w.record_evidence(&good).unwrap();
    assert!(fresh);
    // idempotent on (attempt, check, commit)
    assert_eq!(w.record_evidence(&good).unwrap(), (id, false));
    assert_eq!(w.list_evidence(w.id()).unwrap().len(), 1);
    // a wrong fence for the attempt is refused
    let mut other_check = good.clone();
    other_check.check_id = "lint".into();
    other_check.fence = 7;
    assert!(w.record_evidence(&other_check).is_err());

    // attempt 2 supersedes: attempt 1's late evidence is stale
    let a2 = w
        .new_attempt(&it, 2, "att-2", "local-inproc", &none)
        .unwrap();
    assert_eq!(a2.fence, 2);
    let mut late = good.clone();
    late.check_id = "late".into();
    let err = w.record_evidence(&late).unwrap_err();
    assert!(err.to_string().contains("stale"), "{err}");
    let mut now = ev(
        &a2,
        TrustClass::IndependentCheck,
        "independent_runner",
        2,
        "failed",
    );
    now.check_id = "late".into();
    w.record_evidence(&now).unwrap();

    // finish attempt 1: end details are stored
    let rev = a1.revision;
    let rev = w
        .transition_attempt(
            &a1.id,
            rev,
            AttemptState::Running,
            &AttemptEnd::default(),
            &none,
        )
        .unwrap();
    w.transition_attempt(
        &a1.id,
        rev,
        AttemptState::Finished,
        &AttemptEnd {
            task_status: Some(TaskStatus::Completed),
            subject_commit: Some("abc".into()),
            retained_reason: None,
        },
        &none,
    )
    .unwrap();
    let got = &w.list_attempts(&it).unwrap()[0];
    assert_eq!(got.state, AttemptState::Finished);
    assert_eq!(got.task_status.as_deref(), Some("completed"));
    assert_eq!(got.subject_commit.as_deref(), Some("abc"));
    assert!(got.ended_at.is_some());
}

// -------------------------------------------------------------------- decisions

fn decision(key: &str) -> NewDecision {
    NewDecision {
        kind: "ambiguity".into(),
        dedupe_key: key.into(),
        brief: json!({"why": "which API shape", "alternatives": ["A", "B"], "safe_default": "HOLD"}),
        binding: json!({"contract_version": 1, "plan_version": 1, "subject_commit": "abc"}),
        expires_at: "2099-01-01T00:00:00.000Z".into(),
    }
}

fn guards(actor: Actor) -> DecisionGuards {
    DecisionGuards {
        actor,
        binding_current: true,
        expiry_reached: false,
    }
}

#[test]
fn decisions_dedupe_and_only_a_person_resolves() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let (d, fresh) = w.new_decision(&decision("k")).unwrap();
    assert!(fresh && d.state == DecisionState::Preparing);
    let (again, fresh) = w.new_decision(&decision("k")).unwrap();
    assert!(!fresh && again.id == d.id);
    assert_eq!(w.list_decisions(w.id()).unwrap().len(), 1);
    assert_eq!(
        kinds(&w)
            .iter()
            .filter(|k| *k == "decision.created")
            .count(),
        1
    );
    assert!(
        w.new_decision(&NewDecision {
            brief: json!("text"),
            ..decision("x")
        })
        .is_err()
    );

    let none = Effects::default();
    let rev = w
        .transition_decision(
            &d.id,
            1,
            DecisionState::Pending,
            &guards(Actor::Orchestrator),
            &none,
        )
        .unwrap();
    for who in [Actor::Orchestrator, Actor::Attempt] {
        assert!(
            w.transition_decision(&d.id, rev, DecisionState::Approved, &guards(who), &none)
                .is_err()
        );
    }
    let stale = DecisionGuards {
        binding_current: false,
        ..guards(Actor::User)
    };
    assert!(
        w.transition_decision(&d.id, rev, DecisionState::Approved, &stale, &none)
            .is_err()
    );
    w.transition_decision(
        &d.id,
        rev,
        DecisionState::Approved,
        &guards(Actor::User),
        &none,
    )
    .unwrap();
    let after = w.list_decisions(w.id()).unwrap();
    assert_eq!(after[0].state, DecisionState::Approved);
}

// --------------------------------------------------------------------- commands

fn resolve_cmd(
    w: &OutcomeWriter,
    key: &str,
    d: &DecisionId,
    rev: i64,
    surface: Surface,
) -> NewCommand {
    NewCommand {
        outcome_id: w.id().clone(),
        idempotency_key: key.into(),
        kind: "decision.resolve".into(),
        params: json!({"decision_id": d, "choice": "A"}),
        expected_revision: Some(rev),
        from_surface: surface,
    }
}

/// The handler an orchestrator would run for `decision.resolve`.
fn resolve(
    tx: &mut OutcomeTx<'_, '_>,
    c: &CommandRecord,
    d: &DecisionId,
) -> Result<(CommandState, Value), Error> {
    let rev = c.expected_revision.unwrap();
    tx.transition_decision(
        d,
        rev,
        DecisionState::Approved,
        &guards(Actor::User),
        &Effects::default(),
    )?;
    tx.record_decision_approval(d, rev, "user", "A", &"b".repeat(64))?;
    Ok((CommandState::Applied, json!({"revision": rev + 1})))
}

#[test]
fn two_surfaces_resolving_one_decision_get_one_effect_and_one_loser() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let (d, _) = w.new_decision(&decision("k")).unwrap();
    let rev = w
        .transition_decision(
            &d.id,
            1,
            DecisionState::Pending,
            &guards(Actor::Orchestrator),
            &Effects::default(),
        )
        .unwrap();
    // both surfaces queue while the owner is busy; the owner applies them in order
    let c1 = store
        .enqueue_command(resolve_cmd(&w, "cli-1", &d.id, rev, Surface::Cli))
        .unwrap();
    let c2 = store
        .enqueue_command(resolve_cmd(&w, "tui-1", &d.id, rev, Surface::Tui))
        .unwrap();
    assert_eq!(
        (c1.state, c2.state),
        (CommandState::Queued, CommandState::Queued)
    );
    assert_eq!(w.queued_commands(w.id()).unwrap().len(), 2);

    let did = d.id.clone();
    let a = w
        .apply_command("cli-1", |tx, c| resolve(tx, c, &did))
        .unwrap();
    let b = w
        .apply_command("tui-1", |tx, c| resolve(tx, c, &did))
        .unwrap();
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
    assert_eq!(
        w.list_decisions(w.id()).unwrap()[0].state,
        DecisionState::Approved
    );
    assert!(w.queued_commands(w.id()).unwrap().is_empty());
    let finished = kinds(&w)
        .iter()
        .filter(|k| *k == "outcome.command_finished")
        .count();
    assert_eq!(finished, 2);
}

#[test]
fn a_retried_command_returns_the_stored_result_with_no_second_effect() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let (d, _) = w.new_decision(&decision("k")).unwrap();
    let rev = w
        .transition_decision(
            &d.id,
            1,
            DecisionState::Pending,
            &guards(Actor::Orchestrator),
            &Effects::default(),
        )
        .unwrap();
    let cmd = resolve_cmd(&w, "retry-key", &d.id, rev, Surface::Cli);
    let q = store.enqueue_command(cmd.clone()).unwrap();
    // the same request again: the same row, not a second one
    assert_eq!(store.enqueue_command(cmd.clone()).unwrap().id, q.id);
    let did = d.id.clone();
    let calls = RefCell::new(0);
    let first = w
        .apply_command("retry-key", |tx, c| {
            *calls.borrow_mut() += 1;
            resolve(tx, c, &did)
        })
        .unwrap();
    // asked again after it ran (client retry, or a restarted owner replaying): stored result
    let again = w
        .apply_command("retry-key", |tx, c| {
            *calls.borrow_mut() += 1;
            resolve(tx, c, &did)
        })
        .unwrap();
    assert_eq!(*calls.borrow(), 1);
    assert_eq!(first, again);
    // the surface reading the result later gets the same
    assert_eq!(
        store
            .reader()
            .unwrap()
            .get_command("retry-key")
            .unwrap()
            .unwrap(),
        first
    );
    // the same key for a different request is refused, not silently merged
    let mut other = cmd.clone();
    other.params = json!({"decision_id": d.id, "choice": "B"});
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

#[test]
fn commands_wait_for_an_owner_and_ownership_is_one_lock() {
    let e = env();
    let store = e.v5();
    let (w, _) = ready_outcome(&e, &store);
    let id = w.id().clone();
    // a second owner is refused while the first lives
    assert!(matches!(store.open_outcome(&id), Err(Error::Locked(_))));
    drop(w);
    // no owner runs: a surface can still queue
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
    // an owner starts later and applies it
    let mut w = store.open_outcome(&id).unwrap();
    let done = w
        .apply_command("start-1", |tx, _| {
            let rev: i64 = tx.outcome_revision()?;
            tx.transition_outcome(
                rev,
                OutcomeState::Running,
                OutcomeFacts::default(),
                None,
                &Effects::default(),
            )?;
            Ok((CommandState::Applied, json!({"state": "running"})))
        })
        .unwrap();
    assert_eq!(done.state, CommandState::Applied);
    assert_eq!(w.get_outcome(&id).unwrap().state, OutcomeState::Running);
    // a command for an outcome that does not exist is refused by the foreign key
    let ghost = OutcomeId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    assert!(
        store
            .enqueue_command(NewCommand {
                outcome_id: ghost,
                idempotency_key: "g".into(),
                kind: "outcome.start".into(),
                params: json!({}),
                expected_revision: None,
                from_surface: Surface::Cli,
            })
            .is_err()
    );
    // bad keys
    for key in ["", "has space", &"k".repeat(129)] {
        let r = store.enqueue_command(NewCommand {
            outcome_id: id.clone(),
            idempotency_key: key.into(),
            kind: "outcome.start".into(),
            params: json!({}),
            expected_revision: None,
            from_surface: Surface::Cli,
        });
        assert!(r.is_err(), "{key:?}");
    }
}

#[test]
fn a_surface_cannot_pose_as_another_in_the_database() {
    let e = env();
    let store = e.v5();
    let (w, _) = ready_outcome(&e, &store);
    let r = e.db().execute(
        "INSERT INTO outcome_commands(id,outcome_id,idempotency_key,kind,params,from_surface,state,created_at) \
         VALUES ('x',?1,'k','outcome.start','{}','attempt','queued','t')",
        params![w.id().as_str()],
    );
    assert!(r.is_err(), "only cli, tui and superagent exist");
}

// ----------------------------------------------------------------------- outbox

/// A fake effect with a log that outlives the "process": the log is the world.
struct World<'a> {
    log: &'a RefCell<Vec<String>>,
    /// Pretend the effect ran but its receipt never reached the database.
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

#[test]
fn a_crash_between_commit_and_dispatch_replays_the_effect_exactly_once() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    e.session(&store, "att-1", false);
    let it = item(&items, "implement").clone();
    let id = w.id().clone();
    // the attempt row and its worktree.create outbox row commit together...
    w.new_attempt(
        &it,
        1,
        "att-1",
        "local-inproc",
        &with_worktree_effect("wt-create:att-1"),
    )
    .unwrap();
    let rows = w.list_outbox(&id).unwrap();
    assert_eq!((rows.len(), rows[0].state), (1, OutboxState::Pending));
    // ...and the process dies before dispatching anything
    drop(w);

    let log = RefCell::new(vec![]);
    let now = OffsetDateTime::now_utc();
    let mut w = store.open_outcome(&id).unwrap();
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
    // running the dispatcher again (another restart) does nothing
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
    let (mut w, items) = ready_outcome(&e, &store);
    e.session(&store, "att-1", false);
    let it = item(&items, "implement").clone();
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
    // the dispatcher claims the row and performs the effect...
    let claimed = w.claim_outbox(OffsetDateTime::now_utc()).unwrap().unwrap();
    assert_eq!(
        (claimed.state, claimed.tries),
        (OutboxState::Dispatching, 1)
    );
    log.borrow_mut().push(claimed.idempotency_key.clone());
    // ...and dies before recording the receipt
    drop(w);

    let mut w = store.open_outcome(&id).unwrap();
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

#[test]
fn a_dispatching_row_whose_effect_never_happened_runs_again() {
    let e = env();
    let store = e.v5();
    let (mut w, items) = ready_outcome(&e, &store);
    e.session(&store, "att-1", false);
    let it = item(&items, "implement").clone();
    let id = w.id().clone();
    w.new_attempt(&it, 1, "att-1", "local-inproc", &with_worktree_effect("k"))
        .unwrap();
    w.claim_outbox(OffsetDateTime::now_utc()).unwrap().unwrap();
    drop(w);
    let log = RefCell::new(vec![]);
    let mut w = store.open_outcome(&id).unwrap();
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
    assert_eq!((run.done, run.reconciled), (1, 0));
    assert_eq!(*log.borrow(), ["k"]);
    assert_eq!(w.list_outbox(&id).unwrap()[0].tries, 2);
}

#[test]
fn the_outbox_key_is_unique_failures_back_off_and_stop_after_the_cap() {
    let e = env();
    let store = e.v5();
    let (mut w, _) = ready_outcome(&e, &store);
    let id = w.id().clone();
    let fx = with_worktree_effect("same");
    let both = w
        .atomically(|tx| Ok((tx.enqueue(&fx.outbox[0])?, tx.enqueue(&fx.outbox[0])?)))
        .unwrap();
    assert_eq!(both, (true, false));
    assert_eq!(w.list_outbox(&id).unwrap().len(), 1);

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
    // not due yet: nothing is claimed
    assert!(w.claim_outbox(now).unwrap().is_none());
    // each later pass fails again until the cap, then the row stays failed for a person
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
