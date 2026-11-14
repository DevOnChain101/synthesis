//! Newtype ids (UUIDv4 text like every other id, ADR-016 item 8). Separate types so an attempt id
//! can never be passed where a decision id is expected. The existing `SessionId` and `TaskId`
//! `String` aliases are left alone.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId(pub String);

impl std::fmt::Display for InvalidId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid id {:?}: expected a lowercase UUID", self.0)
    }
}
impl std::error::Error for InvalidId {}

/// `8-4-4-4-12` lowercase hex. Version bits are not checked: ids come from `uuid::Uuid::new_v4`
/// in the store, and a restored or hand-made id must still round-trip.
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn parse(s: &str) -> Result<Self, InvalidId> {
                if is_uuid(s) { Ok(Self(s.to_owned())) } else { Err(InvalidId(s.to_owned())) }
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = InvalidId;
            fn try_from(s: String) -> Result<Self, InvalidId> {
                if is_uuid(&s) { Ok(Self(s)) } else { Err(InvalidId(s)) }
            }
        }
        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// An outcome.
    OutcomeId
);
id_type!(
    /// A work item (a DAG node of a plan version).
    WorkItemId
);
id_type!(
    /// One execution of a work item; owns exactly one session.
    AttemptId
);
id_type!(
    /// A durable human decision.
    DecisionId
);
id_type!(
    /// One evidence row.
    EvidenceId
);
id_type!(
    /// One mutating request from a surface (`outcome_commands`).
    CommandId
);

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "0b9f7f3e-1c2d-4e5f-8a9b-0123456789ab";

    #[test]
    fn parses_only_lowercase_uuids() {
        assert!(OutcomeId::parse(U).is_ok());
        for bad in [
            "",
            "x",
            &U.to_uppercase(),
            &U[..35],
            &format!("{U}0"),
            "0b9f7f3e_1c2d_4e5f_8a9b_0123456789ab",
            "0b9f7f3e-1c2d-4e5f-8a9b-0123456789ag",
            "../../etc/passwd",
        ] {
            assert!(OutcomeId::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn serde_round_trip_validates() {
        let id = AttemptId::parse(U).unwrap();
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(j, format!("\"{U}\""));
        assert_eq!(serde_json::from_str::<AttemptId>(&j).unwrap(), id);
        assert!(serde_json::from_str::<AttemptId>("\"nope\"").is_err());
    }
}
