//! Approval and question dialogs of a native front end (the desktop window, the `synt engine`
//! server): the harness asks, the front end is told through [`Notify`], the answer arrives by id.
//! An ask nobody can answer is denied, never granted; cancelling the task withdraws it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use synthesis_harness::{Approval, ApprovalRequest, Approver, BoxFut, InputProvider, UserReply};
use synthesis_permissions::{Mode, Suggestion};
use synthesis_tools::NeedsUser;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::host::lock;

/// What the front end hears of a dialog.
pub enum Ask {
    /// A permission ask; answer it with [`Asks::answer`].
    Approval(Box<AskApproval>),
    /// A plan or question dialog; answer it with [`Asks::answer_input`].
    Input(Box<AskInput>),
    /// The ask with this id is gone (cancelled, expired or answered).
    Withdraw(String),
}

/// Delivers an [`Ask`]; `false`: nobody is listening any more.
pub type Notify = Arc<dyn Fn(Ask) -> bool + Send + Sync>;

/// What the dialog shows of an [`ApprovalRequest`]. `suggestions` is the list the person can accept:
/// "allow always" never switches into bypassPermissions (that takes its own confirmation).
#[derive(Debug, Clone)]
pub struct AskApproval {
    pub id: String,
    pub agent_id: Option<String>,
    pub tool: String,
    pub input: Value,
    /// `permission` or `outside_read`.
    pub dialog: &'static str,
    pub prompt: String,
    pub reason: String,
    pub origin: Option<String>,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Clone)]
pub struct AskInput {
    pub id: String,
    pub req: NeedsUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Once,
    Always,
    Deny,
}

const DENY_MESSAGE: &str = "The user denied this tool call. Do not retry it unchanged.";

/// Allow once carries no rule; Always carries the rules the dialog offered; anything else denies.
pub fn approval_for(choice: Choice, offered: &[Suggestion]) -> Approval {
    match choice {
        Choice::Once => Approval::allow(),
        Choice::Always => Approval {
            accepted: offered.to_vec(),
            ..Approval::allow()
        },
        Choice::Deny => Approval::deny(DENY_MESSAGE),
    }
}

struct Pending {
    reply: oneshot::Sender<Approval>,
    offered: Vec<Suggestion>,
}

type PendingMap = Arc<Mutex<HashMap<String, Pending>>>;
type InputMap = Arc<Mutex<HashMap<String, oneshot::Sender<Option<UserReply>>>>>;

/// The dialogs waiting for an answer.
#[derive(Default, Clone)]
pub struct Asks {
    pending: PendingMap,
    inputs: InputMap,
}

impl Asks {
    pub fn approver(&self, notify: Notify) -> Arc<dyn Approver> {
        Arc::new(ChanApprover {
            pending: self.pending.clone(),
            notify,
        })
    }

    pub fn input(&self, notify: Notify) -> Arc<dyn InputProvider> {
        Arc::new(ChanInput {
            pending: self.inputs.clone(),
            notify,
        })
    }

    /// No dialog waits for an answer.
    pub fn is_idle(&self) -> bool {
        lock(&self.pending).is_empty() && lock(&self.inputs).is_empty()
    }

    /// Answers the approval `id`; false when it is no longer waiting.
    pub fn answer(&self, id: &str, choice: Choice) -> bool {
        let Some(p) = lock(&self.pending).remove(id) else {
            return false;
        };
        p.reply.send(approval_for(choice, &p.offered)).is_ok()
    }

    /// Answers the plan or question `id` (`None`: nobody answers); false when it is gone.
    pub fn answer_input(&self, id: &str, reply: Option<UserReply>) -> bool {
        let Some(r) = lock(&self.inputs).remove(id) else {
            return false;
        };
        r.send(reply).is_ok()
    }
}

/// Tells the front end to drop a dialog when its future is dropped (cancel or answered).
struct Withdraw {
    notify: Notify,
    id: String,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        (self.notify)(Ask::Withdraw(std::mem::take(&mut self.id)));
    }
}

struct ChanApprover {
    pending: PendingMap,
    notify: Notify,
}

impl Approver for ChanApprover {
    fn approve<'a>(
        &'a self,
        req: ApprovalRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Approval> {
        Box::pin(async move {
            let id = req.approval_id.clone();
            let offered: Vec<Suggestion> = req
                .suggestions
                .into_iter()
                .filter(|s| !matches!(s, Suggestion::SetMode(Mode::BypassPermissions)))
                .collect();
            let (reply, answer) = oneshot::channel();
            // registered before the front end hears of it, so an answer can never land first
            lock(&self.pending).insert(
                id.clone(),
                Pending {
                    reply,
                    offered: offered.clone(),
                },
            );
            let ask = AskApproval {
                id: id.clone(),
                agent_id: req.agent_id,
                tool: req.tool,
                input: req.input,
                dialog: req.dialog,
                prompt: req.prompt,
                reason: req.reason,
                origin: req.origin,
                suggestions: offered,
            };
            if !(self.notify)(Ask::Approval(Box::new(ask))) {
                lock(&self.pending).remove(&id);
                return Approval::auto_denied(); // the window is gone: nobody can approve
            }
            let _withdraw = Withdraw {
                notify: self.notify.clone(),
                id: id.clone(),
            };
            let r = tokio::select! {
                a = answer => a.unwrap_or_else(|_| Approval::auto_denied()),
                () = cancel.cancelled() => Approval {
                    reviewer: "policy",
                    ..Approval::deny("Cancelled before the approval was answered.")
                },
            };
            lock(&self.pending).remove(&id);
            r
        })
    }
}

