use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use ulid::Ulid;

use super::ProcessError;

/// Stable identity for one immutable external-process attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManagedProcessId(Ulid);

impl ManagedProcessId {
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(Ulid::from(value))
    }
}

impl Default for ManagedProcessId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ManagedProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ManagedProcessId {
    type Err = ProcessError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(value)
            .map(Self)
            .map_err(|_| ProcessError::InvalidIdentifier("managed process ID"))
    }
}

/// Backend-owned identity for one concrete supervisor session.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct BackendSessionId(String);

impl BackendSessionId {
    /// Creates an opaque, target-safe backend session identity.
    ///
    /// # Errors
    /// Rejects empty values and characters with target syntax meaning.
    pub fn new(value: impl Into<String>) -> Result<Self, ProcessError> {
        let value = value.into();
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ProcessError::InvalidIdentifier("backend session ID"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn for_process(process_id: ManagedProcessId) -> Self {
        Self(format!("senate-{process_id}").to_ascii_lowercase())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BackendSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for BackendSessionId {
    type Err = ProcessError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for BackendSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_session_identity_is_exact_target_safe_and_deterministic() {
        let process_id = ManagedProcessId::from_u128(42);
        let session = BackendSessionId::for_process(process_id);
        assert_eq!(session, BackendSessionId::for_process(process_id));
        assert!(session.as_str().starts_with("senate-"));
        assert!(BackendSessionId::new("bad:name").is_err());
        assert!(BackendSessionId::new("bad.name").is_err());
    }
}
