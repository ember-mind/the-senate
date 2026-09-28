use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use ulid::Ulid;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum IdError {
    #[error("identifier must not be empty")]
    Empty,
    #[error("identifier must not contain whitespace: {0:?}")]
    ContainsWhitespace(String),
    #[error("invalid ULID: {0:?}")]
    InvalidUlid(String),
}

fn validate_string_id(value: impl Into<String>) -> Result<String, IdError> {
    let value = value.into();
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.chars().any(char::is_whitespace) {
        return Err(IdError::ContainsWhitespace(value));
    }
    Ok(value)
}

macro_rules! string_id {
    ($(#[$metadata:meta])* $name:ident) => {
        $(#[$metadata])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a validated identifier.
            ///
            /// # Errors
            /// Returns [`IdError`] when value is empty or contains whitespace.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                validate_string_id(value).map(Self)
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

macro_rules! ulid_id {
    ($(#[$metadata:meta])* $name:ident) => {
        $(#[$metadata])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Ulid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Ulid::new())
            }

            #[must_use]
            pub fn from_u128(value: u128) -> Self {
                Self(Ulid::from(value))
            }

            #[must_use]
            pub const fn as_ulid(self) -> Ulid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ulid::from_string(value)
                    .map(Self)
                    .map_err(|_| IdError::InvalidUlid(value.to_owned()))
            }
        }
    };
}

ulid_id!(
    /// Stable identity for one orchestration run.
    RunId
);
ulid_id!(
    /// Stable identity for one human-attention request.
    AttentionRequestId
);
ulid_id!(
    /// Stable identity for one produced artifact.
    ArtifactId
);
ulid_id!(
    /// Stable identity for one semantic domain event.
    EventId
);
ulid_id!(
    /// Stable identity for one mission: a project goal several runs serve.
    MissionId
);
ulid_id!(
    /// Stable identity for one recorded mission decision.
    DecisionId
);

string_id!(
    /// Stable workflow-local identity for one stage.
    StageId
);
string_id!(
    /// Identity of immutable effective configuration bound to a run.
    ConfigSnapshotId
);
string_id!(
    /// Stable mission-local identity for one work package.
    WorkPackageId
);
string_id!(
    /// Extensible provider identity such as a native CLI adapter name.
    ProviderId
);
string_id!(
    /// Provider-neutral model identity.
    ModelId
);
string_id!(
    /// Opaque native provider session or thread identity.
    ProviderSessionId
);

#[cfg(test)]
mod tests {
    use super::{ProviderSessionId, RunId, StageId};

    #[test]
    fn string_ids_reject_ambiguous_values() {
        assert!(StageId::new("").is_err());
        assert!(StageId::new("two words").is_err());
        assert_eq!(
            StageId::new("deep_analysis").unwrap().as_str(),
            "deep_analysis"
        );
        assert!(serde_json::from_str::<StageId>(r#""two words""#).is_err());
    }

    #[test]
    fn distinct_id_types_round_trip_without_becoming_interchangeable() {
        let run_id = RunId::from_u128(42);
        let encoded = serde_json::to_string(&run_id).expect("run ID should serialize");
        let decoded: RunId = serde_json::from_str(&encoded).expect("run ID should deserialize");

        assert_eq!(decoded, run_id);
        assert_eq!(
            ProviderSessionId::new("thread-42").unwrap().as_str(),
            "thread-42"
        );
    }
}
