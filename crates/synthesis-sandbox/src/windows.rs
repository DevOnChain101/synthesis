//! Windows: capability reporting only.
//!
//! Gap (honest): no filesystem or network confinement. Planned: AppContainer or a restricted
//! token (e.g. via `rappct`), then a Job Object with memory / active-process limits.
//! The workspace forbids `unsafe`, so raw Job Object calls are not made here; tree kill is
//! already done by `synthesis-terminal` through `process-wrap`'s `job-object` feature.
//! Not run on Windows in this environment.

use crate::*;

pub struct WindowsJobBackend;

impl SandboxBackend for WindowsJobBackend {
    fn name(&self) -> &'static str {
        "windows-job"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: Guarantee::None,
            network: Guarantee::None,
            process_tree: Guarantee::Weak,
            gui: Guarantee::None,
            notes: vec![
                "no filesystem/network confinement (AppContainer/restricted token not implemented)".into(),
                "tree kill comes from the terminal runtime's Job Object; memory and process-count limits are not implemented".into(),
            ],
        }
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        NullSandbox.wrap(policy, c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_no_fs_or_net() {
        let c = WindowsJobBackend.capabilities();
        assert_eq!(
            (c.filesystem, c.network),
            (Guarantee::None, Guarantee::None)
        );
    }
}
