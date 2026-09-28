//! Application use cases coordinating domain, persistence, Git, and providers.

mod error;
mod mission_lead;
pub(crate) mod mission_query;
mod mission_result;
mod mission_service;
mod provider_factory;
pub(crate) mod query;
mod routing;
mod run_service;

pub use error::AppError;
pub use mission_lead::{LeadAnswer, LeadSummary, LeadTurn};
pub use mission_query::{
    DecisionSummary, HandoffSummary, MissionDetails, MissionListItem, WorkPackageSummary,
};
pub use mission_service::{MissionService, NewWorkPackage, Rework};
pub use provider_factory::{
    DevelopmentFakeProviderFactory, ProviderFactory, ProviderResolver, RoutedProvider,
    RuntimeProvider, RuntimeProviderFactory,
};
pub use query::{
    ArtifactSummary, ArtifactView, AttentionSummary, BlockedDependencyRef, CODEX_FALLBACK_MODEL,
    ChangedFileSummary, CommittedEvent, ImageGenerationSummary, ProcessLogStream, ProcessLogView,
    ProviderUsage, Publication, RouteSummary, RunDetails, RunDiffPreview, RunListItem, RunUsage,
    StageDependencyRef, StageExecutionEvidence, StageSummary, StageWaitingSummary, UsageSummary,
};
pub(crate) use routing::resolve_eval_config;
pub use routing::{
    DEFAULT_MAX_IMAGE_GENERATIONS, DecisionBasis, DecisionConfidence, EffortRequest,
    ExecutionSelection, ExecutionTarget, ImageGenerationPlan, MAX_IMAGE_GENERATIONS_CEILING,
    OPERATOR_OVERRIDE_REASON, RECOMMENDED_PROFILE_VERSION, RECOMMENDED_PROFILE_VERSION_V1,
    RECOMMENDED_PROFILE_VERSION_V2, RECOMMENDED_V2_EVIDENCE_FINGERPRINT,
    RECOMMENDED_V2_EVIDENCE_SUITE, RecommendedAvailability, RecommendedDecision, RecommendedEffort,
    RecommendedProvenance, ResourcePlan, RetryRoute, RoleRoute, RoutingError, RoutingPlan,
    UniformProvider, VERIFY_PROVIDER_ID, recommended_effort, recommended_provenance,
    resolve_config_with_image, unroutable_fix_role,
};
pub use run_service::{
    ApplyOutcome, ExecutionReport, PurgeReceipt, QuiescentState, RunService,
    SKIP_ATTENTION_RESPONSE, StartProgress,
};
