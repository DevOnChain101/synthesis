//! `synt outcome ...` (plan PR-13): the CLI surface of outcome orchestration. Thin over
//! [`OutcomeClient`] on the `LocalSocket` transport, attached as surface `cli`. It renders what the
//! server says and decides nothing: no verdict, state, binding or brief is computed here, no option
//! is ever preselected, and a decision is answered only with an explicit choice id plus the
//! revision and brief hash of the card the person was shown (ADR-021, threat T-34, T-37).
//!
//! Exit codes: 0 done; 1 the server did not apply the command (stale, already answered, denied,
//! still queued); 2 refused or failed (outcomes off, native Windows, bad input, link error).
//! Everything printed from the server is made printable first; nothing secret is read or shown.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_core::outcome::{Contract, ContractCheck, ContractCriterion, Envelope};
use synthesis_harness::sanitize_for_display as clean;
use synthesis_session::outcome::{CommandState, Surface};
use synthesis_view::outcomes::{
    CommandReply, DecisionCard, InspectorTab, OutcomeRow, Picker, WSL2_REFUSAL, brief_rows,
};

use crate::outcome_client::{OutcomeClient, Transport};
use crate::{homes, launch_dir};

/// Longest definition file read (criteria, envelope, checks).
const MAX_FILE: u64 = 256 * 1024;
/// How long a mutating command is followed while the owner has not applied it yet.
const WAIT: Duration = Duration::from_secs(10);
/// `schema` of every `--json` object: bumped only with an incompatible change.
const JSON_SCHEMA: u32 = 1;

#[derive(Args)]
pub struct OutcomeArgs {
    /// Machine-readable output: one JSON object with a `schema` field.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: OutcomeCmd,
}

