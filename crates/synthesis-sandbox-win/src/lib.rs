//! Windows AppContainer launcher (docs/specs/sandbox-hardening.md section 5).
//!
//! ADR-010 exception (owner decision 2026-11-15): this is the ONLY crate in the workspace that
//! contains `unsafe`, because an AppContainer process can be created only through Win32 calls
//! (`CreateAppContainerProfile`, `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`,
//! DACL edits, Job Objects) and no safe wrapper crate is maintained for them. The core stays
//! Rust; the `unsafe` blocks are small, each has a `SAFETY:` note, and they all live in `imp.rs`.
//! The wire types below build and serialise on every OS so the callers' logic is unit-tested
//! everywhere; only [`run`] and [`probe`] are Windows-only.
//!
//! What [`run`] does, in order: creates a throw-away AppContainer profile (unique name, so a
//! unique SID), grants that SID access on the listed paths (explicit ACEs, removed again at exit),
//! creates a Job Object (kill on close, process and memory limits, UI restrictions), starts the
//! program suspended inside the AppContainer and the job with ONLY the three standard handles
//! inherited, resumes it, waits, then closes the job (killing leftovers), removes the ACEs and
//! deletes the profile. If the launcher itself is killed the job dies with it (kill on close);
//! the ACEs and the profile are then left behind, harmless because their SID no longer exists
//! as a usable principal (see the spec).
//!
//! Not run on a real Windows machine by the author: CI `windows-latest` only.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod imp;

/// Wire version of [`Spec`]; the launcher refuses any other.
pub const VERSION: u32 = 1;

/// What the AppContainer SID gets on a path. An AppContainer has no access unless an ACE names
/// its SID, so there is no deny: `Hide` and `ReadOnly` CUT the path off from the grants of its
/// parents (its DACL is protected, then restored exactly at exit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Read, list and execute; inherited by children.
    Read,
    /// Read, write, execute and delete the object (not change permissions or owner, not delete a
    /// directory's children through the directory); inherited.
    Modify,
    /// Cut off, then `Read`: readable, never writable, renamable or removable, even inside a
    /// `Modify` root.
    ReadOnly,
    /// Cut off and nothing granted: unreachable even inside a granted root.
    Hide,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub path: PathBuf,
    pub access: Access,
}

/// Everything the launcher needs; built by `synthesis-sandbox` (`windows.rs`), passed as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub version: u32,
    /// Absolute path of the executable (`lpApplicationName`).
    pub program: PathBuf,
    /// The full command line, already quoted (`lpCommandLine`).
    pub command_line: String,
    /// Applied in order, undone in reverse. The cuts (`Hide`, `ReadOnly`) come first: see `plan_grants`.
    pub grants: Vec<Grant>,
    /// Add the `internetClient`, `internetClientServer` and `privateNetworkClientServer`
    /// capabilities. Without them the container has no network at all (loopback included).
    pub network: bool,
    /// Variables the system needs to build the container's process (CreateProcess fails with
    /// `ERROR_ENVVAR_NOT_FOUND` when `LOCALAPPDATA` and the like are missing, which a scrubbed
    /// environment lacks). The launcher sets each one in ITS OWN environment when absent, so the
    /// program inherits them too: paths of the user's profile, not secrets.
    pub launcher_env: Vec<(String, String)>,
    /// Job Object `JobMemoryLimit`, bytes, whole job.
    pub memory: Option<u64>,
    /// Job Object `ActiveProcessLimit`.
    pub tasks: Option<u32>,
}

/// Whether an AppContainer can be created on this machine (creates and deletes a profile).
#[cfg(windows)]
pub fn probe() -> Result<(), String> {
    imp::probe()
}

/// Runs `spec` confined and returns the program's exit code. `Err` means nothing was run, or the
/// launch failed; the message says which step. Always cleans up what it changed.
#[cfg(windows)]
pub fn run(spec: &Spec) -> Result<u32, String> {
    if spec.version != VERSION {
        return Err(format!("unsupported spec version {}", spec.version));
    }
    imp::run(spec)
}

#[cfg(not(windows))]
pub fn probe() -> Result<(), String> {
    Err("AppContainer is Windows-only".into())
}

#[cfg(not(windows))]
pub fn run(_spec: &Spec) -> Result<u32, String> {
    Err("AppContainer is Windows-only".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_roundtrips_and_is_strict() {
        let s = Spec {
            version: VERSION,
            program: PathBuf::from("C:/x/a.exe"),
            command_line: "a.exe /c".into(),
            grants: vec![Grant {
                path: PathBuf::from("C:/w"),
                access: Access::ReadOnly,
            }],
            network: false,
            launcher_env: vec![("LOCALAPPDATA".into(), "C:/u".into())],
            memory: Some(1 << 30),
            tasks: Some(8),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("read_only"));
        assert_eq!(serde_json::from_str::<Spec>(&j).unwrap(), s);
        let extra = j.replacen("\"network\"", "\"extra\":1,\"network\"", 1);
        assert!(serde_json::from_str::<Spec>(&extra).is_err());
    }
}
