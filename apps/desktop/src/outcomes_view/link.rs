//! The Outcomes tab's link to the orchestrator: one worker thread that owns the `OutcomeClient` (the only way
//! this app talks to outcomes, ADR-021) so the UI thread never waits on a socket. It is a request/reply loop:
//! the window sends `Req`s, the worker sends `Msg`s back. Between requests it waits for the push line
//! (`attach push:["decision"]`): a hint only says "look again", so it triggers a refetch, and without any hint
//! the worker refetches every `POLL` anyway (a lost hint costs a delay, never a wrong card).
//!
//! The server is not trusted and every read can fail: a failed link ends the worker with an error the window
//! shows; a refused or unreadable single part (the decisions, the contract, a tab) is shown as that part's error.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::Value;
use synthesis_app::outcome_client::{ClientError, OutcomeClient, Transport};
use synthesis_config::Env;
use synthesis_session::outcome::{CommandState, Surface};
use synthesis_view::outcomes::{
    AttemptView, BudgetView, CommandReply, CriteriaView, DecisionCard, InspectorContent,
    InspectorTab, OverviewTab, is_hex, valid_alt_id,
};

use super::form::{DiffView, Review, Submit};
use super::model::{Detail, Item, Screen};

/// How long without a hint before the worker looks again.
const POLL: Duration = Duration::from_secs(20);
/// Hints are coalesced: at most one refetch per this long, however many hints come.
const MIN_REFETCH: Duration = Duration::from_secs(1);
/// How long one wait for a hint lasts: the longest a request can sit unread.
const HINT_WAIT: Duration = Duration::from_millis(200);
/// Outcomes whose budget is fetched for the list (one call each).
const MAX_BUDGETS: usize = 50;
/// Event pages (200 each) read for History.
const MAX_PAGES: usize = 5;
/// Events the Activity tab of the inspector shows (the newest).
const ACTIVITY_TAIL: usize = 100;
/// How long a queued answer is followed.
const SETTLE: Duration = Duration::from_secs(5);

pub enum Req {
    Refresh,
    /// Check and connect again (after the person enabled outcomes or started the orchestrator).
    Retry,
    Select(String),
    Inspect(InspectorTab),
    Resolve {
        outcome: String,
        card: Box<DecisionCard>,
        choice: String,
    },
    /// Create or revise (the key is the form's: a retry after a lost reply sends the same one).
    Submit(Submit),
    Approve {
        outcome: String,
        version: u32,
        hash: String,
        key: String,
    },
    /// Read the stored contract of an outcome whose contract is not approved, for review.
    Review(String),
    /// The explicit store migration, after the person confirmed the disclosure. Only served while the
    /// orchestrator says the store is not migrated; never sent by itself.
    /// The number of the person's confirm (`State::migrations`): the reply carries it back.
    Enable(u64),
}

/// What the worker says to the open form.
pub enum FormMsg {
    Head {
        ws: String,
        result: Result<String, String>,
    },
    /// A refusal or a failure. `spent`: the server answered, so the idempotency key is used up; otherwise the
    /// reply is lost and the same request goes out again with the same key.
    Failed {
        text: String,
        spent: bool,
    },
    /// The server stored the contract: read it (and its diff) before approving.
    Review(Box<Review>),
    Approved(String),
    /// `HEAD` moved since the form showed its base commit: nothing was created.
    BaseMoved {
        ws: String,
        commit: String,
        text: String,
    },
}

pub enum Msg {
    Screen(Screen),
    Snapshot {
        items: Vec<Item>,
        selected: Option<String>,
        detail: Option<Box<Detail>>,
        inspector: Option<InspectorContent>,
    },
    Inspector {
        outcome: String,
        content: InspectorContent,
    },
    Answered(String),
    Form(FormMsg),
    /// The migration's result (`Err` is the reason; the worker reconnects itself after an `Ok`).
    Enabled(u64, Result<(), String>),
}

pub struct Link {
    tx: Sender<Req>,
    out: UnboundedSender<Msg>,
    env: Env,
}

impl Link {
    /// Starts the worker; it checks the setting and the platform first, then connects.
    pub fn start(env: Env) -> (Link, UnboundedReceiver<Msg>) {
        let (tx, rx) = channel();
        let (out, orx) = unbounded();
        let link = Link {
            tx,
            out: out.clone(),
            env: env.clone(),
        };
        std::thread::spawn(move || run(&env, &rx, &out));
        (link, orx)
    }