#[derive(Subcommand)]
pub enum OutcomeCmd {
    /// Create an outcome (contract v1, waiting for your approval).
    Create {
        /// What must be true when it is done.
        #[arg(long)]
        goal: String,
        /// JSON array of criteria (`id`, `text`, optional `mandatory`, `required_trust`, `check_ids`).
        #[arg(long, value_name = "FILE")]
        criteria_file: PathBuf,
        /// JSON envelope (`allowed_paths`, `denied_paths`, `capabilities`, `budgets`).
        #[arg(long, value_name = "FILE")]
        envelope_file: PathBuf,
        /// JSON array of frozen checks (`id`, `command`) run on a fresh checkout of the result.
        #[arg(long, value_name = "FILE")]
        checks_file: Option<PathBuf>,
        /// Full commit id the work starts from (for example the output of `git rev-parse HEAD`).
        #[arg(long)]
        base_commit: String,
        /// Repository folder (default: the current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Show a contract: what you are about to approve.
    Contract {
        id: String,
        #[arg(long)]
        version: Option<u32>,
    },
    /// Approve the contract you were shown: the version and hash must be named.
    Approve {
        id: String,
        #[arg(long)]
        version: u32,
        #[arg(long)]
        hash: String,
    },
    Start(Cmd),
    Pause(Cmd),
    Resume(Cmd),
    Cancel(Cmd),
    /// Acknowledge a verified outcome (clears it from "needs you").
    Ack(Cmd),
    /// Every outcome with what needs you.
    List,
    /// One outcome: needs-you line, contract, work items, evidence, decisions.
    Status {
        id: String,
    },
    /// Decisions of an outcome, each brief shown whole (one decision when DECISION is given).
    Decisions {
        id: String,
        decision: Option<String>,
    },
    /// Answer a pending decision. There is no default: name `--choice` with the `--revision` and
    /// `--brief-hash` of the decision as `decisions` shows it, or pick on a terminal.
    Resolve {
        outcome: String,
        decision: String,
        #[arg(long)]
        choice: Option<String>,
        #[arg(long)]
        revision: Option<i64>,
        #[arg(long)]
        brief_hash: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Look closer: `--tab overview|workers|activity|diff|evidence`, or one `--attempt`.
    Inspect {
        id: String,
        #[arg(long, default_value = "overview")]
        tab: String,
        #[arg(long)]
        attempt: Option<String>,
    },
    /// The outcome's event log after a cursor.
    Events {
        id: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Migrate the store for outcomes (after you set `synt.orchestrator.enabled`); backs up first.
    Enable {
        /// Confirm that you read the notice.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args)]
pub struct Cmd {
    pub id: String,
    /// Apply only if the outcome is still at this revision.
    #[arg(long)]
    pub expected_revision: Option<i64>,
}

impl OutcomeCmd {
    /// Everything that changes state or reaches the owner's command queue: refused on native
    /// Windows (only the reads stay).
    fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Contract { .. }
                | Self::List
                | Self::Status { .. }
                | Self::Decisions { .. }
                | Self::Inspect { .. }
                | Self::Events { .. }
        )
    }
}

/// The refusal a command meets on this platform, if any.
pub fn platform_refusal(native_windows: bool, cmd: &OutcomeCmd) -> Option<&'static str> {
    (native_windows && cmd.mutates()).then_some(WSL2_REFUSAL)
}

pub fn off_text() -> String {
    "outcomes are off (synt.orchestrator.enabled is false). Set it to true in your user settings to use them; synt never turns it on by itself".into()
}

/// What `enable` shows before it migrates anything (plan 3b item 6).
pub const ENABLE_NOTICE: &str = "Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. User and managed hooks still run on the host, outside the sandbox.\nEnabling migrates the session store to schema v5 after an automatic backup (backups/pre-v5-*). Releases without outcomes refuse a migrated store.";

pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub input: &'a mut dyn BufRead,
    /// A person is at a terminal: `resolve` may offer the numbered picker.
    pub interactive: bool,
}

pub fn run_cmd(env: &Env, a: OutcomeArgs) -> Result<u8, String> {
    if let Some(why) = platform_refusal(cfg!(windows), &a.cmd) {
        return Err(why.into());
    }
    let h = homes(env)?;
    let st = crate::outcome_service::settings_of(env, &h);
    if !crate::outcome_policy::outcome_policy(&st.synthesis.orchestrator).enabled {
        return Err(off_text());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let link =
        crate::orch_endpoint::LocalSocket::connect_or_start(&h.synthesis, Surface::Cli, &exe)
            .map_err(|e| e.to_string())?;
    let mut client = OutcomeClient::new(link, Surface::Cli);
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal() && !a.json;
    let (mut out, mut input) = (std::io::stdout().lock(), stdin.lock());
    dispatch(
        &mut client,
        a.cmd,
        a.json,
        &mut Io {
            out: &mut out,
            input: &mut input,
            interactive,
        },
        WAIT,
    )
}

fn e<E: ToString>(x: E) -> String {
    x.to_string()
}

fn text(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(s)) => clean(s),
        Some(Value::Null) | None => String::new(),
        Some(o) => clean(&o.to_string()),
    }
}

fn read_json<T: DeserializeOwned>(path: &std::path::Path, what: &str) -> Result<T, String> {
    // a plain file only: no symlink, and no FIFO or device that would block the open or surprise
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = o
        .open(path)
        .map_err(|x| format!("{what}: {}: {x}", path.display()))?;
    // the check is on the open handle, so the file cannot be swapped between check and read
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return Err(format!("{what}: {} is not a regular file", path.display()));
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(f, MAX_FILE + 1), &mut buf)
        .map_err(|x| format!("{what}: {x}"))?;
    if buf.len() as u64 > MAX_FILE {
        return Err(format!("{what}: longer than {MAX_FILE} bytes"));
    }
    serde_json::from_str(&buf).map_err(|x| format!("{what}: {}: {x}", path.display()))
}

struct P<'a, 'b>(&'a mut Io<'b>, bool);

impl P<'_, '_> {
    fn line(&mut self, s: &str) -> Result<(), String> {
        writeln!(self.0.out, "{s}").map_err(e)
    }

    /// `--json`: the object with its schema; text mode never reaches it.
    fn json(&mut self, mut v: Value) -> Result<(), String> {
        v["schema"] = json!(JSON_SCHEMA);
        self.line(&v.to_string())
    }
}

