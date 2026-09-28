use chrono::{DateTime, Utc};

use super::{
    AttentionRequest, ConfigSnapshotId, RunId, RunStatus, StageDefinition, StageId,
    StageRouteOverride, StageStatus, WorkflowKind,
};

/// Lifecycle state restored after a run-level pause or interruption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunResumeStatus {
    Running,
    NeedsUser,
}

/// Lifecycle owner responsible for restoring one suspended stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageSuspensionOwner {
    Stage,
    Run,
}

/// Lifecycle state restored after a stage pause or interruption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageResumeStatus {
    Running,
    NeedsUser,
}

/// Persistence-neutral reconstruction state for one stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageRehydrationData {
    pub id: StageId,
    pub status: StageStatus,
    pub suspension_owner: Option<StageSuspensionOwner>,
    pub resume_to: Option<StageResumeStatus>,
    pub route_override: Option<StageRouteOverride>,
}

/// Persistence-neutral reconstruction state for one run aggregate.
///
/// Values remain untrusted until passed through `Run::rehydrate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRehydrationData {
    pub id: RunId,
    pub workflow_kind: WorkflowKind,
    pub stage_definitions: Vec<StageDefinition>,
    pub config_snapshot_id: ConfigSnapshotId,
    pub status: RunStatus,
    pub suspended_from: Option<RunResumeStatus>,
    pub stages: Vec<StageRehydrationData>,
    pub attention_requests: Vec<AttentionRequest>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
