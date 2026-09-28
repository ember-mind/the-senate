use std::path::PathBuf;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::domain::{
    ConfigSnapshotId, EventId, MissionId, MissionInvariantError, RunId, RunInvariantError,
    RunRehydrationError, StageId, WorkPackageId,
};
use crate::providers::{ArtifactRecordError, ProviderSessionRecordId};

use super::RunInputError;
use super::mission::MissionInputError;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON operation failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("stored timestamp is invalid: {0}")]
    Timestamp(#[from] chrono::ParseError),
    #[error("stored run failed domain validation: {0}")]
    Rehydration(#[from] RunRehydrationError),
    #[error("run is invalid before persistence: {0}")]
    InvalidRun(#[from] RunInvariantError),
    #[error("stored run input is invalid: {0}")]
    RunInput(#[from] RunInputError),
    #[error("database schema version {0} is newer than this Senate build supports")]
    UnsupportedDatabaseVersion(u32),
    #[error("run snapshot schema version {0} is unsupported")]
    UnsupportedSnapshotVersion(u32),
    #[error("snapshot envelope has no valid schema_version")]
    InvalidSnapshotEnvelope,
    #[error("snapshot schema version {snapshot} disagrees with runs column {column}")]
    SnapshotVersionMismatch { snapshot: u32, column: u32 },
    #[error("run {0} does not exist")]
    RunNotFound(RunId),
    #[error("run {0} already exists")]
    RunAlreadyExists(RunId),
    #[error("run input for {0} does not exist")]
    RunInputNotFound(RunId),
    #[error("run input for {0} is immutable and stored content differs")]
    RunInputConflict(RunId),
    #[error("persisted mission violates an invariant: {0}")]
    InvalidMission(#[from] MissionInvariantError),
    #[error(transparent)]
    MissionInput(#[from] MissionInputError),
    #[error("mission {0} does not exist")]
    MissionNotFound(MissionId),
    #[error("mission {0} already exists")]
    MissionAlreadyExists(MissionId),
    #[error("mission input for {0} does not exist")]
    MissionInputNotFound(MissionId),
    #[error("mission {mission_id} changed since revision {expected}")]
    MissionConcurrentModification {
        mission_id: MissionId,
        expected: u64,
    },
    #[error("event {event_id} belongs to mission {actual}, not {expected}")]
    EventMissionMismatch {
        event_id: EventId,
        expected: MissionId,
        actual: MissionId,
    },
    #[error("event {event_id} names package {package_id}, which the mission does not have")]
    EventPackageMismatch {
        event_id: EventId,
        package_id: WorkPackageId,
    },
    #[error("mission {mission_id} event sequence gap: expected {expected}, found {actual}")]
    MissionEventSequenceGap {
        mission_id: MissionId,
        expected: u64,
        actual: u64,
    },
    #[error("run {run_id} belongs to mission {mission_id}")]
    RunBoundToMission {
        run_id: RunId,
        mission_id: MissionId,
    },
    #[error("config snapshot {0} does not exist")]
    ConfigSnapshotNotFound(ConfigSnapshotId),
    #[error("config snapshot {0} is immutable and stored content differs")]
    ConfigSnapshotConflict(ConfigSnapshotId),
    #[error("config snapshot schema version must be positive")]
    InvalidConfigSchemaVersion,
    #[error("config snapshot {0} has an invalid content hash")]
    InvalidConfigHash(ConfigSnapshotId),
    #[error("run {run_id} changed since revision {expected}")]
    ConcurrentModification { run_id: RunId, expected: u64 },
    #[error("run snapshot and indexed columns disagree: {0}")]
    SnapshotProjectionMismatch(&'static str),
    #[error("persisted run identity field cannot change: {0}")]
    ImmutableRunFieldChanged(&'static str),
    #[error("commit must include at least one semantic event")]
    EmptyEventBatch,
    #[error("initial event batch must begin with matching run_created event")]
    InvalidInitialEvent,
    #[error("run_created event is only valid in initial event batch")]
    UnexpectedRunCreatedEvent,
    #[error("event {event_id} belongs to run {actual}, expected {expected}")]
    EventRunMismatch {
        event_id: EventId,
        expected: RunId,
        actual: RunId,
    },
    #[error("event {event_id} references unknown stage {stage_id}")]
    EventStageMismatch {
        event_id: EventId,
        stage_id: StageId,
    },
    #[error("event {event_id} at {occurred_at} precedes prior event at {previous}")]
    EventTimestampRegression {
        event_id: EventId,
        previous: DateTime<Utc>,
        occurred_at: DateTime<Utc>,
    },
    #[error("last event timestamp must equal run updated_at")]
    EventStateTimestampMismatch,
    #[error("stored event row and payload disagree: {0}")]
    EventProjectionMismatch(&'static str),
    #[error("event sequence for run {run_id} expected {expected}, found {actual}")]
    EventSequenceGap {
        run_id: RunId,
        expected: u64,
        actual: u64,
    },
    #[error("stored integer is outside supported range: {0}")]
    IntegerRange(&'static str),
    #[error("cannot resolve data path: set SENATE_DATA_DIR or HOME")]
    DataPathUnavailable,
    #[error("workspace for run {0} already exists")]
    WorkspaceAlreadyExists(RunId),
    #[error("workspace for run {run_id} changed since revision {expected}")]
    WorkspaceConcurrentModification { run_id: RunId, expected: u64 },
    #[error("apply operation for run {0} already exists")]
    ApplyOperationAlreadyExists(RunId),
    #[error("apply operation for run {0} does not exist")]
    ApplyOperationNotFound(RunId),
    #[error("apply operation for run {run_id} changed since revision {expected}")]
    ApplyOperationConcurrentModification { run_id: RunId, expected: u64 },
    #[error("run {0} cannot change while an apply operation is active")]
    RunFrozenForApply(RunId),
    #[error("run {run_id} execution requires a ready workspace, found {status:?}")]
    ExecutionWorkspaceNotReady {
        run_id: RunId,
        status: Option<String>,
    },
    #[error("stored workspace record is invalid: {0}")]
    InvalidWorkspaceRecord(String),
    #[error("workspace path is not valid UTF-8: {0}")]
    NonUtf8WorkspacePath(PathBuf),
    #[error("stored provider session is invalid: {0}")]
    InvalidProviderSession(String),
    #[error("stored image generation is invalid: {0}")]
    InvalidImageGeneration(String),
    #[error("provider session {0} does not exist")]
    ProviderSessionNotFound(ProviderSessionRecordId),
    #[error("provider session for run {run_id} stage {stage_id} attempt {attempt} already exists")]
    ProviderSessionConflict {
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
    },
    #[error("provider session {id} changed since revision {expected}")]
    ProviderSessionConcurrentModification {
        id: ProviderSessionRecordId,
        expected: u64,
    },
    #[error(transparent)]
    ArtifactRecord(#[from] ArtifactRecordError),
    #[error("artifact for run {run_id} stage {stage_id} attempt {attempt} already exists")]
    ArtifactConflict {
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
    },
    #[error("artifact file does not match persisted metadata: {0}")]
    ArtifactIntegrity(PathBuf),
}

impl StoreError {
    /// Whether this is an optimistic-concurrency loss: the row moved under a
    /// reader that had already decided what to write. Idempotent callers
    /// repeat instead of surfacing a revision number.
    #[must_use]
    pub const fn is_lost_revision(&self) -> bool {
        matches!(
            self,
            Self::ConcurrentModification { .. }
                | Self::WorkspaceConcurrentModification { .. }
                | Self::ApplyOperationConcurrentModification { .. }
                | Self::ProviderSessionConcurrentModification { .. }
        )
    }
}
