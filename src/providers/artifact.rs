use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::domain::ArtifactMetadata;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRecord {
    metadata: ArtifactMetadata,
    attempt: u32,
    path: PathBuf,
    content_hash: String,
    content_size: u64,
    updated_at: DateTime<Utc>,
}

impl ArtifactRecord {
    /// Builds validated provider-owned artifact metadata.
    ///
    /// # Errors
    /// Rejects relative paths, malformed hashes, zero attempts, or regressed time.
    pub fn new(
        metadata: ArtifactMetadata,
        attempt: u32,
        path: PathBuf,
        content_hash: String,
        content_size: u64,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, ArtifactRecordError> {
        if attempt == 0 {
            return Err(ArtifactRecordError::InvalidAttempt);
        }
        if !path.is_absolute() {
            return Err(ArtifactRecordError::RelativePath(path));
        }
        if content_hash.len() != 64 || !content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ArtifactRecordError::InvalidHash);
        }
        if updated_at < *metadata.created_at() {
            return Err(ArtifactRecordError::TimestampRegression);
        }
        Ok(Self {
            metadata,
            attempt,
            path,
            content_hash,
            content_size,
            updated_at,
        })
    }

    #[must_use]
    pub const fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }
    #[must_use]
    pub const fn content_size(&self) -> u64 {
        self.content_size
    }
    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}

#[derive(Debug, Error)]
pub enum ArtifactRecordError {
    #[error("artifact attempt must be positive")]
    InvalidAttempt,
    #[error("artifact path must be absolute: {0}")]
    RelativePath(PathBuf),
    #[error("artifact content hash must be lowercase SHA-256 hex")]
    InvalidHash,
    #[error("artifact updated_at precedes created_at")]
    TimestampRegression,
}
