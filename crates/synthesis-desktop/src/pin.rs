//! The pinned agent-desktop release. This file is the ONE place the pin lives.
//!
//! UNVERIFIED: the values below are placeholders. The owner fills them from a real release
//! after the D0 spike (docs/DESKTOP-CONTROL.md, "D0 checklist"): the version `agent-desktop
//! version` prints and the sha256 of each platform's binary. Until then no binary matches and
//! the tools refuse to run unless the user sets `synt.desktop.allow_unpinned: true`.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Version `agent-desktop version` must report. UNVERIFIED placeholder.
pub const PINNED_VERSION: &str = "UNVERIFIED";

/// `(<os>-<arch>, sha256 hex of the executable)`. UNVERIFIED placeholders.
pub const PINNED_SHA256: &[(&str, &str)] = &[
    ("macos-aarch64", "UNVERIFIED"),
    ("macos-x86_64", "UNVERIFIED"),
    ("linux-x86_64", "UNVERIFIED"),
    ("linux-aarch64", "UNVERIFIED"),
    ("windows-x86_64", "UNVERIFIED"),
    ("windows-aarch64", "UNVERIFIED"),
];

/// The pin to check against (tests pass their own).
#[derive(Debug, Clone)]
pub struct Pin {
    pub version: String,
    /// Expected sha256 of this platform's executable.
    pub sha256: String,
}

impl Pin {
    /// The compiled-in pin for the running platform.
    pub fn builtin() -> Self {
        let me = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        Self {
            version: PINNED_VERSION.into(),
            sha256: PINNED_SHA256
                .iter()
                .find(|(t, _)| *t == me)
                .map_or("UNVERIFIED", |(_, h)| h)
                .into(),
        }
    }

    /// False while the values are still placeholders.
    pub fn is_filled(&self) -> bool {
        self.version != "UNVERIFIED"
            && self.sha256.len() == 64
            && self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinStatus {
    Match,
    /// The pin is still a placeholder: nothing can match.
    Unpinned,
    Mismatch {
        actual: String,
    },
}

pub fn check(path: &Path, pin: &Pin) -> std::io::Result<PinStatus> {
    if !pin.is_filled() {
        return Ok(PinStatus::Unpinned);
    }
    let actual = sha256_file(path)?;
    Ok(if actual.eq_ignore_ascii_case(&pin.sha256) {
        PinStatus::Match
    } else {
        PinStatus::Mismatch { actual }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_never_match_and_a_filled_pin_compares_the_hash() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("bin");
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(check(&f, &Pin::builtin()).unwrap(), PinStatus::Unpinned);
        let sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let pin = Pin {
            version: "1.0.0".into(),
            sha256: sha.into(),
        };
        assert_eq!(check(&f, &pin).unwrap(), PinStatus::Match);
        std::fs::write(&f, b"tampered").unwrap();
        assert!(matches!(
            check(&f, &pin).unwrap(),
            PinStatus::Mismatch { .. }
        ));
    }
}
