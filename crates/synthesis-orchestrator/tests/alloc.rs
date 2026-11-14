//! Attempt allocation against real git repositories and a real v5 store (tempdirs).

use std::fs;
use std::path::Path;
use std::process::Command;

use synthesis_core::TaskStatus;
use synthesis_core::outcome::*;
use synthesis_orchestrator::{
    AllocError, AllocSpec, Allocator, Cleanup, Platform, cleanup_attempt, finish_attempt,
    retention_due,
};
use synthesis_session::outcome::*;

#[macro_use]
mod common;
use common::*;

#[test]
fn two_attempts_get_distinct_environments_and_the_users_tree_is_untouched() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let a = r.allocator();
    let e1 = a.allocate(&mut w, &r.spec(&one, 1), now()).unwrap();
    let e2 = a.allocate(&mut w, &r.spec(&two, 1), now()).unwrap();
    assert_ne!(e1.session_id, e2.session_id);
    assert_ne!(e1.git.worktree, e2.git.worktree);
    assert_ne!(e1.git.branch, e2.git.branch);
    assert_eq!(e1.git.branch, format!("synthesis/{}", e1.session_id));
    assert!(
        e1.git.worktree.starts_with(r.home.join("worktrees")),
        "{:?}",
        e1.git.worktree
    );
    // the explicit cwd of the attempt is its own worktree, never the user's folder or the other's
    assert_eq!(e1.context.cwd, e1.git.worktree);
    assert_ne!(e1.context.cwd, e2.context.cwd);
    assert_ne!(e1.context.cwd, r.repo);
    assert_eq!(e1.git.baseline, r.head);
    assert_eq!(e1.attempt.fence, 1);
    // stored: the attempt rows, the child sessions with their own cwd, the pinned git info
    for e in [&e1, &e2] {
        let s = r
            .store
            .reader()
            .unwrap()
            .get_session(&e.session_id)
            .unwrap();
        assert_eq!(Path::new(&s.cwd), e.context.cwd.as_path());
        assert_eq!(s.parent.unwrap().session_id, w.control_session_id());
        let stored: synthesis_workspace::GitInfo =
            serde_json::from_str(&s.git_info.unwrap()).unwrap();
        assert_eq!(stored, e.git);
    }
    assert_eq!(r.worktrees(), 3);
    assert_eq!(r.branches().len(), 2);
    assert_eq!(
        g(&r.repo, &["status", "--porcelain"]),
        "",
        "the user's tree is untouched"
    );
    assert_eq!(g(&r.repo, &["rev-parse", "HEAD"]), r.head);
    // each outbox row is done, with a receipt
    let rows = w.list_outbox(w.id()).unwrap();
    assert!(
        rows.len() == 2
            && rows
                .iter()
                .all(|x| x.state == OutboxState::Done && x.receipt.is_some())
    );
}

#[test]
fn a_replay_of_a_finished_allocation_changes_nothing() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let a = r.allocator();
    let e1 = a.allocate(&mut w, &r.spec(&one, 1), now()).unwrap();
    let e1b = a.allocate(&mut w, &r.spec(&one, 1), now()).unwrap();
    assert_eq!(
        (e1.session_id.clone(), e1.attempt.id.clone()),
        (e1b.session_id, e1b.attempt.id)
    );
    assert_eq!(r.worktrees(), 2);
    assert_eq!(w.list_attempts(&one).unwrap().len(), 1);
    assert_eq!(w.list_outbox(w.id()).unwrap().len(), 1);
}