    /// Reads `HEAD` of the folder `ws` on its own thread (git, never on the window's) for the form.
    pub fn head(&self, ws: String) {
        let (out, env) = (self.out.clone(), self.env.clone());
        std::thread::spawn(move || {
            let result = synthesis_view::project::head_commit(std::path::Path::new(&ws), &env);
            let _ = out.unbounded_send(Msg::Form(FormMsg::Head { ws, result }));
        });
    }

    pub fn send(&self, r: Req) {
        // a worker that ended has said why; nothing more to do
        let _ = self.try_send(r);
    }

    /// `false`: the worker has ended (it has said why), so the request went nowhere.
    pub fn try_send(&self, r: Req) -> bool {
        self.tx.send(r).is_ok()
    }
}

/// `synt` next to this app (a packaged install): the orchestrator is started with it. Without one the app only
/// connects to a running orchestrator (it never starts itself, which would open a second window).
fn synt_exe() -> Option<std::path::PathBuf> {
    match synt_beside(&std::env::current_exe().ok()?) {
        Sibling::Usable(p) => Some(p),
        _ => None,
    }
}

/// What sits beside the app executable under the name `synt`.
#[derive(Debug, PartialEq)]
enum Sibling {
    Usable(std::path::PathBuf),
    /// There is one, but it is not what the app may start (not a regular file of the user's, or writable by others).
    Refused,
    Absent,
}

/// The `synt` in the folder `exe` really is in: the path is resolved first, so a symlink to the app from a
/// folder the user can write does not pick a `synt` that sits beside the link (inside `Synthesis.app` that is
/// `Contents/MacOS`). A regular file, never a symlink, never a PATH search, and only one `usable_synt` accepts.
fn synt_beside(exe: &std::path::Path) -> Sibling {
    let Some(dir) = std::fs::canonicalize(exe)
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    else {
        return Sibling::Absent;
    };
    let p = dir.join(if cfg!(windows) { "synt.exe" } else { "synt" });
    match std::fs::symlink_metadata(&p) {
        Err(_) => Sibling::Absent,
        Ok(m) if usable_synt(&dir, &m) => Sibling::Usable(p),
        Ok(_) => Sibling::Refused,
    }
}

/// A regular file (a symlink is not one). On unix also: the user's own, executable, and neither it nor its
/// folder, nor any folder above it up to the `.app` bundle (or just the folder, outside a bundle), writable
/// by group or others; the folder belongs to the user or root. So nobody else can have put it there or
/// swapped it.
fn usable_synt(dir: &std::path::Path, file: &std::fs::Metadata) -> bool {
    if !file.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let me = rustix::process::geteuid().as_raw();
        let Ok(d) = std::fs::metadata(dir) else {
            return false;
        };
        if file.uid() != me || file.mode() & 0o111 == 0 || file.mode() & 0o022 != 0 {
            return false;
        }
        if ![me, 0].contains(&d.uid()) {
            return false;
        }
        let in_bundle = dir
            .ancestors()
            .any(|a| a.extension().is_some_and(|e| e == "app"));
        for a in dir.ancestors() {
            match std::fs::metadata(a) {
                Ok(m) if m.mode() & 0o022 == 0 => {}
                _ => return false,
            }
            // the bundle root ends the walk; outside a bundle only the folder itself counts
            if !in_bundle || a.extension().is_some_and(|e| e == "app") {
                break;
            }
        }
        true
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        true
    }
}

/// The synchronous check before anything starts: the platform supports outcomes and `synt.orchestrator.enabled`
/// is true. `Ok` is the synt home; `Err` is the screen that says why not (no thread, socket or poll is made).
pub fn gate(env: &Env) -> Result<std::path::PathBuf, Screen> {
    if cfg!(windows) {
        return Err(Screen::Windows);
    }
    let h = synthesis_app::homes(env).map_err(Screen::Unreachable)?;
    let st = synthesis_app::outcome_service::settings_of(env, &h);
    if !synthesis_app::outcome_policy::outcome_policy(&st.synthesis.orchestrator).enabled {
        return Err(Screen::Off);
    }
    Ok(h.synthesis)
}

