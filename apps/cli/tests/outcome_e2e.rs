//! A whole outcome through the real `synt outcome` CLI, the real owner process and a real worker
//! (`synt run --unattended`) that replays a scripted model: create, approve, start, an attempt
//! that edits a worktree, independent verification, verdict and acknowledgement, a decision a
//! blocked worker really produced, and kill -9 of the owner at each lifecycle point (AC-04).
//!
//! The owner takes the scripted model and the pause points from `SYNT_TEST_ORCH_*`, which exist
//! only in builds with feature `test-provider` (debug builds; release is a `compile_error`).
//! Needs an enforcing sandbox (a worker refuses to start without one): a host without one skips,
//! unless `SYNT_REQUIRE_BWRAP` is set. Unix only (the Windows named pipe is unverified).
#![cfg(all(unix, feature = "test-provider"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use synthesis_core::outcome::{AttemptState, OutcomeId};
use synthesis_session::Reader;
use synthesis_session::outcome::{AttemptRecord, EvidenceRecord};

const SYNT: &str = env!("CARGO_BIN_EXE_synt");

fn g(dir: &Path, args: &[&str]) -> String {
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
    assert!(o.status.success(), "git {args:?}");
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// One scenario at a time: each starts an owner, workers and sandboxed checks, and a client waits
/// only five seconds for a new owner to publish its endpoint (a loaded debug build can be slower).
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Rig {
    _lock: std::sync::MutexGuard<'static, ()>,
    _t: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    head: String,
    script: PathBuf,
    hold: PathBuf,
    gate: PathBuf,
}

/// Stops the owner a command started, and any worker it left, whatever happens to the test.
impl Drop for Rig {
    fn drop(&mut self) {
        self.kill_owner();
        for (_, pid) in self.workers() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
    }
}

/// The enforcing sandbox a worker needs; `false` = skip (or fail where CI requires one).
fn confines() -> bool {
    let d = tempfile::tempdir().unwrap();
    let o = Command::new(SYNT)
        .args(["doctor", "--cwd"])
        .arg(d.path())
        .env("SYNT_HOME", d.path())
        .output()
        .unwrap();
    let ok = String::from_utf8_lossy(&o.stdout).contains("enforce_ready: yes");
    assert!(
        ok || std::env::var_os("SYNT_REQUIRE_BWRAP").is_none_or(|v| v.is_empty()),
        "SYNT_REQUIRE_BWRAP is set but this host has no enforcing sandbox"
    );
    if !ok {
        println!("SKIP: no enforcing sandbox on this host");
    }
    ok
}

/// What the scripted model does: write `src/hello.txt`, then say it is done. With `gated`, the
/// model answers once (its usage is recorded) and then waits for the gate file: a worker that
/// stays mid-attempt for as long as the test wants.
fn writes(gated: Option<&Path>) -> Value {
    let first = json!({"text": "writing", "tools": [{"id": "w1", "name": "Write",
        "input": {"file_path": "src/hello.txt", "content": "hi\n"}}]});
    let mut second = json!({"text": "done"});
    if let Some(p) = gated {
        second["after_file"] = json!(p);
    }
    json!([first, second, {"text": "done"}, {"text": "done"}])
}

fn rig() -> Rig {
    let lock = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // a short path: a unix socket path is limited to about 100 bytes
    let t = tempfile::Builder::new()
        .prefix("e")
        .tempdir_in("/tmp")
        .unwrap();
    let base = std::fs::canonicalize(t.path()).unwrap();
    let (home, repo) = (base.join("h"), base.join("repo"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("settings.json"),
        json!({"synt": {"orchestrator": {"enabled": true},
            "pricing": {"fake/fake-model": {"input_per_mtok": 1.0, "output_per_mtok": 1.0}}}})
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    g(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    g(&repo, &["add", "."]);
    g(&repo, &["commit", "-qm", "init"]);
    let head = g(&repo, &["rev-parse", "HEAD"]);
    let r = Rig {
        _lock: lock,
        script: base.join("script.json"),
        hold: base.join("hold"),
        gate: base.join("gate"),
        _t: t,
        base,
        home,
        repo,
        head,
    };
    r.set_script(&writes(None));
    r
}

impl Rig {
    fn set_script(&self, v: &Value) {
        std::fs::write(&self.script, v.to_string()).unwrap();
    }

    /// `(exit code, stdout, stderr)`; the environment is the home, the scripted model and nothing else.
    fn synt(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(SYNT)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("SYNT_HOME", &self.home)
            .env("HOME", &self.base)
            .env("SYNT_TEST_ORCH_SCRIPT", &self.script)
            .env("SYNT_TEST_ORCH_HOLD", &self.hold)
            .arg("outcome")
            .args(args)
            .output()
            .unwrap();
        (
            o.status.code().unwrap(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.synt(args);
        assert_eq!(code, 0, "{args:?}: {out} {err}\n{}", self.logs());
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = args.to_vec();
        a.push("--json");
        let out = self.ok(&a);
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"))
    }

    /// The status of an outcome for a failure message; never panics.
    fn json_lenient(&self, args: &[&str]) -> String {
        let mut a = args.to_vec();
        a.push("--json");
        let st = self.synt(&a).1;
        let ev = self.synt(&["events", args[1], "--json"]).1;
        let ev: Value = serde_json::from_str(ev.trim()).unwrap_or_default();
        let ev: Vec<String> = ev["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|e| format!("{} {}", e["type"], e["payload"]))
            .collect();
        format!("{st}\n{}", ev.join("\n"))
    }

    fn file(&self, name: &str, v: Value) -> String {
        let p = self.base.join(name);
        std::fs::write(&p, v.to_string()).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// What the owner and the workers said (for a failing assertion).
    fn logs(&self) -> String {
        ["run/orch.log", "run/orch-worker.log"]
            .iter()
            .map(|f| {
                format!(
                    "--- {f}\n{}",
                    std::fs::read_to_string(self.home.join(f)).unwrap_or_default()
                )
            })
            .collect()
    }

    fn owner_pid(&self) -> Option<u64> {
        let v = std::fs::read(self.home.join("run/orch.json")).ok()?;
        serde_json::from_slice::<Value>(&v).ok()?["pid"].as_u64()
    }

    /// kill -9 of the owner (no cleanup runs), then waits until it is gone.
    fn kill_owner(&self) {
        let Some(pid) = self.owner_pid() else { return };
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        let end = Instant::now() + Duration::from_secs(10);
        while Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .unwrap()
            .status
            .success()
            && Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn reader(&self) -> Reader {
        Reader::open(&self.home).unwrap()
    }

    /// `(attempt id, worker pid)` of every attempt whose worker may still run.
    fn workers(&self) -> Vec<(String, u32)> {
        let Ok(r) = Reader::open(&self.home) else {
            return vec![];
        };
        let mut out = vec![];
        for o in r.list_outcomes().unwrap_or_default() {
            for it in r.list_work_items(&o.id).unwrap_or_default() {
                for a in r.list_attempts(&it.id).unwrap_or_default() {
                    if let Some(pid) = a.worker_pid {
                        out.push((a.id.to_string(), pid));
                    }
                }
            }
        }
        out
    }

    /// Polls `f` until it is `Some`, or fails with the logs.
    fn wait<T>(&self, what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}\n{}\n{}",
                self.logs(),
                self.json_lenient(&["status", &self.oid().to_string()])
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn oid(&self) -> OutcomeId {
        let mut all = self.reader().list_outcomes().unwrap();
        assert_eq!(all.len(), 1, "exactly one outcome, never a duplicate");
        all.remove(0).id
    }

    /// Every attempt of the work item `key`, oldest first.
    fn attempts(&self, id: &str, key: &str) -> Vec<AttemptRecord> {
        let rd = self.reader();
        let item = self.json(&["status", id])["work_items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == key)
            .unwrap_or_else(|| panic!("no work item {key}"))["id"]
            .as_str()
            .unwrap()
            .to_string();
        let item = rd
            .list_work_items(&self.oid())
            .unwrap()
            .into_iter()
            .find(|i| i.id.as_str() == item)
            .unwrap();
        rd.list_attempts(&item.id).unwrap()
    }

    fn evidence(&self) -> Vec<EvidenceRecord> {
        self.reader().list_evidence(&self.oid()).unwrap()
    }

    /// The events of the outcome's control session of one type.
    fn events(&self, id: &str, ty: &str) -> Vec<Value> {
        self.json(&["events", id])["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == ty)
            .cloned()
            .collect()
    }

    fn state(&self, id: &str) -> String {
        self.json(&["status", id])["outcome"]["state"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn wait_state(&self, id: &str, want: &str, secs: u64) {
        self.wait(&format!("outcome {want}"), secs, || {
            (self.state(id) == want).then_some(())
        });
    }

    /// enable, create (a contract whose frozen check needs the file the worker writes), approve.
    fn approved(&self) -> String {
        self.approved_with(json!({"max_cost_usd": 5.0}))
    }

    /// [`Rig::approved`] with these budgets in the envelope.
    fn approved_with(&self, budgets: Value) -> String {
        self.ok(&["enable", "--yes"]);
        let criteria = self.file(
            "criteria.json",
            json!([{"id": "c1", "text": "hello.txt exists", "check_ids": ["t"]}]),
        );
        let checks = self.file(
            "checks.json",
            json!([{"id": "t", "command": "test -f src/hello.txt"}]),
        );
        let envelope = self.file(
            "envelope.json",
            json!({"allowed_paths": ["src/**"], "capabilities": {"repo.read": "allow", "repo.write": "allow"},
                   "budgets": budgets}),
        );
        let made = self.json(&[
            "create",
            "--goal",
            "write src/hello.txt",
            "--criteria-file",
            &criteria,
            "--checks-file",
            &checks,
            "--envelope-file",
            &envelope,
            "--base-commit",
            &self.head,
            "--cwd",
            &self.repo.to_string_lossy(),
        ]);
        let (id, hash) = (
            made["id"].as_str().unwrap().to_string(),
            made["contract_hash"].as_str().unwrap().to_string(),
        );
        self.ok(&["approve", &id, "--version", "1", "--hash", &hash]);
        id
    }
}

/// What must hold after any run, kills included: nothing waits in the queue, the contract was
/// approved and the outcome started once each, and the verdict rests on exactly one passing
/// independent check of the frozen contract (produced by the runner, never by a worker).
fn assert_one_verdict(r: &Rig, id: &str) {
    assert!(
        r.reader().queued_commands(&r.oid()).unwrap().is_empty(),
        "a command was lost"
    );
    assert_eq!(r.events(id, "outcome.contract_approved").len(), 1);
    let started: Vec<_> = r
        .events(id, "outcome.command_finished")
        .into_iter()
        .filter(|e| e["payload"]["kind"] == "outcome.start")
        .collect();
    assert_eq!(started.len(), 1, "start applied once: {started:?}");
    let ev = r.evidence();
    let t: Vec<_> = ev.iter().filter(|e| e.check_id == "t").collect();
    assert_eq!(t.len(), 1, "one verdict row: {ev:?}");
    assert!(
        t[0].result == "passed" && t[0].exit_code == Some(0) && t[0].flags.is_empty(),
        "{:?}",
        t[0]
    );
    for e in &ev {
        assert_eq!(e.producer, "independent_runner", "{e:?}");
    }
    // the checked commit is the one the implement worker made
    let imp = r.attempts(id, "implement");
    let done = imp
        .iter()
        .find(|a| a.state == AttemptState::Finished)
        .expect("a finished attempt");
    assert_eq!(
        done.subject_commit.as_deref(),
        Some(t[0].subject_commit.as_str())
    );
}

fn go(r: &Rig) -> String {
    let id = r.approved();
    r.ok(&["start", &id]);
    id
}

/// [`go`] with an envelope that caps each attempt, so one attempt does not hold the whole cap.
fn go_with(r: &Rig, budgets: Value) -> String {
    let id = r.approved_with(budgets);
    r.ok(&["start", &id]);
    id
}

#[test]
fn a_worker_edits_a_worktree_and_an_independent_check_gives_the_verdict() {
    if !confines() {
        return;
    }
    let r = rig();
    let id = go(&r);
    r.wait_state(&id, "verified", 90);
    assert_one_verdict(&r, &id);
    let s = r.json(&["status", &id]);
    assert_eq!(
        s["outcome"]["needs_you"]["verified_awaiting_ack"], true,
        "{s}"
    );
    // the worker's commit holds the file; the user's repository was not touched
    let imp = r.attempts(&id, "implement");
    assert_eq!(imp.len(), 1, "{imp:?}");
    let commit = imp[0].subject_commit.clone().unwrap();
    assert_eq!(
        g(&r.repo, &["show", &format!("{commit}:src/hello.txt")]),
        "hi"
    );
    // the owner recorded the sandbox it resolved at launch (the worker never reports it)
    assert!(
        imp[0]
            .sandbox_backend
            .as_deref()
            .is_some_and(|b| !b.is_empty())
    );
    assert!(
        matches!(imp[0].sandbox_guarantee.as_deref(), Some("weak" | "strong")),
        "{imp:?}"
    );
    assert_eq!(g(&r.repo, &["rev-parse", "HEAD"]), r.head);
    assert_eq!(g(&r.repo, &["status", "--porcelain"]), "");
    assert!(!r.repo.join("src/hello.txt").exists());
    assert!(
        r.events(&id, "outcome.recovered").is_empty(),
        "nothing needed recovery"
    );
    let out = r.ok(&["inspect", &id, "--tab", "evidence"]);
    assert!(
        out.contains("check_id: t") && out.contains("result: passed"),
        "{out}"
    );
    // the person acknowledges the verdict through the same owner
    let out = r.ok(&["ack", &id]);
    assert!(out.starts_with("outcome.ack: done"), "{out}");
    assert_eq!(r.state(&id), "completed");
}

#[test]
fn a_worker_that_says_done_without_the_file_is_not_verified() {
    if !confines() {
        return;
    }
    let r = rig();
    // the model claims success and changes nothing: the frozen check, not the claim, decides
    r.set_script(&json!([{"text": "all done, it works"}, {"text": "done"}, {"text": "done"}]));
    let id = go(&r);
    let state = r.wait("an outcome that stopped", 90, || {
        let s = r.state(&id);
        (!matches!(s.as_str(), "running" | "ready" | "verifying")).then_some(s)
    });
    assert_ne!(state, "verified");
    assert!(
        r.evidence()
            .iter()
            .all(|e| !(e.check_id == "t" && e.result == "passed"))
    );
    assert_eq!(state, "blocked", "the failed check blocks it");
    let (code, out, err) = r.synt(&["ack", &id]);
    assert!(
        code != 0 && err.is_empty() || out.contains("refused"),
        "{code} {out} {err}"
    );
    assert_eq!(
        r.state(&id),
        "blocked",
        "only a verified outcome is acknowledged"
    );
}

#[test]
fn a_decision_a_blocked_worker_made_is_answered_only_with_an_explicit_choice() {
    if !confines() {
        return;
    }
    let r = rig();
    // the first attempt asks a question and stops: nobody can answer it in an unattended run
    r.set_script(&json!([{"text": "asking", "tools": [{"id": "q1", "name": "AskUserQuestion", "input": {"questions": [
        {"question": "Which API shape?", "options": [{"label": "REST"}, {"label": "gRPC"}]}]}}]},
        {"text": "done"}, {"text": "done"}]));
    let id = go(&r);
    let card = r.wait("a pending decision", 90, || {
        let v = r.json(&["decisions", &id]);
        v["decisions"].as_array()?.first().cloned()
    });
    let did = card["id"].as_str().unwrap().to_string();
    let (rev, hash) = (
        card["revision"].as_i64().unwrap().to_string(),
        card["brief_sha256"].as_str().unwrap().to_string(),
    );
    assert_eq!(card["state"], "pending");
    let out = r.ok(&["decisions", &id]);
    for want in [
        "pending",
        "Which API shape?",
        "If you do nothing:",
        "HOLD",
        "1. REST: REST",
        "2. gRPC: gRPC",
        "none is chosen for you",
    ] {
        assert!(out.contains(want), "{want} in {out}");
    }
    // no choice: nothing is sent; a choice the card does not offer, a stale revision: refused
    let (code, _, err) = r.synt(&["resolve", &id, &did]);
    assert_eq!(code, 2);
    assert!(err.contains("no answer was chosen for you"), "{err}");
    for (choice, revision) in [("MAYBE", rev.as_str()), ("REST", "99")] {
        let (code, ..) = r.synt(&[
            "resolve",
            &id,
            &did,
            "--choice",
            choice,
            "--revision",
            revision,
            "--brief-hash",
            &hash,
        ]);
        assert_eq!(code, 2, "{choice} {revision}");
    }
    assert_eq!(
        r.json(&["decisions", &id])["decisions"][0]["state"],
        "pending"
    );
    assert_eq!(r.attempts(&id, "inspect").len(), 1, "nothing ran again yet");
    // the person answers; the next attempt of the item is told and does the work
    r.set_script(&writes(None));
    let out = r.ok(&[
        "resolve",
        &id,
        &did,
        "--choice",
        "REST",
        "--revision",
        &rev,
        "--brief-hash",
        &hash,
    ]);
    assert!(out.starts_with("decision.resolve: done"), "{out}");
    // the answer moves the blocked outcome by itself (no `resume`): the owner re-checks and runs on
    r.wait_state(&id, "verified", 90);
    let a = r.attempts(&id, "inspect");
    assert_eq!(
        a.len(),
        2,
        "one blocked attempt, one after the answer: {a:?}"
    );
    assert_ne!(
        r.json(&["decisions", &id])["decisions"][0]["state"],
        "pending"
    );
    // a second answer to the same card is not applied
    let (code, out, err) = r.synt(&[
        "resolve",
        &id,
        &did,
        "--choice",
        "gRPC",
        "--revision",
        &rev,
        "--brief-hash",
        &hash,
    ]);
    assert_ne!(code, 0, "{out} {err}");
    assert_eq!(r.attempts(&id, "inspect").len(), 2);
}

#[test]
fn request_evidence_gets_a_new_card_that_names_the_old_one_and_only_it_can_be_answered() {
    if !confines() {
        return;
    }
    let r = rig();
    r.set_script(&json!([{"text": "asking", "tools": [{"id": "q1", "name": "AskUserQuestion", "input": {"questions": [
        {"question": "Which API shape?", "options": [{"label": "REST"}, {"label": "gRPC"}]}]}}]},
        {"text": "done"}, {"text": "done"}]));
    let id = go(&r);
    let old = r.wait("a pending decision", 90, || {
        r.json(&["decisions", &id])["decisions"]
            .as_array()?
            .first()
            .cloned()
    });
    let (did, rev, hash) = (
        old["id"].as_str().unwrap().to_string(),
        old["revision"].as_i64().unwrap().to_string(),
        old["brief_sha256"].as_str().unwrap().to_string(),
    );
    let out = r.ok(&[
        "resolve",
        &id,
        &did,
        "--choice",
        "request_evidence",
        "--revision",
        &rev,
        "--brief-hash",
        &hash,
    ]);
    assert!(
        out.contains("new one"),
        "the reply says a new decision follows: {out}"
    );
    // the owner closes the old card and stores a new one (no model runs for it)
    let new = r.wait("the new card", 90, || {
        r.json(&["decisions", &id])["decisions"]
            .as_array()?
            .iter()
            .find(|d| d["state"] == "pending" && d["id"] != old["id"])
            .cloned()
    });
    let text = new["rows"].to_string();
    assert!(text.contains(&did) && text.contains("supersedes"), "{text}");
    let all = r.json(&["decisions", &id]);
    let states: Vec<_> = all["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(states, ["invalidated", "pending"], "{all}");
    // the old card cannot be answered any more
    let (code, ..) = r.synt(&[
        "resolve",
        &id,
        &did,
        "--choice",
        "REST",
        "--revision",
        &rev,
        "--brief-hash",
        &hash,
    ]);
    assert_ne!(code, 0);
    // the person answers the new card and the work goes on
    r.set_script(&writes(None));
    let (nid, nrev, nhash) = (
        new["id"].as_str().unwrap().to_string(),
        new["revision"].as_i64().unwrap().to_string(),
        new["brief_sha256"].as_str().unwrap().to_string(),
    );
    r.ok(&[
        "resolve",
        &id,
        &nid,
        "--choice",
        "REST",
        "--revision",
        &nrev,
        "--brief-hash",
        &nhash,
    ]);
    r.wait_state(&id, "verified", 90);
}

// ------------------------------------------------------------ AC-04: kill -9 at each point

/// Brings the outcome to its end after the owner was killed: the next command starts a new owner,
/// which adopts the outcome. Returns the id once it is `verified`.
fn recover_to_verified(r: &Rig, id: &str) {
    let _ = std::fs::remove_file(&r.hold);
    r.wait_state(id, "verified", 120);
    assert_one_verdict(r, id);
}

#[test]
fn kill9_after_create_the_contract_is_still_pending_and_approval_continues() {
    if !confines() {
        return;
    }
    let r = rig();
    r.ok(&["enable", "--yes"]);
    let id = {
        let criteria = r.file(
            "criteria.json",
            json!([{"id": "c1", "text": "hello.txt exists", "check_ids": ["t"]}]),
        );
        let checks = r.file(
            "checks.json",
            json!([{"id": "t", "command": "test -f src/hello.txt"}]),
        );
        let envelope = r.file("envelope.json", json!({"allowed_paths": ["src/**"], "capabilities": {"repo.read": "allow", "repo.write": "allow"}, "budgets": {"max_cost_usd": 5.0}}));
        let v = r.json(&[
            "create",
            "--goal",
            "write src/hello.txt",
            "--criteria-file",
            &criteria,
            "--checks-file",
            &checks,
            "--envelope-file",
            &envelope,
            "--base-commit",
            &r.head,
            "--cwd",
            &r.repo.to_string_lossy(),
        ]);
        v["id"].as_str().unwrap().to_string()
    };
    r.kill_owner();
    // a new owner serves the same outcome: one outcome, same contract, still to be approved
    let s = r.json(&["status", &id]);
    assert_eq!(s["outcome"]["state"], "contract_pending", "{s}");
    assert_eq!(s["contract"]["approved"], false);
    assert_eq!(r.oid().as_str(), id);
    let hash = s["contract"]["hash"].as_str().unwrap().to_string();
    r.ok(&["approve", &id, "--version", "1", "--hash", &hash]);
    r.ok(&["start", &id]);
    recover_to_verified(&r, &id);
}

#[test]
fn kill9_after_approve_the_approval_holds_and_start_continues() {
    if !confines() {
        return;
    }
    let r = rig();
    let id = r.approved();
    r.kill_owner();
    let s = r.json(&["status", &id]);
    assert_eq!(s["contract"]["approved"], true, "{s}");
    assert_eq!(s["outcome"]["state"], "contract_pending", "{s}");
    r.ok(&["start", &id]);
    recover_to_verified(&r, &id);
}

#[test]
fn kill9_after_start_before_any_worker_spawns_no_attempt_is_lost_or_doubled() {
    if !confines() {
        return;
    }
    let r = rig();
    // the scheduler is held: start is applied and planned, no worker is spawned
    std::fs::write(&r.hold, "").unwrap();
    let id = go(&r);
    let s = r.json(&["status", &id]);
    assert_eq!(s["outcome"]["state"], "running", "{s}");
    assert_eq!(s["work_items"].as_array().unwrap().len(), 3);
    assert!(r.workers().is_empty(), "no worker was spawned");
    r.kill_owner();
    recover_to_verified(&r, &id);
    for key in ["inspect", "implement"] {
        let a = r.attempts(&id, key);
        assert_eq!(a.len(), 1, "{key}: one attempt, none doubled: {a:?}");
    }
    assert!(
        r.events(&id, "outcome.recovered").is_empty(),
        "there was nothing to recover"
    );
}

/// Starts the gated scenario and waits until the first agent worker has had its first turn.
fn mid_attempt(r: &Rig) -> (String, AttemptRecord) {
    mid_attempt_with(r, json!({"max_cost_usd": 5.0}))
}

fn mid_attempt_with(r: &Rig, budgets: Value) -> (String, AttemptRecord) {
    r.set_script(&writes(Some(&r.gate)));
    let id = go_with(r, budgets);
    let first = r.wait("a running attempt", 90, || {
        r.attempts(&id, "inspect")
            .into_iter()
            .find(|a| a.state == AttemptState::Running && a.worker_pid.is_some())
    });
    // it has had one model turn (usage recorded) and written the file, and now waits at the gate
    let wt = r
        .home
        .join("worktrees")
        .join(&first.session_id)
        .join("src/hello.txt");
    r.wait("the worker's first turn", 60, || wt.exists().then_some(()));
    assert!(r.evidence().is_empty());
    (id, first)
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn kill9_of_the_owner_the_worker_ends_alone_and_the_next_owner_settles_it_once() {
    if !confines() {
        return;
    }
    let r = rig();
    let (id, first) = mid_attempt(&r);
    r.kill_owner();
    // the orphaned worker finishes its attempt with nobody to record it
    std::fs::write(&r.gate, "").unwrap();
    let pid = first.worker_pid.unwrap();
    r.wait("the orphaned worker to exit", 60, || {
        (!alive(pid)).then_some(())
    });
    assert!(
        r.evidence().is_empty(),
        "no one wrote evidence while the owner was dead"
    );
    // a new owner records what the worker left, once, and carries on
    r.ok(&["status", &id]);
    recover_to_verified(&r, &id);
    let rec = r.events(&id, "outcome.recovered");
    assert_eq!(rec.len(), 1, "{rec:?}");
    assert_eq!(rec[0]["payload"]["settled"], 1);
    let a = r.attempts(&id, "inspect");
    assert_eq!(a.len(), 1, "never run a second time: {a:?}");
    assert_eq!((&a[0].id, a[0].state), (&first.id, AttemptState::Finished));
    assert_eq!(r.attempts(&id, "implement").len(), 1);
}

#[test]
fn kill9_of_the_owner_with_a_live_worker_the_next_owner_leaves_it_alone() {
    if !confines() {
        return;
    }
    let r = rig();
    let (id, first) = mid_attempt(&r);
    r.kill_owner();
    r.ok(&["status", &id]);
    let rec = r.wait("the recovery report", 60, || {
        Some(r.events(&id, "outcome.recovered")).filter(|e| !e.is_empty())
    });
    assert_eq!(
        (
            rec[0]["payload"]["alive"].as_u64(),
            rec[0]["payload"]["lost"].as_u64()
        ),
        (Some(1), Some(0)),
        "{rec:?}"
    );
    assert!(
        alive(first.worker_pid.unwrap()),
        "the worker was not killed by the adoption"
    );
    // while the worker lives the outcome is NOT blocked by its own reservation (it holds the whole
    // cap, which is its share, not an overrun) and nothing else starts
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(r.state(&id), "running");
    assert!(r.attempts(&id, "implement").is_empty());
    assert!(r.evidence().is_empty(), "no evidence while it runs");
    // the worker finishes while the new owner watches: the owner settles it from what the session
    // recorded (never a made-up result) and the outcome continues to its verdict
    std::fs::write(&r.gate, "").unwrap();
    recover_to_verified(&r, &id);
    let a = r.attempts(&id, "inspect");
    assert_eq!(a.len(), 1, "still one attempt, never rerun: {a:?}");
    assert_eq!((&a[0].id, a[0].state), (&first.id, AttemptState::Finished));
    assert_eq!(r.attempts(&id, "implement").len(), 1);
    assert_eq!(r.events(&id, "outcome.recovered").len(), 1);
}

#[test]
fn kill9_of_the_owner_and_its_worker_mid_attempt_the_lost_attempt_is_not_rerun() {
    if !confines() {
        return;
    }
    let r = rig();
    let (id, first) = mid_attempt(&r);
    // the whole machine goes down: the owner and its worker
    r.kill_owner();
    let _ = Command::new("kill")
        .args(["-9", &first.worker_pid.unwrap().to_string()])
        .output();
    std::fs::write(&r.gate, "").unwrap();
    r.ok(&["status", &id]);
    let rec = r.wait("the recovery report", 60, || {
        Some(r.events(&id, "outcome.recovered")).filter(|e| !e.is_empty())
    });
    assert_eq!(
        (
            rec[0]["payload"]["lost"].as_u64(),
            rec[0]["payload"]["alive"].as_u64()
        ),
        (Some(1), Some(0)),
        "{rec:?}"
    );
    // the lost attempt's reservation counts as spent (its session never recorded a final usage), so
    // the outcome stops, fail closed, with a reason: it neither reruns the attempt nor continues
    r.wait_state(&id, "blocked", 60);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(r.state(&id), "blocked");
    let why = r
        .events(&id, "outcome.state_changed")
        .into_iter()
        .last()
        .unwrap();
    assert!(
        why["payload"]["reason"]
            .as_str()
            .unwrap()
            .contains("cost cap"),
        "{why}"
    );
    let a = r.attempts(&id, "inspect");
    assert_eq!(a.len(), 1, "no second attempt, no second worktree: {a:?}");
    assert_eq!((&a[0].id, a[0].state), (&first.id, AttemptState::Lost));
    assert!(r.attempts(&id, "implement").is_empty());
    assert!(r.evidence().is_empty(), "a lost attempt leaves no evidence");
    assert_eq!(r.events(&id, "outcome.recovered").len(), 1, "reported once");
    assert!(r.reader().queued_commands(&r.oid()).unwrap().is_empty());
    // the reason names the real numbers: the lost attempt's whole reservation counts as spent
    assert!(
        why["payload"]["reason"]
            .as_str()
            .unwrap()
            .contains("$5.0000 of $5.0000"),
        "{why}"
    );
    // `resume` re-checks and, with no budget left, blocks again: no rerun of the lost attempt
    r.ok(&["resume", &id]);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(r.state(&id), "blocked");
    assert_eq!(r.attempts(&id, "inspect").len(), 1);
    assert!(r.attempts(&id, "implement").is_empty());
}

#[test]
fn kill9_of_the_owner_and_its_worker_with_budget_left_a_new_attempt_does_the_work() {
    if !confines() {
        return;
    }
    let r = rig();
    // each attempt may spend at most 0.5 of the 5.0: the lost one's 0.5 counts as spent, 4.5 is left
    let (id, first) = mid_attempt_with(
        &r,
        json!({"max_cost_usd": 5.0, "attempt_max_cost_usd": 0.5}),
    );
    r.kill_owner();
    let _ = Command::new("kill")
        .args(["-9", &first.worker_pid.unwrap().to_string()])
        .output();
    std::fs::write(&r.gate, "").unwrap();
    r.ok(&["status", &id]);
    recover_to_verified(&r, &id);
    let a = r.attempts(&id, "inspect");
    assert_eq!(a.len(), 2, "the lost attempt and one new one: {a:?}");
    assert_eq!((&a[0].id, a[0].state), (&first.id, AttemptState::Lost));
    assert_ne!(
        a[0].session_id, a[1].session_id,
        "a new session, not a rerun"
    );
    assert_eq!(a[1].state, AttemptState::Finished);
    assert_eq!(r.events(&id, "outcome.recovered").len(), 1);
}

#[test]
fn kill9_during_verification_the_check_is_run_again_and_evidence_is_not_duplicated() {
    if !confines() {
        return;
    }
    let r = rig();
    r.ok(&["enable", "--yes"]);
    // a check that takes a while: the owner dies while it runs
    let criteria = r.file(
        "criteria.json",
        json!([{"id": "c1", "text": "hello.txt exists", "check_ids": ["t"]}]),
    );
    let checks = r.file(
        "checks.json",
        json!([{"id": "t", "command": "sleep 8; test -f src/hello.txt"}]),
    );
    let envelope = r.file("envelope.json", json!({"allowed_paths": ["src/**"], "capabilities": {"repo.read": "allow", "repo.write": "allow"}, "budgets": {"max_cost_usd": 5.0}}));
    let v = r.json(&[
        "create",
        "--goal",
        "write src/hello.txt",
        "--criteria-file",
        &criteria,
        "--checks-file",
        &checks,
        "--envelope-file",
        &envelope,
        "--base-commit",
        &r.head,
        "--cwd",
        &r.repo.to_string_lossy(),
    ]);
    let (id, hash) = (
        v["id"].as_str().unwrap().to_string(),
        v["contract_hash"].as_str().unwrap().to_string(),
    );
    r.ok(&["approve", &id, "--version", "1", "--hash", &hash]);
    r.ok(&["start", &id]);
    r.wait("the verify item to run", 90, || {
        let s = r.json(&["status", &id]);
        let it = s["work_items"]
            .as_array()?
            .iter()
            .find(|i| i["key"] == "test")?
            .clone();
        matches!(it["state"].as_str()?, "running" | "verifying").then_some(())
    });
    assert!(
        r.evidence().iter().all(|e| e.check_id != "t"),
        "the check has not finished"
    );
    r.kill_owner();
    recover_to_verified(&r, &id);
}

#[test]
fn kill9_after_the_verdict_before_the_ack_the_verdict_stands_and_ack_completes() {
    if !confines() {
        return;
    }
    let r = rig();
    let id = go(&r);
    r.wait_state(&id, "verified", 90);
    let before = r.evidence().len();
    r.kill_owner();
    assert_eq!(r.state(&id), "verified");
    assert_eq!(r.evidence().len(), before, "adoption wrote no evidence");
    assert_one_verdict(&r, &id);
    let out = r.ok(&["ack", &id]);
    assert!(out.starts_with("outcome.ack: done"), "{out}");
    assert_eq!(r.state(&id), "completed");
    assert_eq!(r.evidence().len(), before);
    assert_one_verdict(&r, &id);
}