/// Crash points of the allocation, each replayed: the session id recorded first is reused, the
/// worktree exists exactly once, and exactly one attempt row results.
#[test]
fn a_crash_at_any_step_is_reconciled_without_a_second_worktree() {
    need_git!();
    // (1) after the intent was recorded, before anything ran
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let sid = uuid_like(1);
    w.atomically(|tx| tx.enqueue(&intent(&r, &one, 1, &sid)))
        .unwrap();
    drop(w);
    let mut w = r
        .store
        .open_outcome(&r.outcome_id_of(&one), &policy())
        .unwrap();
    let e = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap();
    assert_eq!(e.session_id, sid, "the recorded session id is reused");
    assert_eq!(r.worktrees(), 2);

    // (2) the worktree exists, the session row does not (crash between the two)
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let sid = uuid_like(2);
    w.atomically(|tx| tx.enqueue(&intent(&r, &one, 1, &sid)))
        .unwrap();
    r.wm.allocate_unstarted(&r.repo, &sid, r.base.join("art").join(&sid), &r.head)
        .unwrap();
    assert_eq!(r.worktrees(), 2);
    let e = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap();
    assert!(
        e.session_id == sid && r.worktrees() == 2 && r.branches() == [format!("synthesis/{sid}")]
    );

    // (3) the session row exists without its git info (crash before it was recorded)
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let sid = uuid_like(3);
    w.atomically(|tx| tx.enqueue(&intent(&r, &one, 1, &sid)))
        .unwrap();
    let (alloc, _) =
        r.wm.allocate_unstarted(&r.repo, &sid, r.base.join("art").join(&sid), &r.head)
            .unwrap();
    drop(
        r.store
            .create_session(synthesis_session::NewSession {
                id: Some(sid.clone()),
                workspace: &alloc.context,
                model_config_id: "attempt".into(),
                permission_mode: synthesis_core::PermissionMode::Default,
                permission_policy_id: "orchestrator".into(),
                budget: synthesis_core::Budget::default(),
                parent: Some(synthesis_core::ParentRef {
                    session_id: w.control_session_id().into(),
                    tool_call_id: String::new(),
                }),
                title: None,
            })
            .unwrap(),
    );
    let e = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap();
    assert_eq!(
        e.git,
        alloc.git.unwrap(),
        "the git info was completed, not replaced"
    );
    assert_eq!(r.worktrees(), 2);

    // (4) dispatching when the process died, the effect fully done: reconciled by the probe
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let e0 = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap();
    let id = w.id().clone();
    // put the finished row back in `dispatching`, as a crash after the effect would leave it
    let row = w.list_outbox(&id).unwrap().remove(0);
    r.raw(&format!(
        "UPDATE outbox SET state='dispatching' WHERE id='{}'",
        row.id
    ));
    drop(w);
    let mut w = r.store.open_outcome(&id, &policy()).unwrap();
    let e = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap();
    assert_eq!(e.session_id, e0.session_id);
    assert_eq!(r.worktrees(), 2, "no second worktree");
    assert_eq!(w.list_outbox(&id).unwrap()[0].state, OutboxState::Done);
    assert_eq!(w.list_attempts(&one).unwrap().len(), 1);
}

#[test]
fn a_replay_of_a_started_attempt_does_not_touch_its_files() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let a = r.allocator();
    let e = a.allocate(&mut w, &r.spec(&one, 1), now()).unwrap();
    let rev = e.attempt.revision;
    w.transition_attempt(
        &e.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    fs::write(e.git.worktree.join("work.txt"), "in progress").unwrap();
    fs::write(e.git.worktree.join("a.txt"), "edited\n").unwrap();
    let again = a.allocate(&mut w, &r.spec(&one, 1), now()).unwrap();
    assert_eq!(again.session_id, e.session_id);
    assert_eq!(
        fs::read_to_string(e.git.worktree.join("work.txt")).unwrap(),
        "in progress"
    );
    assert_eq!(
        fs::read_to_string(e.git.worktree.join("a.txt")).unwrap(),
        "edited\n"
    );
}

#[test]
fn native_windows_and_a_folder_that_is_not_a_repository_are_refused_clearly() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let win = Allocator {
        platform: Platform {
            native_windows: true,
        },
        ..r.allocator()
    };
    let err = win.allocate(&mut w, &r.spec(&one, 1), now()).unwrap_err();
    assert!(matches!(err, AllocError::NativeWindows));
    assert!(err.to_string().contains("WSL2"), "{err}");
    assert!(
        w.list_outbox(w.id()).unwrap().is_empty(),
        "nothing was recorded"
    );
    assert_eq!(r.worktrees(), 1);

    let plain = r.base.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let (mut w, one, _) = r.outcome_in(&plain);
    let spec = AllocSpec {
        source: &plain,
        ..r.spec(&one, 1)
    };
    let err = r.allocator().allocate(&mut w, &spec, now()).unwrap_err();
    assert!(err.to_string().contains("not a git repository"), "{err}");
    assert!(w.list_attempts(&one).unwrap().is_empty());
    assert!(
        !r.home
            .join("worktrees")
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    );
}