fn connect(env: &Env) -> Result<synthesis_app::orch_endpoint::LocalSocket, Screen> {
    let home = gate(env)?;
    let link = connect_started(&home);
    link.map_err(|e| {
        let hint = match std::env::current_exe().ok().map(|e| synt_beside(&e)) {
            Some(Sibling::Usable(_)) => "",
            Some(Sibling::Refused) => {
                " The synt next to the app is not a regular executable file owned by you, or it or its folder is writable by others: reinstall the app."
            }
            _ => " Start it with `synt outcome list` in a terminal, then press r.",
        };
        Screen::Unreachable(format!("{e}.{hint}"))
    })
}

/// Connects (with push); when none is running and a `synt` is at hand, starts the orchestrator and waits for it.
fn connect_started(
    home: &std::path::Path,
) -> Result<synthesis_app::orch_endpoint::LocalSocket, ClientError> {
    use synthesis_app::orch_endpoint::{LocalSocket, spawn_detached};
    let first = LocalSocket::connect_with(home, Surface::Superagent, true);
    let Some(exe) = synt_exe().filter(|_| first.is_err()) else {
        return first;
    };
    spawn_detached(&exe, home).map_err(|e| ClientError::Io(e.to_string()))?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(s) = LocalSocket::connect_with(home, Surface::Superagent, true) {
            return Ok(s);
        }
    }
    first
}

