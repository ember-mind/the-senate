//! Explicit, provider-neutral state for recoverable orchestration.
//!
//! Domain types contain no persistence, process, network, Git, or UI behavior.

mod artifact;
mod attention;
mod effort;
mod event;
mod ids;
mod mission;
mod plan_change;
mod rehydration;
mod role;
mod run;
mod stage;
mod workflow;

pub use artifact::{ArtifactKind, ArtifactMetadata, ArtifactStatus};
pub use attention::{AttentionError, AttentionKind, AttentionRequest, AttentionStatus};
pub use effort::{EffortLevel, EffortParseError, EffortSetting};
pub use event::{DomainEvent, DomainEventKind, EventMetadata, NativeModelUsage};
pub use ids::{
    ArtifactId, AttentionRequestId, ConfigSnapshotId, DecisionId, EventId, IdError, MissionId,
    ModelId, ProviderId, ProviderSessionId, RunId, StageId, WorkPackageId,
};
pub use mission::{
    ChangedFile, DecisionAuthor, IntegrationEvidence, Mission, MissionAttention, MissionChange,
    MissionDecision, MissionError, MissionEvent, MissionEventKind, MissionInvariantError,
    MissionRehydrationData, MissionStatus, StageOutcome, WorkPackage, WorkPackageContract,
    WorkPackageRehydrationData, WorkPackageResult, WorkPackageStatus,
};
pub use plan_change::{PLAN_CHANGES_HEADING, PlanChange, PlanChangeParseError, parse_plan_changes};
pub use rehydration::{
    RunRehydrationData, RunResumeStatus, StageRehydrationData, StageResumeStatus,
    StageSuspensionOwner,
};
pub use role::Role;
pub use run::{
    BlockedDependency, CompletionBlocker, CompletionBlockerReason, DependencyOutcome, Run,
    RunAttentionError, RunFixError, RunInvariantError, RunProviderEventError, RunPublishError,
    RunRebaseError, RunRehydrationError, RunStageError, RunStatus, RunTransition,
    RunTransitionError, StageDependencyReport,
};
pub use stage::{
    Stage, StageRehydrationError, StageRouteOverride, StageStatus, StageTransition,
    StageTransitionError,
};
pub use workflow::{
    Dependency, DependencyKind, StageDefinition, StageKind, WorkflowDefinition,
    WorkflowDefinitionError, WorkflowKind, continue_cycle_stages, fix_cycle_stages,
    lead_turn_stages, next_follow_up_stage_id, next_lead_stage_id,
};
