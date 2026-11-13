//! Verification engine (docs/specs/verification.md): check planning, execution through
//! `synthesis-terminal`, status aggregation, completion gate and bounded self-correction.

mod baseline;
mod gate;
mod plan;
mod runner;
mod testout;
mod types;
mod weaken;

pub use baseline::Baseline;
pub use gate::*;
pub use plan::{detect_checks, is_verification_command, plan_checks};
pub use runner::{CheckAuth, CheckAuthorizer, NEEDS_APPROVAL, VerifyRunner};
pub use testout::parse_counts;
pub use types::*;
pub use weaken::{FileEdit, removed_prerequisite_guards, weakened_tests};
