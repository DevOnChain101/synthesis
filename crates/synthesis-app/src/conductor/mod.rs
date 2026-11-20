//! The conductor host (super-agent-s1.md 4.2-4.5, 4.7, 4.8; conductor.md C0-C3): the Rust policy behind the
//! orchestrator chat. The model sees seven `conductor_*` tools, all read-only by type: four read host-built
//! digests, three `propose_*` tools store a [`Proposal`] and return at once. A proposal is never an action. It
//! runs only through [`ProposalExecutor::execute`], which needs a [`PersonClick`], and `PersonClick` is built
//! only by the GPUI handler of an engaged card (P4), never by anything in this module (a grep test checks it).
//! No GPUI, no store, no engine here: the window supplies [`ConductorReads`] and [`Effects`].

mod click;
mod config;
mod digest;
mod exec;
mod host;
mod limits;
mod proposal;
mod reads;
mod tools;

pub use click::PersonClick;
pub use config::{Action, ConductorConfig, Confirm};
pub use exec::{Effects, GoalDraft, ProposalExecutor, SessionStart, Via};
pub use host::{ConductorHost, HostConfig};
pub use proposal::{Payload, Proposal, State};
pub use reads::{ConductorReads, ItemRow, OutcomeRow, SessionReport, SessionRow};
pub use tools::{TOOLS, specs};

/// `message_worker` (a card that messages an outcome worker) ships off until the live worker inbox (P2c) lands.
/// While false, `conductor_propose_assignment` refuses a work-item target and the auto list refuses the name.
pub const MESSAGE_WORKER_ENABLED: bool = false;
