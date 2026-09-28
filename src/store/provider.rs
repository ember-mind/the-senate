use std::path::PathBuf;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::domain::{ArtifactMetadata, ModelId, ProviderId, ProviderSessionId, RunId, StageId};
use crate::domain::{DomainEvent, Run};
use crate::process::ProcessError;
use crate::providers::{
    ArtifactRecord, PendingProviderAttention, ProviderSessionRecord, ProviderSessionRecordId,
    ProviderSessionRevision, ProviderSessionStatus,
};

use super::process::{acknowledge_process_output_row, ensure_execution_guard};
use super::sqlite::{
    commit_run_update_transaction, format_timestamp, i64_to_u64, parse_timestamp, u64_to_i64,
};
use super::{CommitResult, RunRevision, SqliteStore, StoreError};
use crate::providers::ProviderCommit;

impl SqliteStore {
    /// Commits one semantic provider record exactly once with all SQLite-owned checkpoints.
    ///
    /// Artifact bytes must already be durable; transaction binds metadata, run/events,
    /// provider-session CAS, and output-cursor CAS atomically.
    ///
    /// # Errors
    /// Returns guard, concurrency, integrity, domain, or persistence failures.
    pub(crate) fn commit_provider_execution_update(
        &mut self,
        run: &Run,
        expected_revision: RunRevision,
        events: &[DomainEvent],
        commit: &ProviderCommit,
    ) -> Result<CommitResult, ProcessError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_execution_guard(&transaction, run.id())?;
        ensure_provider_commit_identity(&transaction, run.id(), commit)?;
        if let Some(artifact) = commit.artifact() {
            verify_artifact(artifact)?;
        }
        let result =
            commit_run_update_transaction(&transaction, run, expected_revision, events, false)?;
        if let Some(session) = commit.session() {
            update_provider_session_row(
                &transaction,
                session.session(),
                session.expected_revision(),
            )?;
        }
        if let Some(artifact) = commit.artifact() {
            insert_artifact_row(&transaction, artifact)?;
        }
        acknowledge_process_output_row(&transaction, commit.output(), commit.acknowledged_end())?;
        transaction.commit()?;
        Ok(result)
    }

    /// Advances an ignorable provider record without creating semantic history.
    ///
    /// # Errors
    /// Returns guard, session/output concurrency, or persistence failures.
    pub(crate) fn commit_provider_checkpoint(
        &mut self,
        commit: &ProviderCommit,
    ) -> Result<(), ProcessError> {
        if commit.artifact().is_some() {
            return Err(ProcessError::InvalidSpec(
                "artifact checkpoint requires semantic completion",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run_id = if let Some(session) = commit.session() {
            session.session().run_id()
        } else {
            let raw = transaction.query_row(
                "SELECT run_id FROM managed_processes WHERE id = ?1",
                [commit.output().process_id().to_string()],
                |row| row.get::<_, String>(0),
            )?;
            raw.parse()
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid process run ID"))?
        };
        ensure_execution_guard(&transaction, run_id)?;
        ensure_provider_commit_identity(&transaction, run_id, commit)?;
        if let Some(session) = commit.session() {
            update_provider_session_row(
                &transaction,
                session.session(),
                session.expected_revision(),
            )?;
        }
        acknowledge_process_output_row(&transaction, commit.output(), commit.acknowledged_end())?;
        transaction.commit()?;
        Ok(())
    }

    /// Inserts one logical provider-session identity.
    ///
    /// # Errors
    /// Rejects duplicate attempts, invalid records, or persistence failures.
    pub fn insert_provider_session(
        &mut self,
        session: &ProviderSessionRecord,
    ) -> Result<ProviderSessionRecord, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT INTO provider_sessions (
                 id, run_id, stage_id, attempt, provider_id, native_session_id,
                 current_process_id, status, protocol_version, invocation, model_id,
                 cli_version, pending_attention_id, pending_process_id,
                 pending_record_start, pending_record_end, revision, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       NULL, NULL, NULL, NULL, 0, ?13, ?14)
             ON CONFLICT DO NOTHING",
            params![
                session.id().to_string(),
                session.run_id().to_string(),
                session.stage_id().as_str(),
                i64::from(session.attempt()),
                session.provider_id().as_str(),
                session.native_session_id().map(ProviderSessionId::as_str),
                session.current_process_id().map(|id| id.to_string()),
                session.status().as_str(),
                i64::from(session.protocol_version()),
                i64::from(session.invocation()),
                session.model_id().map(ModelId::as_str),
                session.cli_version(),
                format_timestamp(session.created_at()),
                format_timestamp(session.updated_at()),
            ],
        )?;
        if inserted == 0 {
            return Err(StoreError::ProviderSessionConflict {
                run_id: session.run_id(),
                stage_id: session.stage_id().clone(),
                attempt: session.attempt(),
            });
        }
        transaction.commit()?;
        self.load_provider_session(session.id())
    }

    /// Loads one provider session and validates every persisted projection.
    ///
    /// # Errors
    /// Returns not-found, invalid identity/state, or persistence failures.
    pub fn load_provider_session(
        &self,
        id: ProviderSessionRecordId,
    ) -> Result<ProviderSessionRecord, StoreError> {
        load_provider_session_from(&self.connection, id)?
            .ok_or(StoreError::ProviderSessionNotFound(id))
    }

    /// Loads one logical provider session for a stage attempt.
    ///
    /// # Errors
    /// Returns invalid stored state or persistence failures.
    pub fn load_provider_session_for_attempt(
        &self,
        run_id: RunId,
        stage_id: &StageId,
        attempt: u32,
    ) -> Result<Option<ProviderSessionRecord>, StoreError> {
        let id = self
            .connection
            .query_row(
                "SELECT id FROM provider_sessions
                 WHERE run_id = ?1 AND stage_id = ?2 AND attempt = ?3",
                params![run_id.to_string(), stage_id.as_str(), i64::from(attempt)],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        id.map(|id| {
            let id = id
                .parse()
                .map_err(|_| StoreError::InvalidProviderSession("invalid record ID".to_owned()))?;
            self.load_provider_session(id)
        })
        .transpose()
    }

    /// Lists all provider sessions for one run in stable attempt order.
    ///
    /// # Errors
    /// Returns invalid stored state or persistence failures.
    pub fn list_provider_sessions(
        &self,
        run_id: RunId,
    ) -> Result<Vec<ProviderSessionRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM provider_sessions
             WHERE run_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut sessions = Vec::new();
        for id in rows {
            let id = id?
                .parse()
                .map_err(|_| StoreError::InvalidProviderSession("invalid record ID".to_owned()))?;
            sessions.push(self.load_provider_session(id)?);
        }
        Ok(sessions)
    }

    pub(crate) fn update_provider_session(
        &mut self,
        session: &ProviderSessionRecord,
        expected: ProviderSessionRevision,
    ) -> Result<ProviderSessionRecord, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        update_provider_session_row(&transaction, session, expected)?;
        transaction.commit()?;
        self.load_provider_session(session.id())
    }

    /// Inserts immutable artifact metadata after verifying durable file bytes.
    ///
    /// # Errors
    /// Rejects missing/mismatched files, duplicate attempt artifacts, or `SQLite` failures.
    pub fn insert_artifact(&mut self, artifact: &ArtifactRecord) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_artifact(artifact)?;
        insert_artifact_row(&transaction, artifact)?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads artifact metadata for one run.
    ///
    /// # Errors
    /// Returns invalid projections, integrity failures, or `SQLite` failures.
    pub fn list_artifacts(&self, run_id: RunId) -> Result<Vec<ArtifactRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, stage_id, attempt, kind, status, role, provider_id, model_id,
                    path, content_hash, content_size, base_commit, created_at, updated_at
             FROM artifacts WHERE run_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
            ))
        })?;
        let mut artifacts = Vec::new();
        for row in rows {
            let (
                id,
                stage_id,
                attempt,
                kind,
                status,
                role,
                provider_id,
                model_id,
                path,
                content_hash,
                content_size,
                base_commit,
                created_at,
                updated_at,
            ) = row?;
            let created_at = parse_timestamp(&created_at)?;
            let mut metadata = ArtifactMetadata::new(
                id.parse()
                    .map_err(|_| StoreError::SnapshotProjectionMismatch("invalid artifact ID"))?,
                run_id,
                StageId::new(stage_id).map_err(|_| {
                    StoreError::SnapshotProjectionMismatch("invalid artifact stage ID")
                })?,
                enum_from_text(&kind)?,
                enum_from_text(&role)?,
                enum_from_text(&status)?,
                created_at,
            );
            if let Some(provider_id) = provider_id {
                metadata = metadata.with_provider(
                    ProviderId::new(provider_id).map_err(|_| {
                        StoreError::SnapshotProjectionMismatch("invalid artifact provider ID")
                    })?,
                    model_id.map(ModelId::new).transpose().map_err(|_| {
                        StoreError::SnapshotProjectionMismatch("invalid artifact model ID")
                    })?,
                );
            } else if model_id.is_some() {
                return Err(StoreError::SnapshotProjectionMismatch(
                    "artifact model exists without provider",
                ));
            }
            if let Some(base_commit) = base_commit {
                metadata = metadata.with_base_commit(base_commit);
            }
            let artifact = ArtifactRecord::new(
                metadata,
                u32::try_from(attempt).map_err(|_| StoreError::IntegerRange("artifact attempt"))?,
                PathBuf::from(path),
                content_hash,
                i64_to_u64(content_size, "artifact content size")?,
                parse_timestamp(&updated_at)?,
            )?;
            verify_artifact(&artifact)?;
            artifacts.push(artifact);
        }
        Ok(artifacts)
    }
}

