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
    AttemptView, BudgetView, CriteriaView, DecisionCard, InspectorContent, InspectorTab,
    OverviewTab,
};

use super::model::{Detail, Item, Screen};

/// How long without a hint before the worker looks again.
const POLL: Duration = Duration::from_secs(20);
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
}

pub struct Link {
    tx: Sender<Req>,
}

impl Link {
    /// Starts the worker; it checks the setting and the platform first, then connects.
    pub fn start(env: Env) -> (Link, UnboundedReceiver<Msg>) {
        let (tx, rx) = channel();
        let (out, orx) = unbounded();
        std::thread::spawn(move || run(&env, &rx, &out));
        (Link { tx }, orx)
    }

    pub fn send(&self, r: Req) {
        // a worker that ended has said why; nothing more to do
        let _ = self.tx.send(r);
    }
}

/// `synt` next to this app (a packaged install): the orchestrator is started with it. Without one the app only
/// connects to a running orchestrator (it never starts itself, which would open a second window).
fn synt_exe() -> Option<std::path::PathBuf> {
    let p = std::env::current_exe()
        .ok()?
        .parent()?
        .join(if cfg!(windows) { "synt.exe" } else { "synt" });
    p.is_file().then_some(p)
}

fn connect(env: &Env) -> Result<synthesis_app::orch_endpoint::LocalSocket, Screen> {
    if cfg!(windows) {
        return Err(Screen::Windows);
    }
    let h = synthesis_app::homes(env).map_err(Screen::Unreachable)?;
    let st = synthesis_app::outcome_service::settings_of(env, &h);
    if !synthesis_app::outcome_policy::outcome_policy(&st.synthesis.orchestrator).enabled {
        return Err(Screen::Off);
    }
    let link = connect_started(&h.synthesis);
    link.map_err(|e| {
        let hint = if synt_exe().is_none() {
            " Start it with `synt outcome list` in a terminal, then press r."
        } else {
            ""
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
                        match serve(&mut c, rx, out, HINT_WAIT) {
                            Ok(()) => return,
                            Err(e) => Screen::Unreachable(e),
                        }
                    }
                    Ok(_) => Screen::NotReady(
                        "the orchestrator is on but the store is not migrated: run `synt outcome enable` in a terminal, then press r".into(),
                    ),
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
                Ok(_) => {}
                Err(_) => return,
            }
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
) -> Result<(), String> {
    let mut w = Watch {
        selected: None,
        tab: InspectorTab::Overview,
    };
    refresh(c, &mut w, out)?;
    let mut last = Instant::now();
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
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        let hint = c.next_hint(wait).map_err(|e| e.to_string())?;
        if hint.is_some() || last.elapsed() >= POLL {
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
        detail = Some(Box::new(Detail {
            outcome: id,
            cards,
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

#[cfg(test)]
mod tests;
