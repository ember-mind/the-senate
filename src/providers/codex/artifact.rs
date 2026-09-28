use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::domain::{
    ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactStatus, ModelId, ProviderId, StageKind,
};
use crate::engine::ProviderRequest;
use crate::providers::ArtifactRecord;

use super::CodexProviderError;

/// Ceiling for one persisted artifact, and therefore for any final message
/// that could become one.
pub(super) const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;

/// Whether these bytes could be persisted as an artifact.
///
/// Measured as [`persist`] measures them, after the missing trailing newline
/// it appends: that byte is written, so it counts against the ceiling. A
/// message already ending in a newline may therefore be exactly the ceiling,
/// and one that does not may not.
pub(super) fn fits_ceiling(bytes: &[u8]) -> bool {
    bytes
        .len()
        .saturating_add(usize::from(!bytes.ends_with(b"\n")))
        <= MAX_ARTIFACT_BYTES
}

pub(crate) fn persist(
    root: &Path,
    final_message_path: &Path,
    request: &ProviderRequest,
    provider_id: &ProviderId,
    model_id: Option<&ModelId>,
    base_commit: &str,
    now: DateTime<Utc>,
) -> Result<ArtifactRecord, CodexProviderError> {
    let mut bytes = std::fs::read(final_message_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CodexProviderError::MissingFinalMessage(final_message_path.to_path_buf())
        } else {
            error.into()
        }
    })?;
    // Normalize before measuring. The appended newline is part of what gets
    // written, so checking the file's own length would let a message of
    // exactly the ceiling be persisted one byte over it.
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(CodexProviderError::ArtifactTooLarge(MAX_ARTIFACT_BYTES));
    }
    let directory = root.join(request.run_id().to_string()).join("artifacts");
    std::fs::create_dir_all(&directory)?;
    let filename = if request.attempt() == 1 {
        format!("{}.md", request.stage_id())
    } else {
        format!("{}-attempt-{}.md", request.stage_id(), request.attempt())
    };
    let path = directory.join(filename);
    write_once(&path, &bytes)?;
    let hash = hex_sha256(&bytes);
    let metadata = ArtifactMetadata::new(
        ArtifactId::new(),
        request.run_id(),
        request.stage_id().clone(),
        kind(request.stage_kind()),
        request.role(),
        ArtifactStatus::Complete,
        now,
    )
    .with_provider(provider_id.clone(), model_id.cloned())
    .with_base_commit(base_commit);
    ArtifactRecord::new(
        metadata,
        request.attempt(),
        path,
        hash,
        u64::try_from(bytes.len()).expect("bounded artifact length fits u64"),
        now,
    )
    .map_err(|error| CodexProviderError::Protocol(error.to_string()))
}

fn write_once(path: &Path, bytes: &[u8]) -> Result<(), CodexProviderError> {
    if path.exists() {
        return if std::fs::read(path)? == bytes {
            Ok(())
        } else {
            Err(CodexProviderError::ArtifactConflict(PathBuf::from(path)))
        };
    }
    let directory = path
        .parent()
        .ok_or_else(|| CodexProviderError::ArtifactConflict(path.to_path_buf()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(path)? != bytes {
                return Err(CodexProviderError::ArtifactConflict(path.to_path_buf()));
            }
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(&mut result, "{byte:02x}").expect("String writes cannot fail");
    }
    result
}

const fn kind(stage: StageKind) -> ArtifactKind {
    match stage {
        StageKind::Research => ArtifactKind::Research,
        StageKind::Architecture => ArtifactKind::Architecture,
        StageKind::Implementation => ArtifactKind::Implementation,
        StageKind::Simplification => ArtifactKind::Simplification,
        StageKind::CodeQualityReview => ArtifactKind::CodeQualityReview,
        StageKind::SpecReview => ArtifactKind::SpecReview,
        StageKind::Review | StageKind::IndependentReview | StageKind::DeepAnalysis => {
            ArtifactKind::Review
        }
        StageKind::Synthesis => ArtifactKind::Synthesis,
        StageKind::Decision => ArtifactKind::Decision,
        StageKind::Fix => ArtifactKind::Fix,
        StageKind::FollowUp => ArtifactKind::FollowUp,
        StageKind::Lead => ArtifactKind::Lead,
        StageKind::Verify => ArtifactKind::Verify,
    }
}