fn ensure_provider_commit_identity(
    connection: &rusqlite::Connection,
    run_id: RunId,
    commit: &ProviderCommit,
) -> Result<(), ProcessError> {
    let (process_run, process_stage, process_attempt) = connection.query_row(
        "SELECT run_id, stage_id, attempt FROM managed_processes WHERE id = ?1",
        [commit.output().process_id().to_string()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    let process_attempt = u32::try_from(process_attempt)
        .map_err(|_| ProcessError::InvalidStoredProcess("invalid provider process attempt"))?;
    if process_run != run_id.to_string() {
        return Err(ProcessError::InvalidStoredProcess(
            "provider commit run/process mismatch",
        ));
    }
    if let Some(session) = commit.session() {
        let session = session.session();
        if session.run_id() != run_id
            || session.stage_id().as_str() != process_stage
            || session.attempt() != process_attempt
            || session.current_process_id() != Some(commit.output().process_id())
        {
            return Err(ProcessError::InvalidStoredProcess(
                "provider commit session/process mismatch",
            ));
        }
    } else {
        let owns_process = connection
            .query_row(
                "SELECT 1 FROM provider_sessions
                 WHERE run_id = ?1 AND stage_id = ?2 AND attempt = ?3
                   AND current_process_id = ?4",
                params![
                    run_id.to_string(),
                    process_stage,
                    i64::from(process_attempt),
                    commit.output().process_id().to_string()
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !owns_process {
            return Err(ProcessError::InvalidStoredProcess(
                "provider output has no owning session",
            ));
        }
    }
    if let Some(artifact) = commit.artifact() {
        let metadata = artifact.metadata();
        if metadata.run_id() != run_id
            || metadata.stage_id().as_str() != process_stage
            || artifact.attempt() != process_attempt
        {
            return Err(ProcessError::InvalidStoredProcess(
                "provider artifact/process mismatch",
            ));
        }
    }
    Ok(())
}

pub(crate) fn update_provider_session_row(
    transaction: &rusqlite::Transaction<'_>,
    session: &ProviderSessionRecord,
    expected: ProviderSessionRevision,
) -> Result<(), StoreError> {
    let next = expected
        .value()
        .checked_add(1)
        .ok_or(StoreError::IntegerRange("next provider session revision"))?;
    let pending = session.pending_attention();
    let changed = transaction.execute(
        "UPDATE provider_sessions
         SET native_session_id = ?1, current_process_id = ?2, status = ?3,
             invocation = ?4, model_id = ?5, cli_version = ?6,
             pending_attention_id = ?7, pending_process_id = ?8,
             pending_record_start = ?9, pending_record_end = ?10,
             revision = ?11, updated_at = ?12
         WHERE id = ?13 AND revision = ?14",
        params![
            session.native_session_id().map(ProviderSessionId::as_str),
            session.current_process_id().map(|id| id.to_string()),
            session.status().as_str(),
            i64::from(session.invocation()),
            session.model_id().map(ModelId::as_str),
            session.cli_version(),
            pending.map(|pending| pending.attention_id().to_string()),
            pending.map(|pending| pending.process_id().to_string()),
            pending
                .map(|pending| u64_to_i64(pending.record_start(), "attention start"))
                .transpose()?,
            pending
                .map(|pending| u64_to_i64(pending.record_end(), "attention end"))
                .transpose()?,
            u64_to_i64(next, "provider session revision")?,
            format_timestamp(session.updated_at()),
            session.id().to_string(),
            u64_to_i64(expected.value(), "expected provider session revision")?,
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::ProviderSessionConcurrentModification {
            id: session.id(),
            expected: expected.value(),
        });
    }
    Ok(())
}

pub(crate) fn insert_artifact_row(
    transaction: &rusqlite::Transaction<'_>,
    artifact: &ArtifactRecord,
) -> Result<(), StoreError> {
    let metadata = artifact.metadata();
    let path = artifact
        .path()
        .to_str()
        .ok_or_else(|| StoreError::ArtifactIntegrity(artifact.path().to_path_buf()))?;
    let inserted = transaction.execute(
        "INSERT INTO artifacts (
             id, run_id, stage_id, attempt, kind, status, role, provider_id,
             model_id, path, content_hash, content_size, base_commit, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT DO NOTHING",
        params![
            metadata.id().to_string(),
            metadata.run_id().to_string(),
            metadata.stage_id().as_str(),
            i64::from(artifact.attempt()),
            enum_text(metadata.kind())?,
            enum_text(metadata.status())?,
            enum_text(metadata.role())?,
            metadata.provider_id().map(ProviderId::as_str),
            metadata.model_id().map(ModelId::as_str),
            path,
            artifact.content_hash(),
            u64_to_i64(artifact.content_size(), "artifact content size")?,
            metadata.base_commit(),
            format_timestamp(metadata.created_at()),
            format_timestamp(artifact.updated_at()),
        ],
    )?;
    if inserted == 0 {
        return Err(StoreError::ArtifactConflict {
            run_id: metadata.run_id(),
            stage_id: metadata.stage_id().clone(),
            attempt: artifact.attempt(),
        });
    }
    Ok(())
}

pub(crate) fn verify_artifact(artifact: &ArtifactRecord) -> Result<(), StoreError> {
    let bytes = std::fs::read(artifact.path())?;
    let size = u64::try_from(bytes.len()).map_err(|_| StoreError::IntegerRange("artifact size"))?;
    let digest = Sha256::digest(&bytes);
    let mut hash = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hash, "{byte:02x}").expect("writing to String cannot fail");
    }
    if size != artifact.content_size() || hash != artifact.content_hash() {
        return Err(StoreError::ArtifactIntegrity(artifact.path().to_path_buf()));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one row decoder validates every provider-session projection"
)]
fn load_provider_session_from(
    connection: &rusqlite::Connection,
    id: ProviderSessionRecordId,
) -> Result<Option<ProviderSessionRecord>, StoreError> {
    let row = connection
        .query_row(
            "SELECT run_id, stage_id, attempt, provider_id, native_session_id,
                    current_process_id, status, protocol_version, invocation, model_id,
                    cli_version, pending_attention_id, pending_process_id,
                    pending_record_start, pending_record_end, revision, created_at, updated_at
             FROM provider_sessions WHERE id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, String>(17)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        let (
            run_id,
            stage_id,
            attempt,
            provider_id,
            native_session_id,
            current_process_id,
            status,
            protocol_version,
            invocation,
            model_id,
            cli_version,
            pending_attention_id,
            pending_process_id,
            pending_start,
            pending_end,
            revision,
            created_at,
            updated_at,
        ) = row;
        let pending_attention = match (
            pending_attention_id,
            pending_process_id,
            pending_start,
            pending_end,
        ) {
            (None, None, None, None) => None,
            (Some(attention), Some(process), Some(start), Some(end)) => Some(
                PendingProviderAttention::new(
                    attention.parse().map_err(|_| {
                        StoreError::InvalidProviderSession("invalid attention ID".to_owned())
                    })?,
                    process.parse().map_err(|_| {
                        StoreError::InvalidProviderSession("invalid pending process ID".to_owned())
                    })?,
                    i64_to_u64(start, "pending record start")?,
                    i64_to_u64(end, "pending record end")?,
                )
                .map_err(|error| StoreError::InvalidProviderSession(error.to_owned()))?,
            ),
            _ => {
                return Err(StoreError::InvalidProviderSession(
                    "partial pending attention projection".to_owned(),
                ));
            }
        };
        ProviderSessionRecord::from_stored(
            id,
            run_id
                .parse()
                .map_err(|_| StoreError::InvalidProviderSession("invalid run ID".to_owned()))?,
            StageId::new(stage_id)
                .map_err(|_| StoreError::InvalidProviderSession("invalid stage ID".to_owned()))?,
            u32::try_from(attempt).map_err(|_| StoreError::IntegerRange("provider attempt"))?,
            ProviderId::new(provider_id).map_err(|_| {
                StoreError::InvalidProviderSession("invalid provider ID".to_owned())
            })?,
            native_session_id
                .map(ProviderSessionId::new)
                .transpose()
                .map_err(|_| {
                    StoreError::InvalidProviderSession("invalid native session ID".to_owned())
                })?,
            current_process_id
                .map(|id| id.parse())
                .transpose()
                .map_err(|_| {
                    StoreError::InvalidProviderSession("invalid current process ID".to_owned())
                })?,
            ProviderSessionStatus::from_str(&status)
                .map_err(|error| StoreError::InvalidProviderSession(error.to_owned()))?,
            u32::try_from(protocol_version)
                .map_err(|_| StoreError::IntegerRange("provider protocol version"))?,
            u32::try_from(invocation)
                .map_err(|_| StoreError::IntegerRange("provider invocation"))?,
            model_id
                .map(ModelId::new)
                .transpose()
                .map_err(|_| StoreError::InvalidProviderSession("invalid model ID".to_owned()))?,
            cli_version,
            pending_attention,
            ProviderSessionRevision::new(i64_to_u64(revision, "provider revision")?),
            parse_timestamp(&created_at)?,
            parse_timestamp(&updated_at)?,
        )
        .map_err(|error| StoreError::InvalidProviderSession(error.to_owned()))
    })
    .transpose()
}

fn enum_text<T: serde::Serialize>(value: T) -> Result<String, StoreError> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or(StoreError::SnapshotProjectionMismatch(
            "invalid enum projection",
        ))
}

fn enum_from_text<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.to_owned(),
    ))?)
}