#[test]
fn a_recorded_intent_with_a_hostile_session_id_is_refused() {
    need_git!();
    for (n, bad) in [
        "../evil",
        "a/b",
        "ABCDEF01-0000-4000-8000-000000000001",
        "short",
        "",
    ]
    .iter()
    .enumerate()
    {
        let r = rig();
        let (mut w, one, _) = r.outcome();
        w.atomically(|tx| tx.enqueue(&intent(&r, &one, 1, bad)))
            .unwrap();
        let err = r
            .allocator()
            .allocate(&mut w, &r.spec(&one, 1), now())
            .unwrap_err();
        assert!(matches!(err, AllocError::Effect(_)), "{n} {bad:?}: {err}");
        assert_eq!(r.worktrees(), 1, "{bad:?}");
        assert!(w.list_attempts(&one).unwrap().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_planted_where_the_worktree_goes_is_not_followed() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let sid = uuid_like(7);
    w.atomically(|tx| tx.enqueue(&intent(&r, &one, 1, &sid)))
        .unwrap();
    let wdir = r.home.join("worktrees");
    fs::create_dir_all(&wdir).unwrap();
    let outside = r.base.join("elsewhere");
    std::os::unix::fs::symlink(&outside, wdir.join(&sid)).unwrap();
    let err = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap_err();
    assert!(matches!(err, AllocError::Effect(_)), "{err}");
    assert!(!outside.exists(), "nothing was created through the link");
    assert!(w.list_attempts(&one).unwrap().is_empty());
}

#[test]
fn finish_commits_the_tree_and_cleanup_keeps_what_would_be_lost() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let a = r.allocator();
    // an attempt with edits: committed as the subject, then kept (unmerged), reason recorded
    let e = a.allocate(&mut w, &r.spec(&one, 1), now()).unwrap();
    let rev = e.attempt.revision;
    w.transition_attempt(
        &e.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    fs::write(e.git.worktree.join("new.txt"), "x\n").unwrap();
    let subject = finish_attempt(&r.wm, &mut w, &e.attempt, &e.git, TaskStatus::Completed).unwrap();
    assert_ne!(subject, r.head);
    assert_eq!(g(&e.git.worktree, &["rev-parse", "HEAD"]), subject);
    let done = w.list_attempts(&one).unwrap().remove(0);
    assert_eq!(
        (done.state, done.subject_commit.as_deref()),
        (AttemptState::Finished, Some(subject.as_str()))
    );
    let c = cleanup_attempt(&r.wm, &mut w, &done, &e.git).unwrap();
    assert!(
        matches!(&c, Cleanup::Retained(why) if why.contains("unmerged")),
        "{c:?}"
    );
    assert!(e.git.worktree.exists());
    let kept = w.list_attempts(&one).unwrap().remove(0);
    assert!(
        kept.retained_reason
            .as_deref()
            .unwrap()
            .contains("unmerged")
    );
    // a lost attempt with unsaved work is kept as well, never forced
    let e2 = a.allocate(&mut w, &r.spec(&two, 1), now()).unwrap();
    fs::write(e2.git.worktree.join("a.txt"), "unsaved\n").unwrap();
    w.transition_attempt(
        &e2.attempt.id,
        e2.attempt.revision,
        AttemptState::Lost,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    let lost = w.list_attempts(&two).unwrap().remove(0);
    assert!(matches!(
        cleanup_attempt(&r.wm, &mut w, &lost, &e2.git).unwrap(),
        Cleanup::Retained(_)
    ));
    // cleanup of an attempt that has not finished is an error, never a deletion
    let (mut w3, three) = (w, e.attempt.clone());
    let live = AttemptRecord {
        state: AttemptState::Running,
        ..three
    };
    assert!(cleanup_attempt(&r.wm, &mut w3, &live, &e.git).is_err());
    assert!(e.git.worktree.exists());
}

#[test]
fn an_untouched_attempt_is_removed_and_dirty_ones_are_not() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let a = r.allocator();
    // untouched: finish records the unchanged tip; cleanup removes worktree and branch
    let e = a.allocate(&mut w, &r.spec(&one, 1), now()).unwrap();
    let rev = e.attempt.revision;
    w.transition_attempt(
        &e.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    let subject = finish_attempt(&r.wm, &mut w, &e.attempt, &e.git, TaskStatus::Completed).unwrap();
    assert_eq!(subject, r.head, "unchanged tree: the subject is the base");
    let done = w.list_attempts(&one).unwrap().remove(0);
    assert_eq!(
        cleanup_attempt(&r.wm, &mut w, &done, &e.git).unwrap(),
        Cleanup::Removed
    );
    assert!(!e.git.worktree.exists());
    assert_eq!(r.worktrees(), 1);
    // an attempt that was cancelled with uncommitted work: kept, reason recorded, work intact
    let e2 = a.allocate(&mut w, &r.spec(&two, 1), now()).unwrap();
    fs::write(e2.git.worktree.join("a.txt"), "unsaved\n").unwrap();
    let rev = e2.attempt.revision;
    w.transition_attempt(
        &e2.attempt.id,
        rev,
        AttemptState::Cancelled,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    let cancelled = w.list_attempts(&two).unwrap().remove(0);
    let c = cleanup_attempt(&r.wm, &mut w, &cancelled, &e2.git).unwrap();
    assert!(
        matches!(&c, Cleanup::Retained(why) if why.contains("uncommitted")),
        "{c:?}"
    );
    assert_eq!(
        fs::read_to_string(e2.git.worktree.join("a.txt")).unwrap(),
        "unsaved\n"
    );
}

#[test]
fn retention_is_seven_days_by_default_and_only_for_parsable_times() {
    let ended = "2026-01-01T00:00:00.000Z";
    let at = |d: i64, h: i64| {
        time::macros::datetime!(2026-01-01 00:00 UTC)
            + time::Duration::days(d)
            + time::Duration::hours(h)
    };
    assert!(!retention_due(ended, at(6, 23), 7));
    assert!(retention_due(ended, at(7, 0), 7));
    assert!(retention_due(ended, at(30, 0), 7));
    assert!(!retention_due("garbage", at(400, 0), 7));
    assert!(!retention_due("", at(400, 0), 7));
}

/// F1: an allocation does not run, fail or burn tries on rows of other kinds or other keys.
#[test]
fn allocate_leaves_other_outbox_rows_alone() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let other_kind = NewOutbox {
        idempotency_key: "verify:1".into(),
        kind: OutboxKind::VerifierRun,
        payload: serde_json::json!({}),
    };
    // another item's pending worktree.create, with a payload that would fail if it ran
    let other_item = NewOutbox {
        idempotency_key: format!("worktree.create:{two}:1"),
        kind: OutboxKind::WorktreeCreate,
        payload: serde_json::json!({"session_id": "not-a-uuid"}),
    };
    w.atomically(|tx| {
        tx.enqueue(&other_kind)?;
        tx.enqueue(&other_item)
    })
    .unwrap();
    for _ in 0..3 {
        r.allocator()
            .allocate(&mut w, &r.spec(&one, 1), now())
            .unwrap();
    }
    let rows = w.list_outbox(w.id()).unwrap();
    let by = |k: &str| {
        rows.iter()
            .find(|x| x.idempotency_key == k)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (by("verify:1").state, by("verify:1").tries),
        (OutboxState::Pending, 0)
    );
    let k = format!("worktree.create:{two}:1");
    assert_eq!((by(&k).state, by(&k).tries), (OutboxState::Pending, 0));
}

#[test]
fn the_source_must_be_the_outcomes_repository_and_a_removed_worktree_says_so() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let other = r.base.join("other-repo");
    fs::create_dir_all(&other).unwrap();
    g(&other, &["init", "-q"]);
    fs::write(other.join("x"), "x").unwrap();
    g(&other, &["add", "."]);
    g(&other, &["commit", "-qm", "x"]);
    let spec = AllocSpec {
        source: &other,
        ..r.spec(&one, 1)
    };
    let err = r.allocator().allocate(&mut w, &spec, now()).unwrap_err();
    assert!(err.to_string().contains("outcome's repository"), "{err}");
    assert!(w.list_outbox(w.id()).unwrap().is_empty());
    assert_eq!(g(&other, &["worktree", "list"]).lines().count(), 1);
    // a stored row naming another repository is not followed either
    let mut forged = intent(&r, &two, 1, &uuid_like(9));
    forged.payload["source"] = serde_json::json!(other.to_str().unwrap());
    w.atomically(|tx| tx.enqueue(&forged)).unwrap();
    let err = r
        .allocator()
        .allocate(&mut w, &r.spec(&two, 1), now())
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("different effect") || msg.contains("outcome's repository"),
        "{msg}"
    );
    assert_eq!(g(&other, &["worktree", "list"]).lines().count(), 1);
    // a finished attempt whose worktree was removed: a clear error that names the branch
    let e = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap();
    w.transition_attempt(
        &e.attempt.id,
        e.attempt.revision,
        AttemptState::Lost,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    let lost = w.list_attempts(&one).unwrap().remove(0);
    assert_eq!(
        cleanup_attempt(&r.wm, &mut w, &lost, &e.git).unwrap(),
        Cleanup::Removed
    );
    let err = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), now())
        .unwrap_err();
    assert!(
        err.to_string().contains("was removed") && err.to_string().contains("synthesis/"),
        "{err}"
    );
}