fn run(env: &Env, rx: &Receiver<Req>, out: &UnboundedSender<Msg>) {
    loop {
        match connect(env) {
            Ok(link) => {
                let mut c = OutcomeClient::new(link, Surface::Superagent);
                let screen = match c.status() {
                    Ok(s) if s.get("ready") == Some(&Value::Bool(true)) => {
                        match serve(&mut c, rx, out, HINT_WAIT, &|ws| {
                            synthesis_view::project::head_commit(std::path::Path::new(ws), env)
                        }) {
                            Ok(()) => return,
                            Err(e) => Screen::Unreachable(e),
                        }
                    }
                    Ok(_) => {
                        let _ = out.unbounded_send(Msg::Screen(Screen::NotReady(
                            "the orchestrator is on but the session store is not migrated".into(),
                        )));
                        // the one request this state serves is the person's explicit migration
                        if !await_enable(&mut c, rx, out) {
                            return;
                        }
                        continue;
                    }
                    Err(e) => Screen::Unreachable(e.to_string()),
                };
                let _ = out.unbounded_send(Msg::Screen(screen));
            }
            Err(s) => {
                let _ = out.unbounded_send(Msg::Screen(s));
            }
        }
        // wait for a retry; a closed window ends the worker
        loop {
            match rx.recv() {
                Ok(Req::Retry) => break,
                // not connected: a migration request is answered, never left waiting
                Ok(Req::Enable(n)) => {
                    let _ = out.unbounded_send(Msg::Enabled(
                        n,
                        Err("not connected to the orchestrator; press r to check again".into()),
                    ));
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }
}

/// The store is not migrated: waits for `Retry` (`true`: connect again) or the person's `Enable` (the
/// migration; `true` after it worked, so the worker connects to the migrated store). A migration whose
/// reply is not this build's disclosure is refused (the client says so) and reported; the worker then keeps
/// waiting. `false`: the window closed.
fn await_enable<T: Transport>(
    c: &mut OutcomeClient<T>,
    rx: &Receiver<Req>,
    out: &UnboundedSender<Msg>,
) -> bool {
    loop {
        match rx.recv() {
            Ok(Req::Retry) => return true,
            Ok(Req::Enable(n)) => match c.enable() {
                Ok(_) => {
                    let _ = out.unbounded_send(Msg::Enabled(n, Ok(())));
                    return true;
                }
                Err(e) => {
                    let _ = out.unbounded_send(Msg::Enabled(n, Err(e.to_string())));
                    if matches!(e, ClientError::Io(_)) {
                        return true;
                    }
                }
            },
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// What the worker looks at: the selected outcome and the inspector tab.
struct Watch {
    selected: Option<String>,
    tab: InspectorTab,
}

/// The loop over one live client. `Ok` when the window closed; `Err` is the text of a failed link.
pub(super) fn serve<T: Transport>(
    c: &mut OutcomeClient<T>,
    rx: &Receiver<Req>,
    out: &UnboundedSender<Msg>,
    wait: Duration,
    head: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let mut w = Watch {
        selected: None,
        tab: InspectorTab::Overview,
    };
    refresh(c, &mut w, out)?;
    let mut last = Instant::now();
    // a hint came while the last refetch was too recent: look once the interval is over
    let mut dirty = false;
    loop {
        loop {
            match rx.try_recv() {
                Ok(Req::Retry) => {}
                Ok(Req::Refresh) => {
                    refresh(c, &mut w, out)?;
                    last = Instant::now();
                }
                Ok(Req::Select(id)) => {
                    w.selected = Some(id);
                    refresh(c, &mut w, out)?;
                    last = Instant::now();
                }
                Ok(Req::Inspect(tab)) => {
                    w.tab = tab;
                    refresh(c, &mut w, out)?;
                    last = Instant::now();
                }
                Ok(Req::Resolve {
                    outcome,
                    card,
                    choice,
                }) => {
                    let note = answer(c, &outcome, &card, &choice)?;
                    let _ = out.unbounded_send(Msg::Answered(note));
                    refresh(c, &mut w, out)?;
                    last = Instant::now();
                }
                Ok(Req::Submit(s)) => {
                    submit(c, s, &mut w, out, head)?;
                    refresh(c, &mut w, out)?;
                    last = Instant::now();
                }
                Ok(Req::Approve {
                    outcome,
                    version,
                    hash,
                    key,
                }) => {
                    approve(c, &outcome, version, &hash, &key, out)?;
                    refresh(c, &mut w, out)?;
                    last = Instant::now();
                }
                Ok(Req::Review(id)) => {
                    let m = review_draft(c, &id)?;
                    let _ = out.unbounded_send(Msg::Form(m));
                }
                // the store is migrated already (the person's confirm came late): say so, look again
                Ok(Req::Enable(n)) => {
                    let _ = out.unbounded_send(Msg::Enabled(
                        n,
                        Err("already migrated; checking again".into()),
                    ));
                    refresh(c, &mut w, out)?;
                    last = Instant::now();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        dirty |= c.next_hint(wait).map_err(|e| e.to_string())?.is_some();
        if (dirty && last.elapsed() >= MIN_REFETCH) || last.elapsed() >= POLL {
            dirty = false;
            refresh(c, &mut w, out)?;
            last = Instant::now();
        }
    }
}

/// A failed link or a server that broke the protocol ends the worker; any other error is only that call's.
fn soft<T>(r: Result<T, ClientError>) -> Result<Result<T, String>, String> {
    match r {
        Ok(v) => Ok(Ok(v)),
        Err(e @ (ClientError::Io(_) | ClientError::Hostile(_))) => Err(e.to_string()),
        Err(e) => Ok(Err(e.to_string())),
    }
}

fn refresh<T: Transport>(
    c: &mut OutcomeClient<T>,
    w: &mut Watch,
    out: &UnboundedSender<Msg>,
) -> Result<(), String> {
    let rows = c.outcomes(None).map_err(|e| e.to_string())?;
    let mut items = vec![];
    for (i, row) in rows.into_iter().enumerate() {
        let budget = if i < MAX_BUDGETS {
            soft(c.budget(&row.id))?
                .map(|v| BudgetView::from_json(&v))
                .unwrap_or_default()
        } else {
            BudgetView::default()
        };
        items.push(Item { row, budget });
    }
    if !items.iter().any(|i| Some(&i.row.id) == w.selected.as_ref()) {
        w.selected = items.first().map(|i| i.row.id.clone());
    }
    let (mut detail, mut inspector) = (None, None);
    if let Some(item) = items
        .iter()
        .find(|i| Some(&i.row.id) == w.selected.as_ref())
    {
        let id = item.row.id.clone();
        let contract = soft(c.contract(&id, None))?;
        let cards = soft(c.decisions(&id))?;
        let events = events(c, &id)?;
        let history = match &events {
            Ok((evs, next)) => InspectorContent::activity(evs, *next),
            Err(e) => InspectorContent::unavailable(InspectorTab::Activity, e),
        };
        inspector = Some(tab_content(c, w.tab, item, &contract, &events)?);
        let current = contract
            .as_ref()
            .ok()
            .and_then(|v| serde_json::from_value(v.get("contract")?.clone()).ok());
        detail = Some(Box::new(Detail {
            outcome: id,
            cards,
            current,
            contract: contract.and_then(|v| CriteriaView::from_json(&v)),
            history,
        }));
    }
    let _ = out.unbounded_send(Msg::Snapshot {
        items,
        selected: w.selected.clone(),
        detail,
        inspector,
    });
    Ok(())
}

/// The events of the outcome, page by page (the server checks the order and the cursor).
fn events<T: Transport>(
    c: &mut OutcomeClient<T>,
    id: &str,
) -> Result<Result<(Vec<Value>, u64), String>, String> {
    let (mut all, mut after) = (vec![], 0);
    for _ in 0..MAX_PAGES {
        match soft(c.events(id, after))? {
            Ok((page, next)) => {
                let more = page.len() >= 200;
                all.extend(page);
                after = next;
                if !more {
                    break;
                }
            }
            Err(e) => return Ok(Err(e)),
        }
    }
    Ok(Ok((all, after)))
}

/// The content of one inspector tab. Only a failed link is an `Err`; a refused or unreadable reply is
/// `Unavailable` with the reason (a tab never shows an empty page in place of an error).
fn tab_content<T: Transport>(
    c: &mut OutcomeClient<T>,
    tab: InspectorTab,
    item: &Item,
    contract: &Result<Value, String>,
    events: &Result<(Vec<Value>, u64), String>,
) -> Result<InspectorContent, String> {
    let id = &item.row.id;
    let r: Result<InspectorContent, String> = match tab {
        InspectorTab::Overview => contract.clone().and_then(|v| {
            Ok(InspectorContent::Overview(Box::new(OverviewTab {
                outcome: item.row.clone(),
                criteria: CriteriaView::from_json(&v)?,
                // the server's own numbers; the contract's limits only where it sent none
                budget: if item.budget == BudgetView::default() {
                    BudgetView::from_contract(&v)
                } else {
                    item.budget
                },
            })))
        }),
        InspectorTab::Workers => workers(c, id)?,
        InspectorTab::Activity => match events {
            Ok((evs, next)) => Ok(InspectorContent::activity(
                &evs[evs.len().saturating_sub(ACTIVITY_TAIL)..],
                *next,
            )),
            Err(e) => Err(e.clone()),
        },
        InspectorTab::Evidence => {
            soft(c.evidence(id))?.and_then(|rows| InspectorContent::evidence(&rows))
        }
        InspectorTab::Diff => diff(c, id)?,
    };
    Ok(InspectorContent::or_unavailable(tab, r))
}

fn plan_and_items<T: Transport>(
    c: &mut OutcomeClient<T>,
    id: &str,
) -> Result<Result<(Value, Vec<Value>), String>, String> {
    let plan = match soft(c.plan(id))? {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let mut items = vec![];
    for wid in plan
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|i| i.get("id")?.as_str())
    {
        match soft(c.work_item(id, wid))? {
            Ok(v) => items.push(v),
            Err(e) => return Ok(Err(e)),
        }
    }
    Ok(Ok((plan, items)))
}

fn workers<T: Transport>(
    c: &mut OutcomeClient<T>,
    id: &str,
) -> Result<Result<InspectorContent, String>, String> {
    Ok(plan_and_items(c, id)?.and_then(|(p, w)| InspectorContent::workers(&p, &w)))
}

/// The newest attempt that has a candidate commit, through `attempt.diff` (the server reads the repository).
fn diff<T: Transport>(
    c: &mut OutcomeClient<T>,
    id: &str,
) -> Result<Result<InspectorContent, String>, String> {
    let (_, items) = match plan_and_items(c, id)? {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let mut newest = None;
    for w in &items {
        match AttemptView::list_from_work_item(w) {
            Ok(a) => {
                newest = a
                    .into_iter()
                    .rfind(|a| !a.subject_commit.is_empty())
                    .or(newest)
            }
            Err(e) => return Ok(Err(e)),
        }
    }
    let Some(a) = newest else {
        return Ok(Ok(InspectorContent::diff()));
    };
    Ok(soft(c.diff(id, &a.id))?.and_then(|d| InspectorContent::diff_from(&d)))
}

/// Answers `card` with `choice` if the server still has that very brief pending, and follows a queued
/// command. The text is what the person is told. The card is looked at again first: the revision and brief
/// hash sent are the ones of the card that was shown, and a card that moved is not answered (the server would
/// refuse it too: this only says so before sending).
fn answer<T: Transport>(
    c: &mut OutcomeClient<T>,
    outcome: &str,
    card: &DecisionCard,
    choice: &str,
) -> Result<String, String> {
    let now = match soft(c.decisions(outcome))? {
        Ok(v) => v.into_iter().find(|d| d.id == card.id),
        Err(e) => return Ok(format!("Not sent: {e}")),
    };
    let same = now.as_ref().is_some_and(|n| {
        n.revision == card.revision && n.brief_sha256 == card.brief_sha256 && n.resolvable()
    });
    if !same {
        return Ok("This decision changed since you saw it. Nothing was sent; the current one is shown. Read it, then choose again.".into());
    }
    let key = uuid::Uuid::new_v4().to_string();
    let mut r = match soft(c.resolve(outcome, card, choice, &key, None))? {
        Ok(r) => r,
        Err(e) => return Ok(format!("Not sent: {e}")),
    };
    let end = Instant::now() + SETTLE;
    while r.state == CommandState::Queued && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(150));
        r = match soft(c.command_status(outcome, &key))? {
            Ok(r) => r,
            Err(e) => return Ok(format!("Sent; the state could not be read: {e}")),
        };
    }
    Ok(format!("Answer {choice}: {}", r.describe()))
}

/// Tells the form that the request failed. A failed link or a server that broke the protocol also ends the
/// worker (the window shows the screen for it; the form stays open with its text and the same key).
fn form_fail(out: &UnboundedSender<Msg>, e: &ClientError) -> Result<(), String> {
    let _ = out.unbounded_send(Msg::Form(FormMsg::Failed {
        text: e.to_string(),
        spent: matches!(e, ClientError::Refused(_)),
    }));
    match e {
        ClientError::Io(_) | ClientError::Hostile(_) => Err(e.to_string()),
        _ => Ok(()),
    }
}

pub const STALE: &str = "The outcome changed since you opened this form, so nothing was revised. Your text is kept here: close the form, reload (r), read the outcome again, then revise it again.";

/// Follows a queued command for up to `SETTLE`.
fn settle<T: Transport>(
    c: &mut OutcomeClient<T>,
    outcome: &str,
    mut r: CommandReply,
    key: &str,
) -> Result<Result<CommandReply, String>, String> {
    let end = Instant::now() + SETTLE;
    while r.state == CommandState::Queued && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(150));
        r = match soft(c.command_status(outcome, key))? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
    }
    Ok(Ok(r))
}

/// The contract the server stored as version `version`, with the server's own diff when there is an earlier
/// contract to compare it with. What cannot be read, or is not the version and hash reported, is an error in
/// the review and blocks the approval.
fn review<T: Transport>(
    c: &mut OutcomeClient<T>,
    outcome: &str,
    version: u32,
    hash: &str,
) -> Result<Review, String> {
    let contract = soft(c.contract(outcome, Some(version)))?.and_then(|v| {
        let cv = CriteriaView::from_json(&v)?;
        if cv.hash == hash && cv.version == Some(i64::from(version)) {
            Ok(cv)
        } else {
            Err("the stored contract is not the version and hash the orchestrator reported, so it cannot be approved here".into())
        }
    });
    // every version above the first is read against the approved one, whatever opened the review
    let diff = if version > 1 {
        Some(
            soft(c.contract_diff(outcome, version))?
                .and_then(|v| DiffView::from_json(&v, version, hash)),
        )
    } else {
        None
    };
    Ok(Review {
        outcome: outcome.to_string(),
        version,
        hash: hash.to_string(),
        contract,
        diff,
        key: format!("approve-{}", uuid::Uuid::new_v4()),
    })
}

fn to_review<T: Transport>(
    c: &mut OutcomeClient<T>,
    id: &str,
    version: Option<u64>,
    hash: Option<&str>,
) -> Result<FormMsg, String> {
    match (version.and_then(|v| u32::try_from(v).ok()), hash) {
        (Some(v), Some(h)) if valid_alt_id(id) && is_hex(h, 64) => {
            Ok(FormMsg::Review(Box::new(review(c, id, v, h)?)))
        }
        _ => Ok(FormMsg::Failed {
            text: "The orchestrator sent an outcome id, version or contract hash of an unexpected shape; nothing is shown or approved.".into(),
            spent: false,
        }),
    }
}

/// Creates or revises. The contract is stored and waits: nothing is approved here.
fn submit<T: Transport>(
    c: &mut OutcomeClient<T>,
    s: Submit,
    w: &mut Watch,
    out: &UnboundedSender<Msg>,
    head: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let m = match s {
        Submit::Create {
            workspace,
            base_commit,
            contract,
            key,
        } => match head(&workspace) {
            // HEAD is read again now: a base that moved since the form showed it is not used
            Ok(now) if now != base_commit => {
                let text = format!(
                    "The base commit moved while you were editing: HEAD is now {now} (the form showed {base_commit}). Nothing was created. Check it, then confirm again to use the new base."
                );
                let _ = out.unbounded_send(Msg::Form(FormMsg::BaseMoved {
                    ws: workspace,
                    commit: now,
                    text,
                }));
                return Ok(());
            }
            Err(why) => {
                let _ = out.unbounded_send(Msg::Form(FormMsg::Failed {
                    text: format!("Base commit: {why} Nothing was created."),
                    spent: false,
                }));
                return Ok(());
            }
            Ok(_) => match c.create_with_key(&workspace, &base_commit, *contract, &key) {
            Ok(v) => {
                let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
                // the new outcome is the one shown next
                w.selected = Some(id.to_string());
                to_review(
                    c,
                    id,
                    v.get("contract_version").and_then(Value::as_u64),
                    v.get("contract_hash").and_then(Value::as_str),
                )?
            }
                Err(e) => return form_fail(out, &e),
            },
        },
        Submit::Revise {
            outcome,
            revision,
            contract,
            key,
        } => match c.revise_contract(&outcome, revision, *contract, &key) {
            Ok(r) => match settle(c, &outcome, r, &key)? {
                Err(e) => FormMsg::Failed {
                    text: format!("Sent; the state could not be read: {e}"),
                    spent: false,
                },
                Ok(r) => match r.state {
                    CommandState::Applied => to_review(
                        c,
                        &outcome,
                        r.result.get("version").and_then(Value::as_u64),
                        r.result.get("hash").and_then(Value::as_str),
                    )?,
                    CommandState::Stale => FormMsg::Failed {
                        text: STALE.into(),
                        spent: true,
                    },
                    CommandState::Queued => FormMsg::Failed {
                        text: "The revision is queued and not applied yet. Confirm again to look again: the same request is not repeated.".into(),
                        spent: false,
                    },
                    CommandState::Denied | CommandState::AlreadyResolved => FormMsg::Failed {
                        text: format!(
                            "The orchestrator refused the revision ({}). A contract can be revised only while the outcome is blocked with no live attempt.",
                            r.describe()
                        ),
                        spent: true,
                    },
                },
            },
            Err(e) => return form_fail(out, &e),
        },
    };
    let _ = out.unbounded_send(Msg::Form(m));
    Ok(())
}

/// Approves the version and hash of the review (the ones the server reported, never others).
fn approve<T: Transport>(
    c: &mut OutcomeClient<T>,
    outcome: &str,
    version: u32,
    hash: &str,
    key: &str,
    out: &UnboundedSender<Msg>,
) -> Result<(), String> {
    let r = match c.approve_contract(outcome, version, hash, key) {
        Ok(r) => r,
        Err(e) => return form_fail(out, &e),
    };
    let m = match settle(c, outcome, r, key)? {
        Err(e) => FormMsg::Failed {
            text: format!("Sent; the state could not be read: {e}"),
            spent: false,
        },
        Ok(r) if r.state == CommandState::Applied => {
            FormMsg::Approved(format!("Contract v{version}: {}", r.describe()))
        }
        // not applied yet: the contract is still not approved, and the same key looks again
        Ok(r) if r.state == CommandState::Queued => FormMsg::Failed {
            text: format!(
                "The approval of version {version} is {}. It is not approved yet: confirm again to look again (the same request is not repeated), or close this window and watch the outcome.",
                r.describe()
            ),
            spent: false,
        },
        Ok(r) => FormMsg::Failed {
            text: format!("Not approved: {}", r.describe()),
            spent: true,
        },
    };
    let _ = out.unbounded_send(Msg::Form(m));
    Ok(())
}

/// The stored contract of an outcome that is waiting for its first approval.
fn review_draft<T: Transport>(c: &mut OutcomeClient<T>, id: &str) -> Result<FormMsg, String> {
    let fail = |text: String| FormMsg::Failed { text, spent: false };
    Ok(match soft(c.contract(id, None))? {
        Err(e) => fail(e),
        Ok(v) if v.get("approved") == Some(&Value::Bool(false)) => to_review(
            c,
            id,
            v.get("version").and_then(Value::as_u64),
            v.get("hash").and_then(Value::as_str),
        )?,
        Ok(_) => fail("This contract is already approved.".into()),
    })
}

#[cfg(test)]
mod tests;
