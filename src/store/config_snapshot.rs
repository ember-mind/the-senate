use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::domain::ConfigSnapshotId;

use super::StoreError;

const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedConfigSnapshot {
    id: ConfigSnapshotId,
    schema_version: u32,
    payload: Value,
    content_hash: String,
    created_at: DateTime<Utc>,
}

impl ResolvedConfigSnapshot {
    /// Creates immutable resolved configuration with deterministic JSON hashing.
    ///
    /// # Errors
    /// Rejects schema version zero or JSON serialization failure.
    pub fn new(
        id: ConfigSnapshotId,
        schema_version: u32,
        payload: Value,
        created_at: DateTime<Utc>,
    ) -> Result<Self, StoreError> {
        if schema_version == 0 {
            return Err(StoreError::InvalidConfigSchemaVersion);
        }
        let payload = canonicalize(payload);
        let content_hash = hash_payload(&payload)?;
        Ok(Self {
            id,
            schema_version,
            payload,
            content_hash,
            created_at,
        })
    }

    pub(crate) fn from_stored(
        id: ConfigSnapshotId,
        schema_version: u32,
        payload: Value,
        content_hash: &str,
        created_at: DateTime<Utc>,
    ) -> Result<Self, StoreError> {
        let snapshot = Self::new(id, schema_version, payload, created_at)?;
        if snapshot.content_hash != content_hash {
            return Err(StoreError::InvalidConfigHash(snapshot.id));
        }
        Ok(snapshot)
    }

    #[must_use]
    pub const fn id(&self) -> &ConfigSnapshotId {
        &self.id
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    pub(crate) fn payload_json(&self) -> Result<String, StoreError> {
        Ok(serde_json::to_string(&self.payload)?)
    }
}

/// Recursively key-sorts objects so equal content hashes equally.
pub(crate) fn canonical_value(value: Value) -> Value {
    canonicalize(value)
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let object = entries
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect::<Map<_, _>>();
            Value::Object(object)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        scalar => scalar,
    }
}

fn hash_payload(payload: &Value) -> Result<String, StoreError> {
    let encoded = serde_json::to_vec(payload)?;
    let digest = Sha256::digest(encoded);
    let mut hash = String::with_capacity(64);
    for byte in digest {
        hash.push(char::from(HEX[usize::from(byte >> 4)]));
        hash.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_hash_ignores_object_key_order() {
        let at = Utc.with_ymd_and_hms(2026, 8, 14, 8, 0, 0).single().unwrap();
        let left = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("config-left").unwrap(),
            1,
            json!({"z": 1, "nested": {"b": 2, "a": 1}}),
            at,
        )
        .unwrap();
        let right = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("config-right").unwrap(),
            1,
            serde_json::from_str(r#"{"nested":{"a":1,"b":2},"z":1}"#).unwrap(),
            at,
        )
        .unwrap();

        assert_eq!(left.payload_json().unwrap(), right.payload_json().unwrap());
        assert_eq!(left.content_hash(), right.content_hash());
        assert_eq!(left.content_hash().len(), 64);
    }
}
