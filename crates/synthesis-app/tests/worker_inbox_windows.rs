//! Native Windows refuses worker messages until a real Windows run (S1 Q5): the client says so before
//! anything is sent, and `orchestrator.status` does not list `attempt_message`. The rest of the inbox
//! tests are in `worker_inbox.rs` (not Windows).
#![cfg(windows)]

use synthesis_app::outcome_client::{ClientError, InProc, OutcomeClient};
use synthesis_app::outcome_service::Service;
use synthesis_app::proto::FEATURES;
use synthesis_session::outcome::Surface;

#[test]
fn windows_refuses_worker_messages_and_does_not_advertise_them() {
    let home = tempfile::tempdir().unwrap();
    let mut c = OutcomeClient::new(InProc(Service::new(home.path(), true)), Surface::Tui);
    let e = c
        .send_worker_message(
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            "hi",
            false,
            None,
            "k",
        )
        .unwrap_err();
    assert!(
        matches!(&e, ClientError::Refused(m) if m.contains("native Windows")),
        "{e}"
    );
    assert!(
        FEATURES.contains(&"attempt_message"),
        "the constant keeps it; the status filters it"
    );
    let st = c.status().unwrap();
    assert!(
        !st["features"].to_string().contains("attempt_message"),
        "{st}"
    );
}
