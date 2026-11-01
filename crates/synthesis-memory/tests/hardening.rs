//! Re-verification of the store against memory.md §9/§11: concurrent first open, private modes,
//! usage accounting behind `stats` and the daily USD cap.

use synthesis_memory::{OffsetDateTime, Store};

#[test]
fn many_processes_opening_a_fresh_store_at_once_all_succeed() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().to_path_buf();
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let h = home.clone();
            std::thread::spawn(move || Store::open(&h, vec![]).map(|s| s.schema_version().unwrap()))
        })
        .collect();
    for t in threads {
        assert_eq!(t.join().unwrap().unwrap(), 3);
    }
}

#[cfg(unix)]
#[test]
fn an_existing_loose_directory_and_file_are_tightened() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let db = dir.join("memory.db");
    std::fs::write(&db, b"").unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    Store::open(d.path(), vec![]).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&db), 0o600);
}

#[test]
fn usage_feeds_stats_and_the_daily_cost() {
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path(), vec![]).unwrap();
    for (tokens, notes) in [(100, 2), (300, 1), (0, 0), (900, 3)] {
        s.record_retrieval("s", tokens, notes, 5).unwrap();
    }
    let id = s.enqueue_job("w", "s", "t", "{}").unwrap();
    let now = OffsetDateTime::now_utc();
    s.claim_job("w", now).unwrap().unwrap();
    s.record_job_usage(&id, 1200, Some(0.125)).unwrap();
    s.finish_job(&id, None).unwrap();
    let st = s.stats().unwrap();
    assert_eq!((st.retrievals, st.hit_rate), (4, Some(0.75)));
    assert_eq!(st.injected_tokens_p50, Some(100));
    assert_eq!(st.injected_tokens_p95, Some(900));
    assert!((st.reflection_cost_usd_7d - 0.125).abs() < 1e-9 && st.reflection_tokens_7d == 1200);
    assert!((st.reflection_cost_usd_30d - 0.125).abs() < 1e-9);
    assert!((s.reflection_cost_today(now).unwrap() - 0.125).abs() < 1e-9);
}

#[test]
fn two_equally_relevant_notes_are_both_retrieved() {
    // Min-max scaling used to give the weaker of two candidates a score of 0, and a score under
    // the match floor drops a note: the second relevant note never appeared.
    use synthesis_memory::{NewMemory, Query, Trust};
    let d = tempfile::tempdir().unwrap();
    let mut s = Store::open(d.path(), vec![]).unwrap();
    for (t, b) in [
        (
            "Parser tests",
            "run the flaky parser tests with --test-threads=1",
        ),
        (
            "Parser build",
            "the parser build needs the flaky generator step first",
        ),
    ] {
        s.put(NewMemory::new("workspace", "w", "fact", t, b, Trust::Owner))
            .unwrap();
    }
    let q = Query::new("fix the flaky parser", Some("w"), OffsetDateTime::now_utc());
    assert_eq!(s.retrieve(&q).unwrap().notes.len(), 2);
}