fn rows_text(v: &Value) -> String {
    brief_rows(v)
        .iter()
        .map(|r| format!("{}:\n{}", r.label, r.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_json(r: &OutcomeRow) -> Value {
    json!({"id": r.id, "goal": r.goal, "state": r.state, "revision": r.revision,
           "needs_you": {"decisions_pending": r.needs_you.decisions_pending,
                         "blocked": r.needs_you.blocked,
                         "verified_awaiting_ack": r.needs_you.verified_awaiting_ack}})
}

fn card_json(c: &DecisionCard) -> Value {
    let pick = Picker::new(c).map(|p| p.options);
    json!({"id": c.id, "kind": c.kind, "state": c.state, "revision": c.revision,
           "brief_sha256": c.brief_sha256, "expires_at": c.expires_at,
           "rows": c.rows.iter().map(|r| json!({"label": r.label, "text": r.text})).collect::<Vec<_>>(),
           "options": pick.unwrap_or_default().iter().map(|o| json!({"id": o.id, "label": o.label})).collect::<Vec<_>>()})
}

fn needs_line(r: &OutcomeRow) -> String {
    let n = &r.needs_you;
    let mut parts = vec![];
    if n.decisions_pending > 0 {
        parts.push(format!("{} decision(s) pending", n.decisions_pending));
    }
    if n.blocked {
        parts.push("blocked".into());
    }
    if n.verified_awaiting_ack {
        parts.push(format!(
            "verified, waiting for your ack (synt outcome ack {})",
            r.id
        ));
    }
    if parts.is_empty() {
        "Needs you: nothing".into()
    } else {
        format!("Needs you: {}", parts.join("; "))
    }
}

/// A decision as the person must see it to answer: the server's state and every non-empty brief
/// field, then the revision and brief hash to quote and the answers, in order, with no marker.
pub fn render_card(c: &DecisionCard) -> String {
    let mut s = c.render_text();
    s.push_str(&format!(
        "Revision: {}\nBrief hash: {}\n",
        c.revision, c.brief_sha256
    ));
    match Picker::new(c) {
        Some(p) => {
            s.push_str("Answers (none is chosen for you):\n");
            for (i, o) in p.options.iter().enumerate() {
                s.push_str(&format!("  {}. {}: {}\n", i + 1, o.id, o.label));
            }
        }
        None => s.push_str(&format!(
            "This decision cannot be answered ({}).\n",
            c.state
        )),
    }
    s
}

fn settle<T: Transport>(
    c: &mut OutcomeClient<T>,
    outcome: &str,
    mut r: CommandReply,
    wait: Duration,
) -> Result<CommandReply, String> {
    let end = Instant::now() + wait;
    while r.state == CommandState::Queued && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(150));
        r = c.command_status(outcome, &r.idempotency_key).map_err(e)?;
    }
    Ok(r)
}

/// Prints what the server answered; 0 only when it applied the command.
fn report(p: &mut P, r: &CommandReply) -> Result<u8, String> {
    if p.1 {
        p.json(json!({"command": {"key": r.idempotency_key, "kind": r.kind,
            "state": r.state, "message": r.describe(), "result": r.result}}))?;
    } else {
        p.line(&format!("{}: {}", clean(&r.kind), r.describe()))?;
        if !r.result.is_null() {
            p.line(&clean(&r.result.to_string()))?;
        }
    }
    Ok(u8::from(r.state != CommandState::Applied))
}

fn key() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn mutate<T: Transport>(
    c: &mut OutcomeClient<T>,
    p: &mut P,
    kind: &str,
    a: Cmd,
    wait: Duration,
) -> Result<u8, String> {
    let r = c
        .command(kind, &a.id, a.expected_revision, &key())
        .map_err(e)?;
    let r = settle(c, &a.id, r, wait)?;
    report(p, &r)
}

fn find_card<T: Transport>(
    c: &mut OutcomeClient<T>,
    outcome: &str,
    id: &str,
) -> Result<DecisionCard, String> {
    c.decisions(outcome)
        .map_err(e)?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(|| format!("no decision {} in this outcome", clean(id)))
}

/// One command against `c`; `wait` is how long a queued command is followed.
pub fn dispatch<T: Transport>(
    c: &mut OutcomeClient<T>,
    cmd: OutcomeCmd,
    json: bool,
    io: &mut Io,
    wait: Duration,
) -> Result<u8, String> {
    let mut p = P(io, json);
    match cmd {
        OutcomeCmd::Create {
            goal,
            criteria_file,
            envelope_file,
            checks_file,
            base_commit,
            cwd,
        } => {
            let criteria: Vec<ContractCriterion> = read_json(&criteria_file, "criteria")?;
            let envelope: Envelope = read_json(&envelope_file, "envelope")?;
            let checks: Vec<ContractCheck> = match checks_file {
                Some(f) => read_json(&f, "checks")?,
                None => vec![],
            };
            let dir = match cwd {
                Some(d) => d,
                None => launch_dir()?,
            };
            let dir = synthesis_core::canonicalize(&dir)
                .map_err(|x| format!("{}: {x}", dir.display()))?;
            let ws = dir.to_str().ok_or("the folder path is not valid UTF-8")?;
            let contract = Contract {
                version: 1,
                goal,
                criteria,
                envelope,
                checks,
            };
            let v = c.create(ws, &base_commit, contract).map_err(e)?;
            let (id, hash) = (text(&v, "id"), text(&v, "contract_hash"));
            // both are printed in commands to paste: a shape the server does not make is not shown
            if !synthesis_view::outcomes::valid_alt_id(&id)
                || !synthesis_view::outcomes::is_hex(&hash, 64)
            {
                return Err("the orchestrator sent an outcome id or contract hash of an unexpected shape; nothing is shown".into());
            }
            if json {
                p.json(json!({"id": id, "state": text(&v, "state"),
                    "contract_version": v.get("contract_version"), "contract_hash": hash}))?;
            } else {
                p.line(&format!("Created outcome {id} ({}).", text(&v, "state")))?;
                p.line(&format!("Review: synt outcome contract {id}"))?;
                p.line(&format!(
                    "Approve: synt outcome approve {id} --version {} --hash {hash}",
                    text(&v, "contract_version")
                ))?;
            }
            Ok(0)
        }
        OutcomeCmd::Contract { id, version } => {
            let v = c.contract(&id, version).map_err(e)?;
            if json {
                p.json(json!({"version": v.get("version"), "hash": v.get("hash"),
                    "approved": v.get("approved"), "contract": v.get("contract")}))?;
            } else {
                p.line(&format!(
                    "Contract v{} {} ({})",
                    text(&v, "version"),
                    text(&v, "hash"),
                    if v["approved"].as_bool() == Some(true) {
                        "approved"
                    } else {
                        "not approved"
                    }
                ))?;
                p.line(&rows_text(&v["contract"]))?;
            }
            Ok(0)
        }
        OutcomeCmd::Approve { id, version, hash } => {
            let r = c.approve_contract(&id, version, &hash, &key()).map_err(e)?;
            let r = settle(c, &id, r, wait)?;
            report(&mut p, &r)
        }
        OutcomeCmd::Start(a) => mutate(c, &mut p, "start", a, wait),
        OutcomeCmd::Pause(a) => mutate(c, &mut p, "pause", a, wait),
        OutcomeCmd::Resume(a) => mutate(c, &mut p, "resume", a, wait),
        OutcomeCmd::Cancel(a) => mutate(c, &mut p, "cancel", a, wait),
        OutcomeCmd::Ack(a) => mutate(c, &mut p, "ack", a, wait),
        OutcomeCmd::List => {
            let rows = c.outcomes(None).map_err(e)?;
            if json {
                p.json(json!({"outcomes": rows.iter().map(row_json).collect::<Vec<_>>()}))?;
            } else {
                for r in &rows {
                    p.line(&format!(
                        "{}  {}  r{}  {}",
                        r.id, r.state, r.revision, r.goal
                    ))?;
                    p.line(&format!("    {}", needs_line(r)))?;
                }
                p.line(&format!("{} outcome(s)", rows.len()))?;
            }
            Ok(0)
        }
        OutcomeCmd::Status { id } => {
            let o = c.outcome(&id).map_err(e)?;
            let ct = c.contract(&id, None).map_err(e)?;
            let plan = c.plan(&id).map_err(e)?;
            let ev = c.evidence(&id).map_err(e)?;
            let ds = c.decisions(&id).map_err(e)?;
            let items = plan["items"].as_array().cloned().unwrap_or_default();
            if json {
                p.json(json!({"outcome": row_json(&o),
                    "contract": {"version": ct.get("version"), "hash": ct.get("hash"), "approved": ct.get("approved")},
                    "plan_version": plan.get("plan_version"), "work_items": items, "evidence": ev,
                    "decisions": ds.iter().map(card_json).collect::<Vec<_>>()}))?;
                return Ok(0);
            }
            p.line(&needs_line(&o))?;
            p.line(&format!(
                "Outcome {}: {} (revision {})",
                o.id, o.state, o.revision
            ))?;
            p.line(&format!("Goal: {}", o.goal))?;
            p.line(&format!(
                "Contract: v{} {} ({})",
                text(&ct, "version"),
                text(&ct, "hash"),
                if ct["approved"].as_bool() == Some(true) {
                    "approved"
                } else {
                    "not approved"
                }
            ))?;
            p.line("Work items:")?;
            for i in &items {
                p.line(&format!(
                    "  {} [{}] {}, retries used {}",
                    text(i, "key"),
                    text(i, "kind"),
                    text(i, "state"),
                    text(i, "retries_used")
                ))?;
            }
            p.line("Evidence:")?;
            for r in &ev {
                p.line(&format!(
                    "  {} {} ({}) commit {} criteria {}",
                    text(r, "check_id"),
                    text(r, "result"),
                    text(r, "trust_class"),
                    text(r, "subject_commit"),
                    text(r, "criteria_ids")
                ))?;
            }
            let pending = ds.iter().filter(|d| d.resolvable()).count();
            p.line(&format!(
                "Decisions: {} total, {pending} pending (synt outcome decisions {})",
                ds.len(),
                o.id
            ))?;
            if o.state == "verified" {
                p.line("Verified means the independent checks passed for the commit shown above; it does not prove a criterion is met in any other sense.")?;
            }
            Ok(0)
        }
        OutcomeCmd::Decisions { id, decision } => {
            let ds: Vec<_> = c
                .decisions(&id)
                .map_err(e)?
                .into_iter()
                .filter(|d| decision.as_ref().is_none_or(|w| *w == d.id))
                .collect();
            if json {
                p.json(json!({"decisions": ds.iter().map(card_json).collect::<Vec<_>>()}))?;
            } else {
                for d in &ds {
                    p.line(&render_card(d))?;
                }
                p.line(&format!("{} decision(s)", ds.len()))?;
            }
            Ok(0)
        }
        OutcomeCmd::Resolve {
            outcome,
            decision,
            choice,
            revision,
            brief_hash,
            note,
        } => {
            let card = find_card(c, &outcome, &decision)?;
            let chosen = match choice {
                Some(ch) => {
                    let (Some(rev), Some(hash)) = (revision, brief_hash) else {
                        return Err("resolve needs --revision and --brief-hash of the decision as `synt outcome decisions` shows it, next to --choice".into());
                    };
                    if rev != card.revision || hash != card.brief_sha256 {
                        if !json {
                            p.line("The decision changed since you saw it. It is now:")?;
                            p.line(&render_card(&card))?;
                        }
                        return Err("stale: the revision or brief hash is not the current one; nothing was sent".into());
                    }
                    ch
                }
                None if p.0.interactive && card.resolvable() => {
                    p.line(&render_card(&card))?;
                    p.line("Your answer (number, empty to cancel):")?;
                    let mut l = String::new();
                    p.0.input.read_line(&mut l).map_err(e)?;
                    let mut pick = Picker::new(&card).ok_or("this decision cannot be answered")?;
                    let n: usize = l
                        .trim()
                        .parse()
                        .map_err(|_| "no answer given; nothing was sent")?;
                    pick.move_to(n.wrapping_sub(1));
                    pick.choice()
                        .ok_or("no such answer; nothing was sent")?
                        .to_string()
                }
                None => {
                    if !json {
                        p.line(&render_card(&card))?;
                    }
                    return Err(format!(
                        "no answer was chosen for you: repeat with --choice ID --revision {} --brief-hash {}",
                        card.revision, card.brief_sha256
                    ));
                }
            };
            let r = c
                .resolve(&outcome, &card, &chosen, &key(), note.as_deref())
                .map_err(e)?;
            let r = settle(c, &outcome, r, wait)?;
            report(&mut p, &r)
        }
        OutcomeCmd::Inspect { id, tab, attempt } => {
            if let Some(a) = attempt {
                let v = c.attempt(&id, &a).map_err(e)?;
                json_or(&mut p, v.clone(), &rows_text(&v["attempt"]))?;
                return Ok(0);
            }
            let t = InspectorTab::parse(&tab).ok_or_else(|| {
                format!(
                    "no tab {}: overview, workers, activity, diff or evidence",
                    clean(&tab)
                )
            })?;
            inspect(c, &mut p, &id, t)?;
            Ok(0)
        }
        OutcomeCmd::Events { id, after } => {
            let (evs, next) = c.events(&id, after).map_err(e)?;
            if json {
                p.json(json!({"events": evs, "next_event_id": next}))?;
            } else {
                for ev in &evs {
                    p.line(&event_line(ev))?;
                }
                p.line(&format!("next: --after {next}"))?;
            }
            Ok(0)
        }
        OutcomeCmd::Enable { yes } => {
            if !yes {
                p.line(ENABLE_NOTICE)?;
                return Err("not enabled: read the notice, then repeat with --yes".into());
            }
            let v = c.enable().map_err(e)?;
            json_or(
                &mut p,
                v.clone(),
                &format!("Outcomes enabled for this store: {}", clean(&v.to_string())),
            )?;
            Ok(0)
        }
    }
}

fn json_or(p: &mut P, v: Value, text: &str) -> Result<(), String> {
    if p.1 { p.json(v) } else { p.line(text) }
}

fn event_line(ev: &Value) -> String {
    format!(
        "{}  {}  {}",
        text(ev, "seq"),
        text(ev, "type"),
        clean(&ev.to_string())
    )
}

fn inspect<T: Transport>(
    c: &mut OutcomeClient<T>,
    p: &mut P,
    id: &str,
    tab: InspectorTab,
) -> Result<(), String> {
    let mut out = json!({"tab": tab.label().to_lowercase()});
    let mut text_out = vec![format!("[{}]", tab.label())];
    match tab {
        InspectorTab::Overview => {
            let o = c.outcome(id).map_err(e)?;
            let ct = c.contract(id, None).map_err(e)?;
            text_out.push(needs_line(&o));
            text_out.push(format!("{}: {} r{}", o.id, o.state, o.revision));
            text_out.push(rows_text(&ct["contract"]));
            out["outcome"] = row_json(&o);
            out["contract"] = ct;
        }
        InspectorTab::Workers => {
            let plan = c.plan(id).map_err(e)?;
            let mut items = vec![];
            for i in plan["items"].as_array().cloned().unwrap_or_default() {
                let w = c.work_item(id, &text(&i, "id")).map_err(e)?;
                text_out.push(format!(
                    "{} [{}] {}",
                    text(&w, "key"),
                    text(&w, "kind"),
                    text(&w, "state")
                ));
                for a in w["attempts"].as_array().cloned().unwrap_or_default() {
                    text_out.push(format!("  attempt {}", rows_text(&a).replace('\n', " ")));
                }
                items.push(w);
            }
            out["work_items"] = json!(items);
        }
        InspectorTab::Activity => {
            let (evs, next) = c.events(id, 0).map_err(e)?;
            text_out.extend(evs.iter().map(event_line));
            out["events"] = json!(evs);
            out["next_event_id"] = json!(next);
        }
        InspectorTab::Diff => {
            text_out.push("not available: protocol v4 has no diff method; the attempt's branch and session are inspectable with git and `synt sessions show`".into());
            out["available"] = json!(false);
        }
        InspectorTab::Evidence => {
            let ev = c.evidence(id).map_err(e)?;
            text_out.extend(ev.iter().map(|r| rows_text(r).replace('\n', " ")));
            out["evidence"] = json!(ev);
        }
    }
    json_or(p, out, &text_out.join("\n"))
}

#[cfg(test)]
mod tests;
