//! The person's click on an engaged card (or Enter in the worker composer): the only key to
//! [`crate::conductor::ProposalExecutor::execute`].

/// Proof that the person pressed the card's button just now. Built by the GPUI handler of an engaged card and
/// nowhere in the conductor modules (`tests/conductor_grep.rs`). Not `Clone`, so one click runs one proposal.
pub struct PersonClick(());

impl PersonClick {
    /// Call only from the card's click handler.
    pub fn from_card_click() -> Self {
        Self(())
    }
}
