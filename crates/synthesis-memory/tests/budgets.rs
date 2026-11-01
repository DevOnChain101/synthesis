//! Operations from memory.md §11: a schema newer than the build is read-only, and the budgets.

use rusqlite::{Connection, params};
use synthesis_memory::{Caps, Filter, NewMemory, OffsetDateTime, Query, Store, Trust};

const WS: &str = "aaaaaaaaaaaaaaaaaaaaaaaa";

fn flt(limit: usize) -> Filter {
    Filter {
        workspace: Some(WS.into()),
        kind: None,
        status: Some("active".into()),
        limit,
    }
}

#[test]
fn a_newer_schema_opens_read_only() {
    let d = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(d.path(), vec![]).unwrap();
        assert!(!s.read_only());
        s.put(NewMemory::new(
            "workspace",
            WS,
            "fact",
            "Build",
            "cargo build is the build command",
            Trust::Owner,
        ))
        .unwrap();
    }
    let c = Connection::open(d.path().join("memory/memory.db")).unwrap();
    c.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (99, 'x')",
        [],
    )
    .unwrap();
    drop(c);
    let mut s = Store::open(d.path(), vec![]).unwrap();
    assert!(s.read_only());
    // retrieval still works
    assert_eq!(s.search("build command", &flt(5)).unwrap().len(), 1);
    let r = s
        .retrieve(&Query::new(
            "build command",
            Some(WS),
            OffsetDateTime::now_utc(),
        ))
        .unwrap();
    assert_eq!(r.notes.len(), 1);
    // writes refuse with a clear message and nothing was migrated
    let e = s
        .put(NewMemory::new(
            "workspace",
            WS,
            "fact",
            "New",
            "a second note",
            Trust::Agent,
        ))
        .unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
    assert!(s.enqueue_job("w", "s", "t", "{}").is_err());
    assert_eq!(s.schema_version().unwrap(), 99);
}

fn seed(
    c: &mut Connection,
    scope: &str,
    key: &str,
    kind: &str,
    n: usize,
    pinned: bool,
    trust: &str,
) {
    let tx = c.transaction().unwrap();
    for i in 0..n {
        let at = format!(
            "2026-01-01T{:02}:{:02}:{:02}Z",
            i / 3600 % 24,
            i / 60 % 60,
            i % 60
        );
        tx.execute(
            "INSERT INTO memories(id, scope, scope_key, kind, title, body, trust, status, pinned, content_hash, source, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'active',?8,?9,'{}',?10,?10)",
            params![
                format!("{kind}-{scope}-{trust}-{pinned}-{i}"),
                scope,
                key,
                kind,
                format!("{kind} {i} topic{}", i % 97),
                format!("note {i} about module{} and handler{}", i % 211, i % 53),
                trust,
                pinned,
                format!("h-{kind}-{scope}-{trust}-{pinned}-{i}"),
                at
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn jan2() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_767_312_000).unwrap() // the day after the seeded records
}

fn count(c: &Connection, status: &str, kind: &str, scope: &str) -> i64 {
    c.query_row(
        "SELECT count(*) FROM memories WHERE status=?1 AND kind=?2 AND scope=?3",
        params![status, kind, scope],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn prune_archives_the_oldest_episodes_over_the_caps_and_spares_the_persons_notes() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path(), vec![]).unwrap();
    let mut c = Connection::open(s.path()).unwrap();
    seed(&mut c, "workspace", WS, "episode", 6, false, "agent");
    seed(&mut c, "workspace", WS, "fact", 3, false, "owner");
    seed(&mut c, "workspace", WS, "episode", 1, true, "agent");
    seed(&mut c, "user", "", "episode", 5, false, "agent");
    seed(&mut c, "user", "", "fact", 2, false, "owner");
    let caps = Caps {
        workspace: 8,
        user: 4,
        db_bytes: u64::MAX,
    };
    // workspace holds 10 active (cap 8), user 7 (cap 4)
    let (_, archived) = s.prune_capped(365, jan2(), &caps).unwrap();
    assert_eq!(archived, 2 + 3);
    assert_eq!(count(&c, "archived", "episode", "workspace"), 2);
    assert_eq!(count(&c, "archived", "episode", "user"), 3);
    assert_eq!(count(&c, "active", "fact", "workspace"), 3);
    // the oldest went first
    let oldest: String = c
        .query_row(
            "SELECT status FROM memories WHERE id='episode-workspace-agent-false-0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(oldest, "archived");
    let pinned: String = c
        .query_row(
            "SELECT status FROM memories WHERE id='episode-workspace-agent-true-0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pinned, "active");
    // a size cap archives a quarter of the active unpinned episodes
    let tiny = Caps {
        workspace: 1000,
        user: 1000,
        db_bytes: 1,
    };
    let (_, more) = s.prune_capped(365, jan2(), &tiny).unwrap();
    assert_eq!(more, 2); // 4 + 2 = 6 active unpinned episodes, a quarter rounded up
    assert!(s.stats().unwrap().over_budget.is_empty());
}

#[test]
fn retrieval_p95_on_10k_records_stays_under_a_generous_bound() {
    // The spec's 20 ms p95 is an unmeasured assumption (memory.md §11); this guards against a
    // regression to something unusable (debug build, shared CI runner), not against the target.
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path(), vec![]).unwrap();
    let mut c = Connection::open(s.path()).unwrap();
    seed(&mut c, "workspace", WS, "fact", 10_000, false, "agent");
    let now = OffsetDateTime::now_utc();
    let mut ms: Vec<u128> = (0..40)
        .map(|i| {
            let q = format!("module{} handler{} topic{}", i * 5, i % 53, i);
            let t = std::time::Instant::now();
            s.retrieve(&Query::new(&q, Some(WS), now)).unwrap();
            t.elapsed().as_millis()
        })
        .collect();
    ms.sort_unstable();
    let p95 = ms[ms.len() * 95 / 100];
    println!("retrieval p95 over 10k records: {p95} ms");
    assert!(p95 < 200, "p95 {p95} ms");
}