/// Plan and question dialogs (`ExitPlanMode`, `AskUserQuestion`).
struct ChanInput {
    pending: InputMap,
    notify: Notify,
}

impl InputProvider for ChanInput {
    fn ask<'a>(
        &'a self,
        req: &'a NeedsUser,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>> {
        Box::pin(async move {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(1);
            let id = format!("ask-{}", N.fetch_add(1, Ordering::Relaxed));
            let (reply, answer) = oneshot::channel();
            lock(&self.pending).insert(id.clone(), reply);
            let ask = AskInput {
                id: id.clone(),
                req: req.clone(),
            };
            if !(self.notify)(Ask::Input(Box::new(ask))) {
                lock(&self.pending).remove(&id);
                return None;
            }
            let _withdraw = Withdraw {
                notify: self.notify.clone(),
                id: id.clone(),
            };
            let r = tokio::select! {
                a = answer => a.unwrap_or(None),
                () = cancel.cancelled() => None,
            };
            lock(&self.pending).remove(&id);
            r
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use synthesis_harness::ApprovalDecision;
    use synthesis_permissions::RuleSpec;

    fn offered() -> Vec<Suggestion> {
        vec![Suggestion::AddRules {
            rules: vec![RuleSpec {
                tool_name: "Bash".into(),
                rule_content: Some("ls".into()),
            }],
            behavior: synthesis_permissions::Decision::Allow,
        }]
    }

    fn request(suggestions: Vec<Suggestion>, reason: &str) -> ApprovalRequest {
        ApprovalRequest {
            approval_id: "a".into(),
            agent_id: None,
            tool_call_id: "t".into(),
            tool: "Bash".into(),
            input: json!({}),
            dialog: "permission",
            prompt: String::new(),
            reason: reason.into(),
            suggestions,
            origin: None,
        }
    }

    #[test]
    fn choices_map_to_decisions_and_only_always_carries_rules() {
        let s = offered();
        let once = approval_for(Choice::Once, &s);
        assert_eq!(once.decision, ApprovalDecision::Allow);
        assert!(once.accepted.is_empty());
        let always = approval_for(Choice::Always, &s);
        assert_eq!(always.decision, ApprovalDecision::Allow);
        assert_eq!(always.accepted, s);
        let deny = approval_for(Choice::Deny, &s);
        assert_eq!(deny.decision, ApprovalDecision::Deny);
        assert!(deny.accepted.is_empty());
    }

    #[test]
    fn a_gone_window_denies_instead_of_hanging() {
        let gone: Notify = Arc::new(|_| false);
        let asks = Asks::default();
        let (ap, inp) = (asks.approver(gone.clone()), asks.input(gone));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let a = rt.block_on(ap.approve(request(vec![], ""), &cancel));
        assert_eq!(a.decision, ApprovalDecision::AutoDenied);
        let q = NeedsUser::ExitPlanMode { plan: "p".into() };
        assert!(rt.block_on(inp.ask(&q, &cancel)).is_none());
    }

    #[test]
    fn allow_always_never_offers_bypass() {
        let seen = Arc::new(Mutex::new(vec![]));
        let s2 = seen.clone();
        let notify: Notify = Arc::new(move |a| {
            lock(&s2).push(a);
            true
        });
        let ap = Asks::default().approver(notify);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        // cancelled right after the ask is out: the approver answers with a policy denial
        let req = request(
            vec![
                Suggestion::SetMode(Mode::BypassPermissions),
                offered().remove(0),
            ],
            "why",
        );
        let r = rt.block_on(async {
            let f = ap.approve(req, &cancel);
            tokio::pin!(f);
            tokio::select! {
                _ = &mut f => unreachable!("nobody answered"),
                () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            c2.cancel();
            f.await
        });
        assert_eq!(r.decision, ApprovalDecision::Deny);
        let seen = lock(&seen);
        let Some(Ask::Approval(a)) = seen.first() else {
            panic!("the ask reaches the window")
        };
        assert_eq!(a.suggestions, offered());
        assert_eq!(a.reason, "why");
        assert!(matches!(seen.get(1), Some(Ask::Withdraw(id)) if id == "a"));
    }
}
