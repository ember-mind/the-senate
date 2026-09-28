use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::domain::{
    ConfigSnapshotId, EffortSetting, ModelId, ProviderId, Role, StageDefinition, StageKind,
    StageRouteOverride, WorkflowDefinition, fix_cycle_stages,
};
use crate::store::ResolvedConfigSnapshot;

/// The provider identity every verifier stage resolves to.
///
/// Verification is deterministic command execution, not agent work, so it
/// has no place in a routing decision: no profile chooses it, no snapshot
/// records it, and no evidence suite ranks it. The router answers for
/// [`Role::Verifier`] itself, which is also what lets a run sealed before the
/// role existed keep loading and keep growing fix cycles.
pub const VERIFY_PROVIDER_ID: &str = "verify";

/// Frozen initial Recommended policy. Preserved verbatim so persisted
/// snapshots created under it keep resolving identically; never re-emitted
/// for new runs.
pub const RECOMMENDED_PROFILE_VERSION_V1: &str = "recommended_v1";
/// Frozen second Recommended policy: the routes `recommended_v3` inherits,
/// under native-default effort. Snapshots created under it keep resolving
/// identically; never re-emitted for new runs.
pub const RECOMMENDED_PROFILE_VERSION_V2: &str = "recommended_v2";
/// Current Recommended policy emitted for new `--profile recommended` runs.
pub const RECOMMENDED_PROFILE_VERSION: &str = "recommended_v3";
const UNIFORM_PROFILE_VERSION: &str = "uniform_v1";
pub(crate) const EVAL_PROFILE_VERSION: &str = "eval_v1";

/// Suite identity backing `recommended_v2` role decisions.
pub const RECOMMENDED_V2_EVIDENCE_SUITE: &str = "role_core_v3";
/// Frozen fingerprint of the evidence suite at decision time.
pub const RECOMMENDED_V2_EVIDENCE_FINGERPRINT: &str =
    "cb9856d2c8edbc4cb0a59520aa140ef4567dce3b650b14f0436d42c4b11c375b";

/// How one role decision in a Recommended profile was justified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionBasis {
    /// Backed by the profile's benchmark evidence at the stated confidence.
    Measured(DecisionConfidence),
    /// Carried over from the previous profile; no current benchmark evidence.
    Inherited,
    /// Expert policy stated ahead of benchmark evidence, to be replaced by a
    /// measured decision or withdrawn once the evidence is in.
    Provisional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionConfidence {
    High,
    Medium,
}

/// One immutable per-role routing decision inside a Recommended profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecommendedDecision {
    pub role: Role,
    pub provider: &'static str,
    pub basis: DecisionBasis,
    /// Factual machine-readable rationale; never a monetary/token-cost claim.
    pub rationale: &'static str,
}

/// One immutable per-role requested-effort decision inside a Recommended
/// profile.
///
/// Stated for the provider the same profile routes the role to. A fallback
/// route (only one native provider ready at creation) still runs at this
/// level, on the other runtime's own scale: the two scales are not
/// comparable, and the profile does not pretend they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecommendedEffort {
    pub role: Role,
    pub effort: EffortSetting,
    pub basis: DecisionBasis,
    /// Factual machine-readable rationale; never a monetary/token-cost claim.
    pub rationale: &'static str,
}

/// Immutable provenance for one versioned Recommended profile.
///
/// `benchmark_kind: "native_runtime"` means targets were whole native
/// runtimes (`provider / native_default`) which may orchestrate multiple
/// models/subagents internally — not single-model comparisons. Latency
/// evidence is runtime-level (suite median), not per-role isolated.
///
/// `efforts` is empty for the profiles that predate effort policy: they
/// state no level, so every role runs at the runtime's native default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecommendedProvenance {
    pub profile_version: &'static str,
    pub evidence_suite: Option<&'static str>,
    pub evidence_fingerprint: Option<&'static str>,
    pub repetitions_per_case: Option<u32>,
    pub targets: &'static [&'static str],
    pub benchmark_kind: &'static str,
    pub decisions: &'static [RecommendedDecision],
    pub efforts: &'static [RecommendedEffort],
}

const RECOMMENDED_V1_PROVENANCE: RecommendedProvenance = RecommendedProvenance {
    profile_version: RECOMMENDED_PROFILE_VERSION_V1,
    evidence_suite: None,
    evidence_fingerprint: None,
    repetitions_per_case: None,
    targets: &[],
    benchmark_kind: "expert_provisional",
    decisions: &[
        RecommendedDecision {
            role: Role::Researcher,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "initial_provisional_policy",
        },
        RecommendedDecision {
            role: Role::Architect,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "initial_provisional_policy",
        },
        RecommendedDecision {
            role: Role::Implementer,
            provider: "codex",
            basis: DecisionBasis::Inherited,
            rationale: "initial_provisional_policy",
        },
        RecommendedDecision {
            role: Role::CodeQualityReviewer,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "initial_provisional_policy",
        },
        RecommendedDecision {
            role: Role::SpecReviewer,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "initial_provisional_policy",
        },
        RecommendedDecision {
            role: Role::Reviewer,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "initial_provisional_policy",
        },
        RecommendedDecision {
            role: Role::EngineeringLead,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "initial_provisional_policy",
        },
    ],
    efforts: &[],
};

const RECOMMENDED_V2_PROVENANCE: RecommendedProvenance = RecommendedProvenance {
    profile_version: RECOMMENDED_PROFILE_VERSION_V2,
    evidence_suite: Some(RECOMMENDED_V2_EVIDENCE_SUITE),
    evidence_fingerprint: Some(RECOMMENDED_V2_EVIDENCE_FINGERPRINT),
    repetitions_per_case: Some(3),
    targets: &["claude/native_default", "codex/native_default"],
    benchmark_kind: "native_runtime",
    decisions: &[
        RecommendedDecision {
            role: Role::Implementer,
            provider: "codex",
            basis: DecisionBasis::Measured(DecisionConfidence::High),
            rationale: "equivalent_measured_correctness_lower_observed_runtime_latency",
        },
        RecommendedDecision {
            role: Role::CodeQualityReviewer,
            provider: "claude",
            basis: DecisionBasis::Measured(DecisionConfidence::Medium),
            rationale: "higher_measured_defect_recall_accepting_non_must_fix_fp_noise",
        },
        RecommendedDecision {
            role: Role::SpecReviewer,
            provider: "codex",
            basis: DecisionBasis::Measured(DecisionConfidence::Medium),
            rationale: "equivalent_measured_correctness_lower_observed_runtime_latency",
        },
        RecommendedDecision {
            role: Role::Researcher,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "no_role_core_v3_evidence_inherited_from_recommended_v1",
        },
        RecommendedDecision {
            role: Role::Architect,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "no_role_core_v3_evidence_inherited_from_recommended_v1",
        },
        RecommendedDecision {
            role: Role::EngineeringLead,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "no_role_core_v3_evidence_inherited_from_recommended_v1",
        },
        RecommendedDecision {
            role: Role::Reviewer,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "legacy_compatibility_route_inherited_from_recommended_v1",
        },
        RecommendedDecision {
            role: Role::Simplifier,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "new_role_no_measured_evidence_provisional_policy",
        },
    ],
    efforts: &[],
};

/// The first profile to state effort per role: routes inherited from
/// `recommended_v2` unchanged, plus a requested level for every role, so
/// reasoning roles think at `high`, the implementer executes an explicit
/// plan at `medium`, and the simplifier reduces at `low`.
///
/// Every effort row is `Provisional`: stated ahead of an effort sweep, on
/// the strength of the role contracts rather than measured evidence. The
/// route rows are `Inherited` rather than restated as measured, because the
/// v2 measurements were taken at native-default effort and this profile no
/// longer runs the implementer there.
const RECOMMENDED_V3_PROVENANCE: RecommendedProvenance = RecommendedProvenance {
    profile_version: RECOMMENDED_PROFILE_VERSION,
    evidence_suite: None,
    evidence_fingerprint: None,
    repetitions_per_case: None,
    targets: &[],
    benchmark_kind: "expert_provisional",
    decisions: &[
        RecommendedDecision {
            role: Role::Implementer,
            provider: "codex",
            basis: DecisionBasis::Inherited,
            rationale: "route_inherited_from_recommended_v2_measured_at_native_default_effort",
        },
        RecommendedDecision {
            role: Role::CodeQualityReviewer,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "route_inherited_from_recommended_v2_measured_at_native_default_effort",
        },
        RecommendedDecision {
            role: Role::SpecReviewer,
            provider: "codex",
            basis: DecisionBasis::Inherited,
            rationale: "route_inherited_from_recommended_v2_measured_at_native_default_effort",
        },
        RecommendedDecision {
            role: Role::Researcher,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "route_inherited_from_recommended_v2",
        },
        RecommendedDecision {
            role: Role::Architect,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "route_inherited_from_recommended_v2",
        },
        RecommendedDecision {
            role: Role::EngineeringLead,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "route_inherited_from_recommended_v2",
        },
        RecommendedDecision {
            role: Role::Reviewer,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "legacy_compatibility_route_inherited_from_recommended_v2",
        },
        RecommendedDecision {
            role: Role::Simplifier,
            provider: "claude",
            basis: DecisionBasis::Inherited,
            rationale: "route_inherited_from_recommended_v2",
        },
    ],
    efforts: &[
        RecommendedEffort {
            role: Role::Researcher,
            effort: EffortSetting::HIGH,
            basis: DecisionBasis::Provisional,
            rationale: "reasoning_role_inspects_the_repository_ahead_of_design",
        },
        RecommendedEffort {
            role: Role::Architect,
            effort: EffortSetting::HIGH,
            basis: DecisionBasis::Provisional,
            rationale: "planning_role_removes_uncertainty_ahead_of_lower_effort_execution",
        },
        RecommendedEffort {
            role: Role::Implementer,
            effort: EffortSetting::MEDIUM,
            basis: DecisionBasis::Provisional,
            rationale: "executes_an_explicit_plan_pending_effort_sweep",
        },
        RecommendedEffort {
            role: Role::Simplifier,
            effort: EffortSetting::LOW,
            basis: DecisionBasis::Provisional,
            rationale: "contract_only_reduces_within_the_diff_and_never_improves",
        },
        RecommendedEffort {
            role: Role::CodeQualityReviewer,
            effort: EffortSetting::HIGH,
            basis: DecisionBasis::Provisional,
            rationale: "independent_review_guards_lower_effort_execution",
        },
        RecommendedEffort {
            role: Role::SpecReviewer,
            effort: EffortSetting::HIGH,
            basis: DecisionBasis::Provisional,
            rationale: "independent_review_guards_lower_effort_execution",
        },
        RecommendedEffort {
            role: Role::EngineeringLead,
            effort: EffortSetting::HIGH,
            basis: DecisionBasis::Provisional,
            rationale: "decides_over_conflicting_review_evidence",
        },
        RecommendedEffort {
            role: Role::Reviewer,
            effort: EffortSetting::HIGH,
            basis: DecisionBasis::Provisional,
            rationale: "legacy_review_role_held_to_the_reviewer_level",
        },
    ],
};

/// Immutable provenance for a known Recommended profile version.
#[must_use]
pub fn recommended_provenance(profile_version: &str) -> Option<&'static RecommendedProvenance> {
    match profile_version {
        RECOMMENDED_PROFILE_VERSION_V1 => Some(&RECOMMENDED_V1_PROVENANCE),
        RECOMMENDED_PROFILE_VERSION_V2 => Some(&RECOMMENDED_V2_PROVENANCE),
        RECOMMENDED_PROFILE_VERSION => Some(&RECOMMENDED_V3_PROVENANCE),
        _ => None,
    }
}

/// The current Recommended profile's requested effort for one role:
/// `NativeDefault` for any role the profile states nothing about.
#[must_use]
pub fn recommended_effort(role: Role) -> EffortSetting {
    RECOMMENDED_V3_PROVENANCE
        .efforts
        .iter()
        .find(|decision| decision.role == role)
        .map_or(EffortSetting::NativeDefault, |decision| decision.effort)
}

/// One immutable provider/model destination selected for a role.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExecutionTarget {
    provider_id: ProviderId,
    model_id: Option<ModelId>,
}

impl ExecutionTarget {
    #[must_use]
    pub const fn new(provider_id: ProviderId, model_id: Option<ModelId>) -> Self {
        Self {
            provider_id,
            model_id,
        }
    }

    #[must_use]
    pub const fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    #[must_use]
    pub const fn model_id(&self) -> Option<&ModelId> {
        self.model_id.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleRoute {
    target: ExecutionTarget,
    reason: String,
}

impl RoleRoute {
    #[must_use]
    pub const fn target(&self) -> &ExecutionTarget {
        &self.target
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Validated immutable role routing reconstructed from one configuration snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingPlan {
    profile: String,
    profile_version: String,
    role_routes: HashMap<Role, RoleRoute>,
    provider_configs: HashMap<String, ProviderConfig>,
}

impl RoutingPlan {
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub fn profile_version(&self) -> &str {
        &self.profile_version
    }

    /// The configured destination for one role.
    ///
    /// [`Role::Verifier`] is answered without consulting the snapshot — see
    /// [`VERIFY_PROVIDER_ID`] — so it is absent from [`Self::routes`], which
    /// reports only what the snapshot actually decided.
    #[must_use]
    pub fn route(&self, role: Role) -> Option<&RoleRoute> {
        if role == Role::Verifier {
            return Some(verifier_route());
        }
        self.role_routes.get(&role)
    }

    pub fn routes(&self) -> impl Iterator<Item = (Role, &RoleRoute)> {
        self.role_routes.iter().map(|(role, route)| (*role, route))
    }

    #[must_use]
    pub fn provider_config(&self, provider_id: &ProviderId) -> Option<&ProviderConfig> {
        self.provider_configs.get(provider_id.as_str())
    }

    #[cfg(test)]
    pub(crate) fn test_plan(routes: HashMap<Role, ExecutionTarget>) -> Self {
        let provider_configs = routes
            .values()
            .map(|target| target.provider_id().as_str())
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|provider| {
                let dto = provider_config_dto(provider).unwrap();
                (
                    provider.to_owned(),
                    ProviderConfig {
                        profile: dto.profile,
                        schema_version: dto.schema_version,
                        provider_options: dto.provider_options,
                        scenario: dto.scenario,
                    },
                )
            })
            .collect();
        Self {
            profile: "test".to_owned(),
            profile_version: "test_v1".to_owned(),
            role_routes: routes
                .into_iter()
                .map(|(role, target)| {
                    (
                        role,
                        RoleRoute {
                            target,
                            reason: "test_route".to_owned(),
                        },
                    )
                })
                .collect(),
            provider_configs,
        }
    }

    /// Decodes config v2 or normalizes a legacy uniform v1 config in memory.
    ///
    /// # Errors
    /// Rejects malformed, unsafe, unsupported, or incomplete routing configuration.
    pub fn from_snapshot(
        snapshot: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
    ) -> Result<Self, RoutingError> {
        match snapshot.schema_version() {
            1 => decode_legacy(snapshot, workflow),
            2..=4 => decode_v2(snapshot, workflow),
            version => Err(RoutingError::UnsupportedSchema(version)),
        }
    }
}

/// Validated immutable per-role requested effort reconstructed from one
/// configuration snapshot. Separate from `RoutingPlan`: routing answers the
/// destination, the resource plan answers requested native-runtime effort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourcePlan {
    role_efforts: HashMap<Role, EffortSetting>,
}

impl ResourcePlan {
    /// Requested effort for one routed role.
    ///
    /// The verifier runs commands, which have no effort dial; it is always
    /// native default and never stated in a snapshot, so a v3 plan sealed
    /// without it is complete.
    #[must_use]
    pub fn effort(&self, role: Role) -> Option<EffortSetting> {
        if role == Role::Verifier {
            return Some(EffortSetting::NativeDefault);
        }
        self.role_efforts.get(&role).copied()
    }

    pub fn efforts(&self) -> impl Iterator<Item = (Role, EffortSetting)> + '_ {
        self.role_efforts
            .iter()
            .map(|(role, effort)| (*role, *effort))
    }

    /// Reconstructs the immutable resource plan from a persisted snapshot.
    ///
    /// Schema v1 and v2 predate effort policy and always decode to
    /// `NativeDefault` for every routable role — never `Medium` — so no old
    /// run changes native runtime behavior. Routable, not just required: a
    /// completed run can grow a fix cycle, and driving those stages asks for
    /// their effort exactly like it asks for their route. Schema v3 requires
    /// an explicit setting for at least the workflow's required roles and
    /// nothing beyond its routable ones; anything malformed fails closed.
    ///
    /// # Errors
    /// Rejects malformed payloads, unknown settings, or incomplete coverage.
    pub fn from_snapshot(
        snapshot: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
    ) -> Result<Self, RoutingError> {
        match snapshot.schema_version() {
            1 | 2 => Ok(Self {
                role_efforts: routable_roles(workflow)
                    .into_iter()
                    .map(|role| (role, EffortSetting::NativeDefault))
                    .collect(),
            }),
            3 => {
                let payload: RoutingPayloadV2 = serde_json::from_value(snapshot.payload().clone())?;
                validate_resource_plan_shape(&payload, workflow)?;
                let role_efforts = payload.resource_plan.ok_or_else(|| {
                    RoutingError::InvalidConfig("schema v3 requires a resource plan".to_owned())
                })?;
                Ok(Self { role_efforts })
            }
            // Schema v4 exists for image generation, not effort: a resource
            // plan is present exactly when effort was explicit, and its
            // absence means NativeDefault just as v2 did.
            4 => {
                let payload: RoutingPayloadV2 = serde_json::from_value(snapshot.payload().clone())?;
                validate_resource_plan_shape(&payload, workflow)?;
                Ok(Self {
                    role_efforts: payload.resource_plan.unwrap_or_else(|| {
                        routable_roles(workflow)
                            .into_iter()
                            .map(|role| (role, EffortSetting::NativeDefault))
                            .collect()
                    }),
                })
            }
            version => Err(RoutingError::UnsupportedSchema(version)),
        }
    }
}

/// Default per-run image generation bound. Four is enough for a hero image
/// plus a retry or two of it, and small enough that an agent looping on a
/// prompt cannot run up a bill: authorization is never unlimited.
pub const DEFAULT_MAX_IMAGE_GENERATIONS: u32 = 4;
/// Hard ceiling any persisted plan may declare.
pub const MAX_IMAGE_GENERATIONS_CEILING: u32 = 32;

/// Validated immutable image-generation authorization reconstructed from one
/// configuration snapshot: which roles may call the tool and how many times
/// the run may generate. Separate from `RoutingPlan` (destination) and
/// `ResourcePlan` (effort): this plan answers what additional tool a role may
/// use. It names no backend and no model; the backend owns those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageGenerationPlan {
    roles: BTreeSet<Role>,
    max_generations: u32,
}

impl ImageGenerationPlan {
    /// No role may generate images. What every run gets unless asked.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            roles: BTreeSet::new(),
            max_generations: 0,
        }
    }

    /// The v1 grant: the Implementer alone, with the default bound.
    #[must_use]
    pub fn implementer_only() -> Self {
        Self {
            roles: BTreeSet::from([Role::Implementer]),
            max_generations: DEFAULT_MAX_IMAGE_GENERATIONS,
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.roles.is_empty()
    }

    #[must_use]
    pub fn allows(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    #[must_use]
    pub const fn max_generations(&self) -> u32 {
        self.max_generations
    }

    pub fn roles(&self) -> impl Iterator<Item = Role> + '_ {
        self.roles.iter().copied()
    }

    /// Reconstructs the immutable plan from a persisted snapshot.
    ///
    /// Schema v1–v3 predate the tool and always decode disabled. Schema v4
    /// requires an explicit `image_generation` block; a block under an older
    /// schema, an unknown role, a role outside the workflow, or a bound
    /// outside `1..=MAX_IMAGE_GENERATIONS_CEILING` fails closed. Resume never
    /// consults the current CLI flags.
    ///
    /// # Errors
    /// Rejects malformed or out-of-range image generation configuration.
    pub fn from_snapshot(
        snapshot: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
    ) -> Result<Self, RoutingError> {
        match snapshot.schema_version() {
            1 => Ok(Self::disabled()),
            2..=4 => {
                let payload: RoutingPayloadV2 = serde_json::from_value(snapshot.payload().clone())?;
                validate_image_generation_shape(&payload, workflow)?;
                Ok(payload
                    .image_generation
                    .map_or_else(Self::disabled, |dto| Self {
                        roles: dto.roles.into_iter().collect(),
                        max_generations: dto.max_generations,
                    }))
            }
            version => Err(RoutingError::UnsupportedSchema(version)),
        }
    }
}

/// Structural image-generation rules shared by every decoder: nothing
/// before v4 may carry the block, v4 must, and what it carries must be a
/// non-empty set of the workflow's routable roles with a sane bound.
fn validate_image_generation_shape(
    payload: &RoutingPayloadV2,
    workflow: &WorkflowDefinition,
) -> Result<(), RoutingError> {
    match (payload.schema_version, payload.image_generation.as_ref()) {
        (1..=3, None) => Ok(()),
        (1..=3, Some(_)) => Err(RoutingError::InvalidConfig(
            "image generation requires config schema 4".to_owned(),
        )),
        (4, None) => Err(RoutingError::InvalidConfig(
            "schema v4 requires an image generation block".to_owned(),
        )),
        (4, Some(dto)) => {
            let routable = routable_roles(workflow);
            let distinct = dto.roles.iter().collect::<BTreeSet<_>>();
            if dto.roles.is_empty()
                || distinct.len() != dto.roles.len()
                || !dto.roles.iter().all(|role| routable.contains(role))
            {
                return Err(RoutingError::InvalidConfig(
                    "image generation roles must be distinct routable workflow roles".to_owned(),
                ));
            }
            if !(1..=MAX_IMAGE_GENERATIONS_CEILING).contains(&dto.max_generations) {
                return Err(RoutingError::InvalidConfig(format!(
                    "image generation bound must be within 1..={MAX_IMAGE_GENERATIONS_CEILING}"
                )));
            }
            Ok(())
        }
        _ => Err(RoutingError::InvalidConfig(
            "unsupported payload schema for image generation".to_owned(),
        )),
    }
}

/// Structural effort rules shared by routing and resource decoding: v2 must
/// not smuggle a resource plan; v3 must cover every required role and stay
/// within the routable ones.
///
/// A window between the two bounds exists because v3 payloads sealed before
/// fix-cycle routing cover only the roles their workflow starts with. Those
/// snapshots are never rewritten, so they must keep decoding; what they
/// cannot do is execute a fix cycle, which [`unroutable_fix_role`] refuses
/// before one is committed.
fn validate_resource_plan_shape(
    payload: &RoutingPayloadV2,
    workflow: &WorkflowDefinition,
) -> Result<(), RoutingError> {
    match (payload.schema_version, payload.resource_plan.as_ref()) {
        (1 | 2 | 4, None) => Ok(()),
        (2, Some(_)) => Err(RoutingError::InvalidConfig(
            "resource plan requires config schema 3".to_owned(),
        )),
        (3, Some(plan)) => {
            let required = required_roles(workflow);
            let routable = routable_roles(workflow);
            if !required.iter().all(|role| plan.contains_key(role))
                || !plan.keys().all(|role| routable.contains(role))
            {
                return Err(RoutingError::InvalidConfig(
                    "resource plan must cover the workflow's required roles and stay within its routable roles"
                        .to_owned(),
                ));
            }
            Ok(())
        }
        (3, None) => Err(RoutingError::InvalidConfig(
            "schema v3 requires a resource plan".to_owned(),
        )),
        (4, Some(plan)) => {
            let required = required_roles(workflow);
            let routable = routable_roles(workflow);
            if !required.iter().all(|role| plan.contains_key(role))
                || !plan.keys().all(|role| routable.contains(role))
            {
                return Err(RoutingError::InvalidConfig(
                    "resource plan must cover the workflow's required roles and stay within its routable roles"
                        .to_owned(),
                ));
            }
            Ok(())
        }
        _ => Err(RoutingError::InvalidConfig(
            "unsupported payload schema for resource plan".to_owned(),
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderConfig {
    profile: String,
    schema_version: u32,
    provider_options: Value,
    scenario: Option<String>,
}

impl ProviderConfig {
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    #[must_use]
    pub const fn provider_options(&self) -> &Value {
        &self.provider_options
    }

    #[must_use]
    pub fn scenario(&self) -> Option<&str> {
        self.scenario.as_deref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UniformProvider {
    Claude,
    Codex,
    Fake,
}

impl UniformProvider {
    /// The identity leaf runtimes and events use for this provider.
    #[must_use]
    pub fn provider_id(self) -> ProviderId {
        // Every variant's name is a non-empty, whitespace-free literal.
        ProviderId::new(self.as_str()).unwrap_or_else(|_| unreachable!())
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Fake => "fake",
        }
    }
}

impl TryFrom<&str> for UniformProvider {
    type Error = RoutingError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "fake" => Ok(Self::Fake),
            other => Err(RoutingError::UnsupportedProvider(other.to_owned())),
        }
    }
}

/// Reason recorded on every operator-chosen route override.
pub const OPERATOR_OVERRIDE_REASON: &str = "operator_override";

/// Where an operator sends one failed stage on retry.
///
/// Distinct from [`ExecutionSelection`], which decides a whole run once at
/// creation: this names one provider for one stage, after the fact, and
/// leaves the snapshot alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryRoute {
    provider: UniformProvider,
    model: Option<ModelId>,
}

impl RetryRoute {
    #[must_use]
    pub const fn new(provider: UniformProvider, model: Option<ModelId>) -> Self {
        Self { provider, model }
    }

    #[must_use]
    pub const fn provider(&self) -> UniformProvider {
        self.provider
    }

    #[must_use]
    pub const fn model(&self) -> Option<&ModelId> {
        self.model.as_ref()
    }

    /// The stage-level record of this choice.
    #[must_use]
    pub fn into_override(self) -> StageRouteOverride {
        StageRouteOverride::new(self.provider.provider_id(), self.model)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionSelection {
    Uniform(UniformProvider),
    Recommended,
}

/// How a new run asks for effort, before its per-role plan is sealed.
///
/// `ProfileDefault` takes each role's level from the routing profile: the
/// current Recommended profile states one per role, and a uniform
/// `--provider` run has no profile policy, so it stays native. `Uniform` is
/// the operator overriding every role with one level — `NativeDefault`
/// included, which is how a Recommended run opts out of the profile's
/// policy. `PerRole` names some roles and leaves the rest to the profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EffortRequest {
    #[default]
    ProfileDefault,
    Uniform(EffortSetting),
    PerRole(HashMap<Role, EffortSetting>),
}

impl From<EffortSetting> for EffortRequest {
    fn from(setting: EffortSetting) -> Self {
        Self::Uniform(setting)
    }
}

/// `None` is the profile's own policy; `Some` overrides every role.
impl From<Option<EffortSetting>> for EffortRequest {
    fn from(setting: Option<EffortSetting>) -> Self {
        setting.map_or(Self::ProfileDefault, Self::Uniform)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecommendedAvailability {
    pub claude: bool,
    pub codex: bool,
}

/// Resolves a selection to explicit routes and seals requested effort per
/// role, encoding immutable config schema v2 (every role native) or v3.
///
/// # Errors
/// Rejects unavailable Recommended, invalid identifiers/configuration, or an
/// effort request naming a role the workflow cannot route.
pub fn resolve_config(
    selection: ExecutionSelection,
    effort: impl Into<EffortRequest>,
    workflow: &WorkflowDefinition,
    availability: RecommendedAvailability,
    id: ConfigSnapshotId,
    created_at: DateTime<Utc>,
) -> Result<ResolvedConfigSnapshot, RoutingError> {
    resolve_config_with_image(
        selection,
        effort,
        &ImageGenerationPlan::disabled(),
        workflow,
        availability,
        id,
        created_at,
    )
}

/// [`resolve_config`] plus an image-generation grant. A disabled grant emits
/// the byte-identical v2/v3 payload; an enabled one seals schema v4.
///
/// # Errors
/// Rejects unavailable Recommended, invalid identifiers/configuration, an
/// effort request naming a role the workflow cannot route, or a grant naming
/// a role it cannot route.
pub fn resolve_config_with_image(
    selection: ExecutionSelection,
    effort: impl Into<EffortRequest>,
    image: &ImageGenerationPlan,
    workflow: &WorkflowDefinition,
    availability: RecommendedAvailability,
    id: ConfigSnapshotId,
    created_at: DateTime<Utc>,
) -> Result<ResolvedConfigSnapshot, RoutingError> {
    let effort = effort.into();
    let roles = routable_roles(workflow);
    let (profile, profile_version, routes) = match selection {
        ExecutionSelection::Uniform(provider) => {
            let routes = roles
                .into_iter()
                .map(|role| {
                    (
                        role,
                        RouteDto {
                            provider: provider.as_str().to_owned(),
                            model: None,
                            reason: "explicit_provider".to_owned(),
                        },
                    )
                })
                .collect();
            ("uniform", UNIFORM_PROFILE_VERSION, routes)
        }
        ExecutionSelection::Recommended => {
            let routes = recommended_routes(roles, availability)?;
            ("recommended", RECOMMENDED_PROFILE_VERSION, routes)
        }
    };
    let used = routes
        .values()
        .map(|route| route.provider.as_str())
        .collect::<HashSet<_>>();
    let providers = used
        .into_iter()
        .map(|provider| Ok((provider.to_owned(), provider_config_dto(provider)?)))
        .collect::<Result<HashMap<_, _>, RoutingError>>()?;
    // A profile states effort only under Recommended; a uniform provider has
    // no policy of its own and stays native unless the operator says
    // otherwise.
    let profile_effort = |role: Role| match selection {
        ExecutionSelection::Recommended => recommended_effort(role),
        ExecutionSelection::Uniform(_) => EffortSetting::NativeDefault,
    };
    let efforts = match &effort {
        EffortRequest::ProfileDefault => routes
            .keys()
            .map(|role| (*role, profile_effort(*role)))
            .collect::<HashMap<_, _>>(),
        EffortRequest::Uniform(setting) => routes
            .keys()
            .map(|role| (*role, *setting))
            .collect::<HashMap<_, _>>(),
        EffortRequest::PerRole(named) => {
            if let Some(alien) = named.keys().find(|role| !routes.contains_key(role)) {
                return Err(RoutingError::EffortRoleUnroutable(*alien));
            }
            routes
                .keys()
                .map(|role| {
                    (
                        *role,
                        named
                            .get(role)
                            .copied()
                            .unwrap_or_else(|| profile_effort(*role)),
                    )
                })
                .collect::<HashMap<_, _>>()
        }
    };
    // Every role native keeps the exact pre-effort schema-v2 payload; any
    // explicit level persists the whole per-role resource plan under schema
    // v3. Old payloads are never rewritten either way.
    let (mut schema_version, resource_plan) = if efforts
        .values()
        .all(|setting| *setting == EffortSetting::NativeDefault)
    {
        (2, None)
    } else {
        (3, Some(efforts))
    };
    // An image grant is the only thing that seals schema v4; without one the
    // payload stays whatever effort alone would have produced.
    let image_generation = image.is_enabled().then(|| {
        schema_version = 4;
        ImageGenerationDto {
            roles: image.roles().collect(),
            max_generations: image.max_generations(),
        }
    });
    let payload = RoutingPayloadV2 {
        schema_version,
        profile: profile.to_owned(),
        profile_version: profile_version.to_owned(),
        routes,
        providers,
        resource_plan,
        image_generation,
    };
    let snapshot = ResolvedConfigSnapshot::new(
        id,
        schema_version,
        serde_json::to_value(payload)?,
        created_at,
    )
    .map_err(|error| RoutingError::InvalidConfig(error.to_string()))?;
    RoutingPlan::from_snapshot(&snapshot, workflow)?;
    ResourcePlan::from_snapshot(&snapshot, workflow)?;
    ImageGenerationPlan::from_snapshot(&snapshot, workflow)?;
    Ok(snapshot)
}

/// Encodes one isolated evaluation route while keeping support roles synthetic.
///
/// This profile is not exposed by normal run creation or Recommended routing.
pub(crate) fn resolve_eval_config(
    target_role: Role,
    target: &ExecutionTarget,
    effort: EffortSetting,
    workflow: &WorkflowDefinition,
    id: ConfigSnapshotId,
    created_at: DateTime<Utc>,
) -> Result<ResolvedConfigSnapshot, RoutingError> {
    let roles = required_roles(workflow);
    if !roles.contains(&target_role) {
        return Err(RoutingError::EvaluationRoleAbsent(target_role));
    }
    // The candidate is measured at the requested level; the Fake support
    // roles have no dial, but a v3 plan must still cover every required
    // role, so they carry the same setting.
    let (schema_version, resource_plan) = match effort {
        EffortSetting::NativeDefault => (2, None),
        explicit @ EffortSetting::Level(_) => (
            3,
            Some(
                roles
                    .iter()
                    .map(|role| (*role, explicit))
                    .collect::<HashMap<_, _>>(),
            ),
        ),
    };
    let routes = roles
        .into_iter()
        .map(|role| {
            let (target, reason) = if role == target_role {
                (target.clone(), "eval_candidate")
            } else {
                (
                    ExecutionTarget::new(
                        ProviderId::new("fake").expect("static provider ID is valid"),
                        None,
                    ),
                    "eval_support",
                )
            };
            (
                role,
                RouteDto {
                    provider: target.provider_id().to_string(),
                    model: target.model_id().map(ToString::to_string),
                    reason: reason.to_owned(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let providers = routes
        .values()
        .map(|route| route.provider.as_str())
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|provider| Ok((provider.to_owned(), provider_config_dto(provider)?)))
        .collect::<Result<HashMap<_, _>, RoutingError>>()?;
    let payload = RoutingPayloadV2 {
        schema_version,
        profile: "eval".to_owned(),
        profile_version: EVAL_PROFILE_VERSION.to_owned(),
        routes,
        providers,
        resource_plan,
        image_generation: None,
    };
    let snapshot = ResolvedConfigSnapshot::new(
        id,
        schema_version,
        serde_json::to_value(payload)?,
        created_at,
    )
    .map_err(|error| RoutingError::InvalidConfig(error.to_string()))?;
    RoutingPlan::from_snapshot(&snapshot, workflow)?;
    ResourcePlan::from_snapshot(&snapshot, workflow)?;
    Ok(snapshot)
}

fn recommended_routes(
    roles: HashSet<Role>,
    availability: RecommendedAvailability,
) -> Result<HashMap<Role, RouteDto>, RoutingError> {
    let fallback = match availability {
        RecommendedAvailability {
            claude: true,
            codex: false,
        } => Some("claude"),
        RecommendedAvailability {
            claude: false,
            codex: true,
        } => Some("codex"),
        RecommendedAvailability {
            claude: false,
            codex: false,
        } => return Err(RoutingError::RecommendedUnavailable),
        RecommendedAvailability {
            claude: true,
            codex: true,
        } => None,
    };
    Ok(roles
        .into_iter()
        .map(|role| {
            let (provider, reason) = if let Some(provider) = fallback {
                (
                    provider,
                    "fallback_preferred_provider_unavailable_at_run_creation",
                )
            } else {
                let decision = RECOMMENDED_V3_PROVENANCE
                    .decisions
                    .iter()
                    .find(|decision| decision.role == role)
                    .expect("recommended_v3 provenance covers every role");
                (decision.provider, decision.rationale)
            };
            (
                role,
                RouteDto {
                    provider: provider.to_owned(),
                    model: None,
                    reason: reason.to_owned(),
                },
            )
        })
        .collect())
}

/// The route every verifier stage takes, built once. Static rather than a
/// field so a snapshot decoded before the role existed carries it too.
fn verifier_route() -> &'static RoleRoute {
    static ROUTE: LazyLock<RoleRoute> = LazyLock::new(|| RoleRoute {
        target: ExecutionTarget::new(
            ProviderId::new(VERIFY_PROVIDER_ID).expect("static provider ID is valid"),
            None,
        ),
        reason: "implicit_verify_provider".to_owned(),
    });
    &ROUTE
}

/// Whether a role's route is decided by the configuration snapshot at all.
/// Only the verifier is not; see [`VERIFY_PROVIDER_ID`].
const fn routed_by_snapshot(role: Role) -> bool {
    !matches!(role, Role::Verifier)
}

fn required_roles(workflow: &WorkflowDefinition) -> HashSet<Role> {
    workflow
        .stages()
        .iter()
        .map(crate::domain::StageDefinition::role)
        .filter(|role| routed_by_snapshot(*role))
        .collect()
}

/// The roles a fix cycle would add to this workflow, if it can grow one.
///
/// Derived from [`fix_cycle_stages`] rather than named here, so the two cannot
/// drift: whatever the cycle is made of is what has to be routable.
fn fix_cycle_roles(workflow: &WorkflowDefinition) -> HashSet<Role> {
    workflow
        .stages()
        .iter()
        .rev()
        .find(|stage| stage.kind() == StageKind::Decision)
        .map(|decision| {
            fix_cycle_stages(1, decision.id())
                .iter()
                .map(StageDefinition::role)
                .filter(|role| routed_by_snapshot(*role))
                .collect()
        })
        .unwrap_or_default()
}

/// Every role this run's configuration has to answer for, including the ones
/// it does not use yet.
///
/// Configuration is sealed at creation and never re-resolved, but a completed
/// run that reached a verdict can still grow a fix cycle, and those stages
/// arrive long after the sealing. Resolving only the roles a workflow starts
/// with left a review — the one workflow whose entire output is a list of
/// things to fix — with no route for the role that would fix them.
///
/// Deliberately not what [`validate_required_routes`] checks. That question is
/// about the stages a run actually has, and widening it would reject every
/// configuration written before this one.
fn routable_roles(workflow: &WorkflowDefinition) -> HashSet<Role> {
    let mut roles = required_roles(workflow);
    roles.extend(fix_cycle_roles(workflow));
    roles
}

/// The first fix-cycle role this sealed configuration cannot execute, if any:
/// a role it cannot route, or one it states no requested effort for.
///
/// Asked before a fix is committed rather than discovered while driving one.
/// The request appends stages and gives the workspace a branch, so a run whose
/// configuration predates fix-cycle routing would otherwise be left carrying
/// stages nothing can execute — and unreadable, because reading a run resolves
/// its routes. Effort is held to the same bar as the route: a v3 snapshot
/// sealed before fix-cycle routing states effort only for the roles its
/// workflow started with, and inventing a level for the fix would re-resolve
/// what was sealed.
///
/// # Errors
/// Returns the decoding failures of [`RoutingPlan::from_snapshot`] and
/// [`ResourcePlan::from_snapshot`].
pub fn unroutable_fix_role(
    snapshot: &ResolvedConfigSnapshot,
    workflow: &WorkflowDefinition,
) -> Result<Option<Role>, RoutingError> {
    let plan = RoutingPlan::from_snapshot(snapshot, workflow)?;
    let efforts = ResourcePlan::from_snapshot(snapshot, workflow)?;
    let mut missing = fix_cycle_roles(workflow)
        .into_iter()
        .filter(|role| plan.route(*role).is_none() || efforts.effort(*role).is_none())
        .collect::<Vec<_>>();
    missing.sort_by_key(|role| format!("{role:?}"));
    Ok(missing.into_iter().next())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutingPayloadV2 {
    schema_version: u32,
    profile: String,
    profile_version: String,
    routes: HashMap<Role, RouteDto>,
    providers: HashMap<String, ProviderConfigDto>,
    /// Schema v3 only: per-role requested effort. Absent on v2 payloads;
    /// unknown values fail decoding closed instead of degrading silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource_plan: Option<HashMap<Role, EffortSetting>>,
    /// Schema v4 only: which roles may call the image tool and the per-run
    /// bound. Absent on v2/v3 payloads; unknown roles fail decoding closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_generation: Option<ImageGenerationDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageGenerationDto {
    roles: Vec<Role>,
    max_generations: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteDto {
    provider: String,
    model: Option<String>,
    reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderConfigDto {
    profile: String,
    schema_version: u32,
    provider_options: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scenario: Option<String>,
}

/// Every profile identity a v2/v3 payload may carry: the frozen Recommended
/// versions decode alongside the current one, so no sealed run ever stops
/// loading when the policy moves on.
fn known_profile(profile: &str, version: &str) -> bool {
    matches!(
        (profile, version),
        ("uniform", UNIFORM_PROFILE_VERSION)
            | (
                "recommended",
                RECOMMENDED_PROFILE_VERSION_V1
                    | RECOMMENDED_PROFILE_VERSION_V2
                    | RECOMMENDED_PROFILE_VERSION
            )
            | ("eval", EVAL_PROFILE_VERSION)
    )
}

fn decode_v2(
    snapshot: &ResolvedConfigSnapshot,
    workflow: &WorkflowDefinition,
) -> Result<RoutingPlan, RoutingError> {
    let payload: RoutingPayloadV2 = serde_json::from_value(snapshot.payload().clone())?;
    if payload.schema_version != snapshot.schema_version() {
        return Err(RoutingError::InvalidConfig(
            "payload schema_version must match snapshot schema".to_owned(),
        ));
    }
    validate_resource_plan_shape(&payload, workflow)?;
    validate_image_generation_shape(&payload, workflow)?;
    if !known_profile(&payload.profile, &payload.profile_version) {
        return Err(RoutingError::InvalidProfileMetadata);
    }
    let mut provider_configs = HashMap::new();
    for (provider, config) in payload.providers {
        validate_provider_config(&provider, &config)?;
        provider_configs.insert(
            provider,
            ProviderConfig {
                profile: config.profile,
                schema_version: config.schema_version,
                provider_options: config.provider_options,
                scenario: config.scenario,
            },
        );
    }
    let mut role_routes = HashMap::new();
    for (role, route) in payload.routes {
        let provider_id = ProviderId::new(route.provider.clone())
            .map_err(|error| RoutingError::InvalidConfig(error.to_string()))?;
        if !matches!(provider_id.as_str(), "claude" | "codex" | "fake") {
            return Err(RoutingError::UnsupportedProvider(route.provider));
        }
        if payload.profile == "recommended" && provider_id.as_str() == "fake" {
            return Err(RoutingError::FakeInRecommended);
        }
        if !provider_configs.contains_key(provider_id.as_str()) {
            return Err(RoutingError::MissingProviderConfig(provider_id.to_string()));
        }
        let model_id = route
            .model
            .map(ModelId::new)
            .transpose()
            .map_err(|error| RoutingError::InvalidConfig(error.to_string()))?;
        if route.reason.is_empty() || route.reason.chars().any(char::is_whitespace) {
            return Err(RoutingError::InvalidConfig(
                "route reason must be non-empty machine text".to_owned(),
            ));
        }
        role_routes.insert(
            role,
            RoleRoute {
                target: ExecutionTarget::new(provider_id, model_id),
                reason: route.reason,
            },
        );
    }
    validate_required_routes(workflow, &role_routes)?;
    if payload.profile == "uniform"
        && role_routes
            .values()
            .map(RoleRoute::target)
            .collect::<HashSet<_>>()
            .len()
            != 1
    {
        return Err(RoutingError::InvalidConfig(
            "uniform profile must use one execution target".to_owned(),
        ));
    }
    if payload.profile == "eval" {
        let candidate_count = role_routes
            .values()
            .filter(|route| route.reason() == "eval_candidate")
            .count();
        let support_routes_valid = role_routes.values().all(|route| {
            route.reason() == "eval_candidate"
                || (route.reason() == "eval_support"
                    && route.target().provider_id().as_str() == "fake"
                    && route.target().model_id().is_none())
        });
        if candidate_count != 1
            || !support_routes_valid
            || role_routes.len() != required_roles(workflow).len()
        {
            return Err(RoutingError::InvalidConfig(
                "eval profile requires one candidate route and Fake support routes".to_owned(),
            ));
        }
    }
    Ok(RoutingPlan {
        profile: payload.profile,
        profile_version: payload.profile_version,
        role_routes,
        provider_configs,
    })
}

fn decode_legacy(
    snapshot: &ResolvedConfigSnapshot,
    workflow: &WorkflowDefinition,
) -> Result<RoutingPlan, RoutingError> {
    let payload = snapshot.payload();
    if payload.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(RoutingError::InvalidConfig(
            "legacy payload schema_version must be 1".to_owned(),
        ));
    }
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .ok_or_else(|| RoutingError::InvalidConfig("legacy provider missing".to_owned()))?;
    if matches!(provider, "claude" | "codex")
        && !payload
            .get("model")
            .is_some_and(|model| model.is_null() || model.is_string())
    {
        return Err(RoutingError::InvalidConfig(
            "legacy native model must be null or string".to_owned(),
        ));
    }
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(ModelId::new)
        .transpose()
        .map_err(|error| RoutingError::InvalidConfig(error.to_string()))?;
    let config = legacy_provider_config(provider, payload)?;
    let provider_id = ProviderId::new(provider)
        .map_err(|error| RoutingError::InvalidConfig(error.to_string()))?;
    let target = ExecutionTarget::new(provider_id, model);
    let role_routes = required_roles(workflow)
        .into_iter()
        .map(|role| {
            (
                role,
                RoleRoute {
                    target: target.clone(),
                    reason: "legacy_uniform_config".to_owned(),
                },
            )
        })
        .collect();
    Ok(RoutingPlan {
        profile: payload
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("legacy_uniform")
            .to_owned(),
        profile_version: "legacy_schema_v1".to_owned(),
        role_routes,
        provider_configs: HashMap::from([(provider.to_owned(), config)]),
    })
}

fn legacy_provider_config(provider: &str, payload: &Value) -> Result<ProviderConfig, RoutingError> {
    let dto = match provider {
        "fake"
            if payload.get("profile").and_then(Value::as_str) == Some("development_fake")
                && payload.get("scenario").and_then(Value::as_str)
                    == Some("default_success_v1") =>
        {
            provider_config_dto("fake")?
        }
        "claude"
            if payload.get("profile").and_then(Value::as_str) == Some("native_claude")
                && payload.get("provider_options") == Some(&json!({})) =>
        {
            provider_config_dto("claude")?
        }
        "codex"
            if payload.get("profile").and_then(Value::as_str) == Some("native_codex")
                && payload.get("provider_options") == Some(&codex_options()) =>
        {
            provider_config_dto("codex")?
        }
        "claude" | "codex" | "fake" => {
            return Err(RoutingError::InvalidConfig(
                "unsupported legacy provider configuration".to_owned(),
            ));
        }
        other => return Err(RoutingError::UnsupportedProvider(other.to_owned())),
    };
    Ok(ProviderConfig {
        profile: dto.profile,
        schema_version: dto.schema_version,
        provider_options: dto.provider_options,
        scenario: dto.scenario,
    })
}

fn validate_required_routes(
    workflow: &WorkflowDefinition,
    routes: &HashMap<Role, RoleRoute>,
) -> Result<(), RoutingError> {
    for role in required_roles(workflow) {
        if !routes.contains_key(&role) {
            return Err(RoutingError::MissingRoleRoute(role));
        }
    }
    Ok(())
}

fn provider_config_dto(provider: &str) -> Result<ProviderConfigDto, RoutingError> {
    match provider {
        "claude" => Ok(ProviderConfigDto {
            profile: "native_claude".to_owned(),
            schema_version: 1,
            provider_options: json!({}),
            scenario: None,
        }),
        "codex" => Ok(ProviderConfigDto {
            profile: "native_codex".to_owned(),
            schema_version: 1,
            provider_options: codex_options(),
            scenario: None,
        }),
        "fake" => Ok(ProviderConfigDto {
            profile: "development_fake".to_owned(),
            schema_version: 1,
            provider_options: json!({}),
            scenario: Some("default_success_v1".to_owned()),
        }),
        other => Err(RoutingError::UnsupportedProvider(other.to_owned())),
    }
}

fn validate_provider_config(
    provider: &str,
    config: &ProviderConfigDto,
) -> Result<(), RoutingError> {
    let expected = provider_config_dto(provider)?;
    if config.profile != expected.profile
        || config.schema_version != expected.schema_version
        || config.provider_options != expected.provider_options
        || config.scenario != expected.scenario
    {
        return Err(RoutingError::InvalidProviderConfig(provider.to_owned()));
    }
    Ok(())
}

fn codex_options() -> Value {
    json!({
        "execution_protocol": "exec_json_v1",
        "sandbox_policy": "stage_kind_v1",
        "approval_policy": "never"
    })
}

#[derive(Debug, Error)]
pub enum RoutingError {
    #[error("unsupported execution configuration schema {0}")]
    UnsupportedSchema(u32),
    #[error("invalid routing configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid routing profile metadata")]
    InvalidProfileMetadata,
    #[error("workflow role {0:?} has no configured route")]
    MissingRoleRoute(Role),
    #[error("unsupported route provider {0:?}")]
    UnsupportedProvider(String),
    #[error("route provider {0:?} has no native provider configuration")]
    MissingProviderConfig(String),
    #[error("provider {0:?} has unsupported native configuration")]
    InvalidProviderConfig(String),
    #[error("recommended profile cannot route to development FakeProvider")]
    FakeInRecommended,
    #[error("recommended profile requires authenticated Claude Code or Codex CLI")]
    RecommendedUnavailable,
    #[error("evaluation target role {0:?} is absent from workflow")]
    EvaluationRoleAbsent(Role),
    #[error("effort names role {0:?}, which this workflow cannot route")]
    EffortRoleUnroutable(Role),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::WorkflowKind;

    type CorruptConfig = (&'static str, fn(&mut Value));

    /// The standard workflow as persisted by runs sealed before the
    /// simplification stage existed. Old snapshots resolve against their own
    /// stored graph, never against today's built-in.
    fn pre_simplification_standard_workflow() -> WorkflowDefinition {
        use crate::domain::{Dependency, StageId};
        let id = |value: &str| StageId::new(value).unwrap();
        let required = |value: &str| Dependency::required(id(value));
        WorkflowDefinition::new(
            WorkflowKind::Standard,
            vec![
                StageDefinition::new(
                    id("architecture"),
                    StageKind::Architecture,
                    Role::Architect,
                    vec![],
                ),
                StageDefinition::new(
                    id("implementation"),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![required("architecture")],
                ),
                StageDefinition::new(
                    id("quality_review"),
                    StageKind::CodeQualityReview,
                    Role::CodeQualityReviewer,
                    vec![required("implementation")],
                ),
                StageDefinition::new(
                    id("spec_review"),
                    StageKind::SpecReview,
                    Role::SpecReviewer,
                    vec![required("implementation"), required("architecture")],
                ),
                StageDefinition::new(
                    id("decision"),
                    StageKind::Decision,
                    Role::EngineeringLead,
                    vec![required("quality_review"), required("spec_review")],
                ),
            ],
        )
        .unwrap()
    }

    fn snapshot(selection: ExecutionSelection, available: RecommendedAvailability) -> RoutingPlan {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let snapshot = resolve_config(
            selection,
            EffortSetting::NativeDefault,
            &workflow,
            available,
            ConfigSnapshotId::new("routing-test").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap()
    }

    #[test]
    fn recommended_v3_maps_roles_exactly_and_pins_no_models() {
        let plan = snapshot(
            ExecutionSelection::Recommended,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
        );
        assert_eq!(plan.profile_version(), RECOMMENDED_PROFILE_VERSION);
        assert_eq!(plan.profile_version(), "recommended_v3");
        for (role, provider) in [
            (Role::Architect, "claude"),
            (Role::Implementer, "codex"),
            (Role::CodeQualityReviewer, "claude"),
            (Role::SpecReviewer, "codex"),
            (Role::EngineeringLead, "claude"),
        ] {
            let route = plan.route(role).unwrap();
            assert_eq!(route.target().provider_id().as_str(), provider, "{role:?}");
            assert!(
                route.target().model_id().is_none(),
                "{role:?} pinned a model"
            );
        }
    }

    #[test]
    fn recommended_v2_full_mapping_covers_every_role_including_researcher_and_legacy_reviewer() {
        let provenance = recommended_provenance("recommended_v2").unwrap();
        let expected = [
            (Role::Researcher, "claude"),
            (Role::Architect, "claude"),
            (Role::Implementer, "codex"),
            (Role::CodeQualityReviewer, "claude"),
            (Role::SpecReviewer, "codex"),
            (Role::EngineeringLead, "claude"),
            (Role::Reviewer, "claude"),
            (Role::Simplifier, "claude"),
        ];
        assert_eq!(provenance.decisions.len(), expected.len());
        for (role, provider) in expected {
            let decision = provenance
                .decisions
                .iter()
                .find(|decision| decision.role == role)
                .unwrap();
            assert_eq!(decision.provider, provider, "{role:?}");
        }
    }

    #[test]
    fn recommended_v1_snapshot_still_resolves_v1_routes_unchanged() {
        // Byte-level persisted v1 payload: SpecReviewer routed to Claude with
        // the original v1 reasons. Introducing v2 must not reinterpret it.
        // The workflow is the graph such a run persisted — the standard
        // workflow as it existed before the simplification stage, since a
        // stored run resolves routes against its own stored graph.
        let workflow = pre_simplification_standard_workflow();
        let mut routes = serde_json::Map::new();
        for (role, provider, reason) in [
            ("architect", "claude", "recommended_default"),
            ("implementer", "codex", "implementation_specialist"),
            (
                "code_quality_reviewer",
                "claude",
                "independent_from_implementer",
            ),
            ("spec_reviewer", "claude", "independent_from_implementer"),
            ("engineering_lead", "claude", "recommended_default"),
        ] {
            routes.insert(
                role.to_owned(),
                json!({"provider": provider, "model": null, "reason": reason}),
            );
        }
        let payload = json!({
            "schema_version": 2,
            "profile": "recommended",
            "profile_version": RECOMMENDED_PROFILE_VERSION_V1,
            "routes": routes,
            "providers": {
                "claude": serde_json::to_value(provider_config_dto("claude").unwrap()).unwrap(),
                "codex": serde_json::to_value(provider_config_dto("codex").unwrap()).unwrap(),
            }
        });
        let snapshot = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("persisted-v1").unwrap(),
            2,
            payload.clone(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let plan = RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(plan.profile_version(), "recommended_v1");
        assert_eq!(
            plan.route(Role::SpecReviewer)
                .unwrap()
                .target()
                .provider_id()
                .as_str(),
            "claude"
        );
        assert_eq!(
            plan.route(Role::Implementer)
                .unwrap()
                .target()
                .provider_id()
                .as_str(),
            "codex"
        );
        assert_eq!(
            plan.route(Role::CodeQualityReviewer)
                .unwrap()
                .target()
                .provider_id()
                .as_str(),
            "claude"
        );
        assert!(
            plan.routes()
                .all(|(_, route)| route.target().model_id().is_none())
        );
        // Snapshot payload itself is never rewritten by decoding.
        assert_eq!(snapshot.payload(), &payload);
    }

    #[test]
    fn recommended_v2_provenance_records_measured_and_inherited_evidence() {
        let provenance = recommended_provenance(RECOMMENDED_PROFILE_VERSION_V2).unwrap();
        assert_eq!(provenance.evidence_suite, Some("role_core_v3"));
        assert_eq!(
            provenance.evidence_fingerprint,
            Some("cb9856d2c8edbc4cb0a59520aa140ef4567dce3b650b14f0436d42c4b11c375b")
        );
        assert_eq!(provenance.repetitions_per_case, Some(3));
        assert_eq!(provenance.benchmark_kind, "native_runtime");
        assert_eq!(
            provenance.targets,
            ["claude/native_default", "codex/native_default"]
        );
        let basis = |role: Role| {
            provenance
                .decisions
                .iter()
                .find(|decision| decision.role == role)
                .unwrap()
                .basis
        };
        assert_eq!(
            basis(Role::Implementer),
            DecisionBasis::Measured(DecisionConfidence::High)
        );
        assert_eq!(
            basis(Role::CodeQualityReviewer),
            DecisionBasis::Measured(DecisionConfidence::Medium)
        );
        assert_eq!(
            basis(Role::SpecReviewer),
            DecisionBasis::Measured(DecisionConfidence::Medium)
        );
        for role in [
            Role::Researcher,
            Role::Architect,
            Role::EngineeringLead,
            Role::Reviewer,
        ] {
            assert_eq!(basis(role), DecisionBasis::Inherited, "{role:?}");
        }
        // No monetary/token-cost claims encoded anywhere in rationales.
        for decision in provenance.decisions {
            for term in ["cost", "cheap", "token", "price", "usd", "%"] {
                assert!(
                    !decision.rationale.contains(term),
                    "{:?} rationale encodes cost claim",
                    decision.role
                );
            }
        }
        // v1 provenance exists and is explicitly non-benchmark.
        let v1 = recommended_provenance("recommended_v1").unwrap();
        assert_eq!(v1.evidence_suite, None);
        assert!(
            v1.decisions
                .iter()
                .all(|decision| decision.basis == DecisionBasis::Inherited)
        );
        assert!(recommended_provenance("recommended_v999").is_none());
    }

    /// The current profile changes no route: every v3 destination is the v2
    /// one, restated as inherited because the v2 measurements were taken at
    /// native-default effort. What v3 adds is a requested level per role,
    /// every one of them provisional until an effort sweep replaces it.
    #[test]
    fn recommended_v3_inherits_every_v2_route_and_states_a_provisional_effort_per_role() {
        let v2 = recommended_provenance(RECOMMENDED_PROFILE_VERSION_V2).unwrap();
        let v3 = recommended_provenance(RECOMMENDED_PROFILE_VERSION).unwrap();
        assert_eq!(v3.profile_version, "recommended_v3");
        assert_eq!(v3.benchmark_kind, "expert_provisional");
        assert_eq!(v3.evidence_suite, None);
        assert!(v2.efforts.is_empty(), "v2 predates effort policy");
        assert_eq!(v3.decisions.len(), v2.decisions.len());
        for decision in v3.decisions {
            let inherited = v2
                .decisions
                .iter()
                .find(|candidate| candidate.role == decision.role)
                .unwrap_or_else(|| panic!("{:?} is new in v3", decision.role));
            assert_eq!(decision.provider, inherited.provider, "{:?}", decision.role);
            assert_eq!(
                decision.basis,
                DecisionBasis::Inherited,
                "{:?}",
                decision.role
            );
        }
        // One effort row per routed role, no more and no fewer.
        assert_eq!(v3.efforts.len(), v3.decisions.len());
        for decision in v3.decisions {
            let effort = v3
                .efforts
                .iter()
                .find(|candidate| candidate.role == decision.role)
                .unwrap_or_else(|| panic!("{:?} has a route but no effort", decision.role));
            assert_eq!(
                effort.basis,
                DecisionBasis::Provisional,
                "{:?}",
                decision.role
            );
            assert_ne!(
                effort.effort,
                EffortSetting::NativeDefault,
                "{:?}: a stated policy is a level, never the runtime's default",
                decision.role
            );
        }
        // The shape of the policy: reasoning roles high, the implementer at
        // medium, the simplifier at low.
        assert_eq!(recommended_effort(Role::Architect), EffortSetting::HIGH);
        assert_eq!(recommended_effort(Role::Researcher), EffortSetting::HIGH);
        assert_eq!(
            recommended_effort(Role::CodeQualityReviewer),
            EffortSetting::HIGH
        );
        assert_eq!(recommended_effort(Role::SpecReviewer), EffortSetting::HIGH);
        assert_eq!(
            recommended_effort(Role::EngineeringLead),
            EffortSetting::HIGH
        );
        assert_eq!(recommended_effort(Role::Implementer), EffortSetting::MEDIUM);
        assert_eq!(recommended_effort(Role::Simplifier), EffortSetting::LOW);
        assert_eq!(
            recommended_effort(Role::Verifier),
            EffortSetting::NativeDefault,
            "no dial, no row"
        );
        // No monetary/token-cost claims in any rationale, route or effort.
        for rationale in v3
            .decisions
            .iter()
            .map(|decision| decision.rationale)
            .chain(v3.efforts.iter().map(|effort| effort.rationale))
        {
            for term in ["cost", "cheap", "token", "price", "usd", "%"] {
                assert!(
                    !rationale.contains(term),
                    "{rationale} encodes a cost claim"
                );
            }
        }
    }

    /// The default run now carries the profile's own effort per role, and
    /// carries it sealed: the same snapshot decodes the same plan whatever
    /// the current profile later says.
    #[test]
    fn profile_default_under_recommended_seals_the_v3_effort_column() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortRequest::ProfileDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("profile-default").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert_eq!(snapshot.schema_version(), 3);
        let plan = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(plan.effort(Role::Architect), Some(EffortSetting::HIGH));
        assert_eq!(plan.effort(Role::Implementer), Some(EffortSetting::MEDIUM));
        assert_eq!(plan.effort(Role::Simplifier), Some(EffortSetting::LOW));
        assert_eq!(
            plan.effort(Role::CodeQualityReviewer),
            Some(EffortSetting::HIGH)
        );
        assert_eq!(plan.effort(Role::SpecReviewer), Some(EffortSetting::HIGH));
        assert_eq!(
            plan.effort(Role::EngineeringLead),
            Some(EffortSetting::HIGH)
        );
        assert_eq!(
            plan.effort(Role::Verifier),
            Some(EffortSetting::NativeDefault)
        );
        // The routes are untouched by the effort column.
        let routing = RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(
            routing
                .route(Role::Implementer)
                .unwrap()
                .target()
                .provider_id()
                .as_str(),
            "codex"
        );
    }

    /// A uniform provider has no profile policy: omitting effort keeps every
    /// native invocation byte-identical to what it was, under schema v2.
    #[test]
    fn profile_default_under_a_uniform_provider_stays_native_and_schema_v2() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        for provider in [
            UniformProvider::Claude,
            UniformProvider::Codex,
            UniformProvider::Fake,
        ] {
            let snapshot = resolve_config(
                ExecutionSelection::Uniform(provider),
                EffortRequest::ProfileDefault,
                &workflow,
                RecommendedAvailability::default(),
                ConfigSnapshotId::new("uniform-default").unwrap(),
                std::time::SystemTime::now().into(),
            )
            .unwrap();
            assert_eq!(snapshot.schema_version(), 2, "{provider:?}");
            assert!(snapshot.payload().get("resource_plan").is_none());
            let plan = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
            for (_, effort) in plan.efforts() {
                assert_eq!(effort, EffortSetting::NativeDefault, "{provider:?}");
            }
        }
    }

    /// Naming some roles leaves the rest to the profile; naming a role the
    /// workflow cannot route is refused before anything is sealed.
    #[test]
    fn per_role_effort_fills_unnamed_roles_from_the_profile_and_rejects_alien_roles() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let available = RecommendedAvailability {
            claude: true,
            codex: true,
        };
        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortRequest::PerRole(HashMap::from([
                (Role::Architect, EffortSetting::XHIGH),
                (Role::Implementer, EffortSetting::LOW),
            ])),
            &workflow,
            available,
            ConfigSnapshotId::new("per-role").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let plan = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(plan.effort(Role::Architect), Some(EffortSetting::XHIGH));
        assert_eq!(plan.effort(Role::Implementer), Some(EffortSetting::LOW));
        assert_eq!(
            plan.effort(Role::Simplifier),
            Some(EffortSetting::LOW),
            "unnamed roles keep the profile's level"
        );
        assert_eq!(plan.effort(Role::SpecReviewer), Some(EffortSetting::HIGH));

        // Under a uniform provider the unnamed roles have no profile to fall
        // back on and stay native.
        let uniform = resolve_config(
            ExecutionSelection::Uniform(UniformProvider::Codex),
            EffortRequest::PerRole(HashMap::from([(Role::Implementer, EffortSetting::HIGH)])),
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("per-role-uniform").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert_eq!(uniform.schema_version(), 3);
        let plan = ResourcePlan::from_snapshot(&uniform, &workflow).unwrap();
        assert_eq!(plan.effort(Role::Implementer), Some(EffortSetting::HIGH));
        assert_eq!(
            plan.effort(Role::Architect),
            Some(EffortSetting::NativeDefault)
        );

        // Fast has no architect and the verifier is never in a plan.
        let fast = WorkflowDefinition::built_in(WorkflowKind::Fast);
        for (role, name) in [(Role::Architect, "architect"), (Role::Verifier, "verifier")] {
            let refused = resolve_config(
                ExecutionSelection::Recommended,
                EffortRequest::PerRole(HashMap::from([(role, EffortSetting::HIGH)])),
                &fast,
                available,
                ConfigSnapshotId::new(format!("alien-{name}")).unwrap(),
                std::time::SystemTime::now().into(),
            );
            assert!(
                matches!(refused, Err(RoutingError::EffortRoleUnroutable(refused)) if refused == role),
                "{name}: {refused:?}"
            );
        }
    }

    #[test]
    fn recommended_persists_only_roles_required_by_workflow() {
        let plan = snapshot(
            ExecutionSelection::Recommended,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
        );
        assert_eq!(plan.routes().count(), 6);

        let workflow = WorkflowDefinition::built_in(WorkflowKind::Fast);
        let config = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("fast-used-roles").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let fast = RoutingPlan::from_snapshot(&config, &workflow).unwrap();
        assert_eq!(fast.routes().count(), 1);
        assert!(fast.route(Role::Implementer).is_some());
    }

    #[test]
    fn recommended_falls_back_only_during_resolution() {
        for (availability, provider) in [
            (
                RecommendedAvailability {
                    claude: true,
                    codex: false,
                },
                "claude",
            ),
            (
                RecommendedAvailability {
                    claude: false,
                    codex: true,
                },
                "codex",
            ),
        ] {
            let plan = snapshot(ExecutionSelection::Recommended, availability);
            assert!(plan.routes().all(|(_, route)| {
                route.target().provider_id().as_str() == provider
                    && route.reason().starts_with("fallback_")
            }));
        }
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Fast);
        assert!(matches!(
            resolve_config(
                ExecutionSelection::Recommended,
                EffortSetting::NativeDefault,
                &workflow,
                RecommendedAvailability::default(),
                ConfigSnapshotId::new("none").unwrap(),
                std::time::SystemTime::now().into(),
            ),
            Err(RoutingError::RecommendedUnavailable)
        ));
    }

    #[test]
    fn uniform_routes_every_required_role_and_model_is_target_identity() {
        let routing = snapshot(
            ExecutionSelection::Uniform(UniformProvider::Codex),
            RecommendedAvailability::default(),
        );
        assert!(
            routing
                .routes()
                .all(|(_, route)| route.target().provider_id().as_str() == "codex")
        );
        let plain = ExecutionTarget::new(ProviderId::new("codex").unwrap(), None);
        let modeled = ExecutionTarget::new(
            ProviderId::new("codex").unwrap(),
            Some(ModelId::new("model-a").unwrap()),
        );
        assert_ne!(plain, modeled);
        assert_eq!(HashSet::from([plain, modeled]).len(), 2);
    }

    /// A review's stages never mention the Implementer, but a completed review
    /// can be sent back to fix what it found — and configuration is sealed at
    /// creation, so the route has to be there before anyone asks.
    #[test]
    fn a_review_is_configured_to_route_the_fix_it_may_later_be_asked_for() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        assert!(
            !workflow
                .stages()
                .iter()
                .any(|stage| stage.role() == Role::Implementer),
            "no review stage implements anything; the route is for the cycle it can grow"
        );

        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("review-fixable").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();

        assert_eq!(
            unroutable_fix_role(&snapshot, &workflow).unwrap(),
            None,
            "every role a fix cycle adds is routable"
        );
        let plan = RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert!(plan.route(Role::Implementer).is_some());
    }

    /// The verifier never appears in a snapshot and never needs to: every
    /// selection routes it to the same deterministic provider, with native
    /// default effort, and the persisted routes stay exactly the roles the
    /// snapshot decided.
    #[test]
    fn the_verifier_routes_implicitly_to_the_verify_provider_under_every_selection() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        for selection in [
            ExecutionSelection::Uniform(UniformProvider::Fake),
            ExecutionSelection::Uniform(UniformProvider::Claude),
            ExecutionSelection::Uniform(UniformProvider::Codex),
            ExecutionSelection::Recommended,
        ] {
            let snapshot = resolve_config(
                selection,
                EffortSetting::Level(crate::domain::EffortLevel::High),
                &workflow,
                RecommendedAvailability {
                    claude: true,
                    codex: true,
                },
                ConfigSnapshotId::new("verifier").unwrap(),
                std::time::SystemTime::now().into(),
            )
            .unwrap();
            let plan = RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap();
            let route = plan.route(Role::Verifier).expect("verifier always routes");
            assert_eq!(route.target().provider_id().as_str(), VERIFY_PROVIDER_ID);
            assert_eq!(route.target().model_id(), None);
            assert!(
                plan.routes().all(|(role, _)| role != Role::Verifier),
                "the verifier is not a persisted route"
            );
            assert!(
                snapshot.payload()["routes"].get("verifier").is_none(),
                "the snapshot never names the verifier"
            );
            assert!(
                snapshot.payload()["resource_plan"]
                    .get("verifier")
                    .is_none(),
                "the resource plan never names the verifier"
            );
            let efforts = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
            assert_eq!(
                efforts.effort(Role::Verifier),
                Some(EffortSetting::NativeDefault)
            );
        }
        assert!(
            RECOMMENDED_V2_PROVENANCE
                .decisions
                .iter()
                .all(|decision| decision.role != Role::Verifier)
        );
    }

    /// A run sealed before the verifier existed carries neither its route
    /// nor its effort, and is never rewritten. It still loads against the
    /// verify-bearing built-in, and the fix cycle it can grow — which now
    /// includes a verify stage — is still fully routable.
    #[test]
    fn a_snapshot_sealed_before_the_verifier_existed_still_loads_and_fixes() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let sealed = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::Level(crate::domain::EffortLevel::Medium),
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("pre-verifier").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        // Today's payload already omits the verifier; a pre-verifier snapshot
        // is byte-identical in shape, which is the whole point.
        let plan = RoutingPlan::from_snapshot(&sealed, &workflow).unwrap();
        assert_eq!(
            plan.route(Role::Verifier)
                .unwrap()
                .target()
                .provider_id()
                .as_str(),
            VERIFY_PROVIDER_ID
        );
        assert_eq!(unroutable_fix_role(&sealed, &workflow).unwrap(), None);
    }

    /// Configurations written before fix-cycle routing carry no such route, and
    /// they are never rewritten. Asking has to say so *before* a fix appends
    /// stages the run could then never execute — or be read past.
    #[test]
    fn a_configuration_sealed_without_fix_routing_names_what_it_cannot_route() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        let sealed = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("legacy-review").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        // Exactly what a pre-fix-cycle snapshot looks like: the workflow's own
        // roles and nothing more.
        let mut payload = sealed.payload().clone();
        let routes = payload
            .get_mut("routes")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        routes.remove("implementer");
        let legacy = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("legacy-review-trimmed").unwrap(),
            sealed.schema_version(),
            payload,
            std::time::SystemTime::now().into(),
        )
        .unwrap();

        assert_eq!(
            unroutable_fix_role(&legacy, &workflow).unwrap(),
            Some(Role::Implementer)
        );
    }

    /// The route alone is not enough to drive a fix stage: the provider also
    /// asks the resource plan for the role's requested effort. A v2 review
    /// snapshot predates effort policy entirely, so its plan must cover the
    /// fix-cycle roles with `NativeDefault` — this is the exact gap behind
    /// "configured effort missing for Implementer".
    #[test]
    fn a_review_states_native_default_effort_for_the_fix_it_may_be_asked_for() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("review-effort").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert_eq!(snapshot.schema_version(), 2);
        let plan = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(
            plan.effort(Role::Implementer),
            Some(EffortSetting::NativeDefault),
            "the fix cycle's roles need effort exactly like they need routes"
        );
        assert_eq!(unroutable_fix_role(&snapshot, &workflow).unwrap(), None);
    }

    /// Explicit effort on a review seals a v3 plan over every routable role,
    /// Implementer included, and the seal-time round-trip must accept it.
    #[test]
    fn explicit_effort_on_a_review_seals_and_covers_the_fix_cycle() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::HIGH,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("review-explicit").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert_eq!(snapshot.schema_version(), 3);
        let plan = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(plan.effort(Role::Implementer), Some(EffortSetting::HIGH));
        assert_eq!(unroutable_fix_role(&snapshot, &workflow).unwrap(), None);
    }

    /// A v3 snapshot sealed before fix-cycle routing states effort only for
    /// the roles its workflow started with. It must keep decoding — the run
    /// is still readable — but a fix request has to be refused by name, not
    /// discovered mid-drive as a missing effort.
    #[test]
    fn a_v3_plan_sealed_without_fix_effort_decodes_but_names_the_refused_role() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        let sealed = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::HIGH,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("pre-fix-v3").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let mut payload = sealed.payload().clone();
        payload["resource_plan"]
            .as_object_mut()
            .unwrap()
            .remove("implementer");
        let legacy = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("pre-fix-v3-trimmed").unwrap(),
            sealed.schema_version(),
            payload,
            std::time::SystemTime::now().into(),
        )
        .unwrap();

        let plan = ResourcePlan::from_snapshot(&legacy, &workflow).unwrap();
        assert_eq!(plan.effort(Role::Implementer), None);
        assert_eq!(
            unroutable_fix_role(&legacy, &workflow).unwrap(),
            Some(Role::Implementer)
        );
    }

    /// The widened coverage rule is a window, not an open door: a role no
    /// run of this workflow could ever grow still fails closed.
    #[test]
    fn a_v3_plan_with_a_role_outside_the_routable_set_fails_closed() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        let sealed = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::HIGH,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("alien-role").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let mut payload = sealed.payload().clone();
        payload["resource_plan"]["architect"] = json!("high");
        let widened = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("alien-role-widened").unwrap(),
            sealed.schema_version(),
            payload,
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert!(ResourcePlan::from_snapshot(&widened, &workflow).is_err());
    }

    #[test]
    fn persisted_routes_are_independent_from_current_policy() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Fast);
        let persisted = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("stable").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let loaded = RoutingPlan::from_snapshot(&persisted, &workflow).unwrap();
        assert_eq!(
            loaded
                .route(Role::Implementer)
                .unwrap()
                .target()
                .provider_id()
                .as_str(),
            "codex"
        );
    }

    /// `--effort native` is the opt-out from the profile's policy: every
    /// role native, and the payload exactly what it was before effort policy
    /// existed.
    #[test]
    fn native_effort_keeps_schema_v2_payload_identical_to_pre_effort_encoding() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("native-default").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert_eq!(snapshot.schema_version(), 2);
        assert!(snapshot.payload().get("resource_plan").is_none());
        // Opting out of the effort column does not change the profile.
        assert_eq!(
            snapshot.payload().get("profile_version").unwrap(),
            RECOMMENDED_PROFILE_VERSION
        );
        let plan = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
        for (_, effort) in plan.efforts() {
            assert_eq!(effort, EffortSetting::NativeDefault);
        }
    }

    #[test]
    fn explicit_effort_persists_per_role_resource_plan_under_schema_v3() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Deep);
        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::HIGH,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("explicit-high").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert_eq!(snapshot.schema_version(), 3);
        // Routing behavior is untouched by effort: same profile identity.
        assert_eq!(
            snapshot.payload().get("profile_version").unwrap(),
            RECOMMENDED_PROFILE_VERSION
        );
        let routing = RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(routing.profile_version(), RECOMMENDED_PROFILE_VERSION);
        let plan = ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap();
        let mut count = 0;
        for (_, effort) in plan.efforts() {
            assert_eq!(effort, EffortSetting::HIGH);
            count += 1;
        }
        assert_eq!(count, routing.routes().count());
        // Restart determinism: decoding the same immutable snapshot twice is exact.
        assert_eq!(
            plan,
            ResourcePlan::from_snapshot(&snapshot, &workflow).unwrap()
        );
    }

    #[test]
    fn schema_v1_and_v2_decode_to_native_default_never_medium() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let legacy = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("legacy-effort").unwrap(),
            1,
            json!({
                "schema_version":1,
                "profile":"native_codex",
                "provider":"codex",
                "model":null,
                "provider_options":codex_options()
            }),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let v2 = resolve_config(
            ExecutionSelection::Uniform(UniformProvider::Fake),
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("v2-effort").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        for snapshot in [&legacy, &v2] {
            let plan = ResourcePlan::from_snapshot(snapshot, &workflow).unwrap();
            for role in workflow
                .stages()
                .iter()
                .map(crate::domain::StageDefinition::role)
            {
                let effort = plan.effort(role).unwrap();
                assert_eq!(effort, EffortSetting::NativeDefault);
                assert_ne!(effort, EffortSetting::MEDIUM);
            }
        }
    }

    #[test]
    fn malformed_or_unknown_effort_fails_closed() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let valid = resolve_config(
            ExecutionSelection::Uniform(UniformProvider::Fake),
            EffortSetting::MEDIUM,
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("valid-effort").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert_eq!(valid.schema_version(), 3);

        let corrupt = |mutate: fn(&mut Value), id: &str| {
            let mut payload = valid.payload().clone();
            mutate(&mut payload);
            ResolvedConfigSnapshot::new(
                ConfigSnapshotId::new(id).unwrap(),
                payload
                    .get("schema_version")
                    .and_then(Value::as_u64)
                    .and_then(|version| u32::try_from(version).ok())
                    .unwrap_or(3),
                payload,
                std::time::SystemTime::now().into(),
            )
            .unwrap()
        };

        // Unknown future setting never silently becomes Medium/default.
        let unknown = corrupt(
            |payload| {
                payload["resource_plan"]["implementer"] = json!("turbo");
            },
            "unknown-effort",
        );
        assert!(ResourcePlan::from_snapshot(&unknown, &workflow).is_err());
        assert!(RoutingPlan::from_snapshot(&unknown, &workflow).is_err());

        // Missing role coverage fails closed.
        let missing = corrupt(
            |payload| {
                payload["resource_plan"]
                    .as_object_mut()
                    .unwrap()
                    .remove("implementer");
            },
            "missing-role-effort",
        );
        assert!(ResourcePlan::from_snapshot(&missing, &workflow).is_err());

        // v3 without a resource plan fails closed.
        let absent = corrupt(
            |payload| {
                payload.as_object_mut().unwrap().remove("resource_plan");
            },
            "absent-plan",
        );
        assert!(ResourcePlan::from_snapshot(&absent, &workflow).is_err());

        // A resource plan smuggled into schema v2 fails closed.
        let mut smuggled_payload = valid.payload().clone();
        smuggled_payload["schema_version"] = json!(2);
        let smuggled = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("smuggled-plan").unwrap(),
            2,
            smuggled_payload,
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        assert!(RoutingPlan::from_snapshot(&smuggled, &workflow).is_err());
    }

    #[test]
    fn legacy_schema_v1_normalizes_without_rewriting_snapshot() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let payload = json!({
            "schema_version":1,
            "profile":"native_codex",
            "provider":"codex",
            "model":null,
            "provider_options":codex_options()
        });
        let snapshot = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("legacy").unwrap(),
            1,
            payload.clone(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let plan = RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert!(
            plan.routes()
                .all(|(_, route)| route.target().provider_id().as_str() == "codex")
        );
        assert_eq!(snapshot.payload(), &payload);
        assert_eq!(plan.profile_version(), "legacy_schema_v1");
    }

    #[test]
    fn config_v2_validation_fails_closed_for_malformed_routes_and_native_options() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let valid = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("valid").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let cases: [CorruptConfig; 7] = [
            (
                "missing-route",
                (|value: &mut Value| {
                    value["routes"].as_object_mut().unwrap().remove("architect");
                }) as fn(&mut Value),
            ),
            ("unsupported-provider", |value: &mut Value| {
                value["routes"]["architect"]["provider"] = json!("gemini");
            }),
            ("invalid-model", |value: &mut Value| {
                value["routes"]["architect"]["model"] = json!(42);
            }),
            ("missing-provider-config", |value: &mut Value| {
                value["providers"].as_object_mut().unwrap().remove("codex");
            }),
            ("malformed-profile", |value: &mut Value| {
                value["profile_version"] = json!("recommended_v999");
            }),
            ("unsafe-options", |value: &mut Value| {
                value["providers"]["codex"]["provider_options"]["approval_policy"] =
                    json!("always");
            }),
            ("fake-recommended", |value: &mut Value| {
                value["routes"]["architect"]["provider"] = json!("fake");
                value["providers"]["fake"] =
                    serde_json::to_value(provider_config_dto("fake").unwrap()).unwrap();
            }),
        ];
        for (name, mutate) in cases {
            let mut payload = valid.payload().clone();
            mutate(&mut payload);
            let snapshot = ResolvedConfigSnapshot::new(
                ConfigSnapshotId::new(name).unwrap(),
                2,
                payload,
                std::time::SystemTime::now().into(),
            )
            .unwrap();
            assert!(
                RoutingPlan::from_snapshot(&snapshot, &workflow).is_err(),
                "{name} unexpectedly passed"
            );
        }
    }

    #[test]
    fn eval_profile_routes_exactly_one_role_to_modeled_candidate_and_support_to_fake() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        let target = ExecutionTarget::new(
            ProviderId::new("codex").unwrap(),
            Some(ModelId::new("candidate-model").unwrap()),
        );
        let snapshot = resolve_eval_config(
            Role::SpecReviewer,
            &target,
            EffortSetting::NativeDefault,
            &workflow,
            ConfigSnapshotId::new("eval-routing").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let plan = RoutingPlan::from_snapshot(&snapshot, &workflow).unwrap();
        assert_eq!(plan.profile(), "eval");
        assert_eq!(plan.profile_version(), EVAL_PROFILE_VERSION);
        assert_eq!(plan.route(Role::SpecReviewer).unwrap().target(), &target);
        assert_eq!(
            plan.routes()
                .filter(|(_, route)| route.reason() == "eval_candidate")
                .count(),
            1
        );
        assert!(plan.routes().all(|(role, route)| {
            role == Role::SpecReviewer
                || (route.target().provider_id().as_str() == "fake"
                    && route.target().model_id().is_none()
                    && route.reason() == "eval_support")
        }));
        assert_eq!(RECOMMENDED_PROFILE_VERSION_V1, "recommended_v1");
        assert_eq!(RECOMMENDED_PROFILE_VERSION_V2, "recommended_v2");
        assert_eq!(RECOMMENDED_PROFILE_VERSION, "recommended_v3");
    }

    #[test]
    fn image_generation_seals_schema_v4_and_disabled_keeps_old_payloads_byte_identical() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let at: DateTime<Utc> = std::time::SystemTime::now().into();
        let baseline = resolve_config(
            ExecutionSelection::Uniform(UniformProvider::Claude),
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("img-off").unwrap(),
            at,
        )
        .unwrap();
        let disabled = resolve_config_with_image(
            ExecutionSelection::Uniform(UniformProvider::Claude),
            EffortSetting::NativeDefault,
            &ImageGenerationPlan::disabled(),
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("img-off").unwrap(),
            at,
        )
        .unwrap();
        assert_eq!(
            disabled, baseline,
            "a disabled grant must not touch the payload"
        );
        assert_eq!(disabled.schema_version(), 2);
        assert!(
            !ImageGenerationPlan::from_snapshot(&disabled, &workflow)
                .unwrap()
                .is_enabled()
        );

        let enabled = resolve_config_with_image(
            ExecutionSelection::Uniform(UniformProvider::Claude),
            EffortSetting::NativeDefault,
            &ImageGenerationPlan::implementer_only(),
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("img-on").unwrap(),
            at,
        )
        .unwrap();
        assert_eq!(enabled.schema_version(), 4);
        assert_eq!(
            enabled.payload()["image_generation"],
            json!({"roles": ["implementer"], "max_generations": DEFAULT_MAX_IMAGE_GENERATIONS})
        );
        assert!(enabled.payload().get("resource_plan").is_none());
        // The snapshot names no backend and no model: those belong to the backend.
        assert!(!enabled.payload().to_string().contains("openai"));
        assert!(!enabled.payload().to_string().contains("gpt-image"));
        let plan = ImageGenerationPlan::from_snapshot(&enabled, &workflow).unwrap();
        assert!(plan.allows(Role::Implementer));
        assert!(!plan.allows(Role::CodeQualityReviewer));
        assert!(!plan.allows(Role::SpecReviewer));
        assert_eq!(plan.max_generations(), DEFAULT_MAX_IMAGE_GENERATIONS);
        // Routing and effort read v4 exactly as they read v2.
        let routing = RoutingPlan::from_snapshot(&enabled, &workflow).unwrap();
        assert_eq!(routing.profile(), "uniform");
        let efforts = ResourcePlan::from_snapshot(&enabled, &workflow).unwrap();
        for (_, effort) in efforts.efforts() {
            assert_eq!(effort, EffortSetting::NativeDefault);
        }

        let both = resolve_config_with_image(
            ExecutionSelection::Uniform(UniformProvider::Codex),
            EffortSetting::HIGH,
            &ImageGenerationPlan::implementer_only(),
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("img-effort").unwrap(),
            at,
        )
        .unwrap();
        assert_eq!(both.schema_version(), 4);
        assert!(both.payload().get("resource_plan").is_some());
        let efforts = ResourcePlan::from_snapshot(&both, &workflow).unwrap();
        assert_eq!(efforts.effort(Role::Implementer), Some(EffortSetting::HIGH));
        assert!(
            ImageGenerationPlan::from_snapshot(&both, &workflow)
                .unwrap()
                .is_enabled()
        );
    }

    #[test]
    fn old_snapshots_decode_image_generation_disabled_and_malformed_fails_closed() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let at: DateTime<Utc> = std::time::SystemTime::now().into();
        for effort in [EffortSetting::NativeDefault, EffortSetting::LOW] {
            let old = resolve_config(
                ExecutionSelection::Uniform(UniformProvider::Fake),
                effort,
                &workflow,
                RecommendedAvailability::default(),
                ConfigSnapshotId::new("old").unwrap(),
                at,
            )
            .unwrap();
            assert!(
                !ImageGenerationPlan::from_snapshot(&old, &workflow)
                    .unwrap()
                    .is_enabled(),
                "schema {} must decode disabled",
                old.schema_version()
            );
        }
        let legacy = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("legacy").unwrap(),
            1,
            json!({"schema_version":1,"profile":"native_codex","provider":"codex","provider_options":{}}),
            at,
        )
        .unwrap();
        assert_eq!(
            ImageGenerationPlan::from_snapshot(&legacy, &workflow).unwrap(),
            ImageGenerationPlan::disabled()
        );

        let sealed = resolve_config_with_image(
            ExecutionSelection::Uniform(UniformProvider::Fake),
            EffortSetting::NativeDefault,
            &ImageGenerationPlan::implementer_only(),
            &workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("valid").unwrap(),
            at,
        )
        .unwrap();
        let corruptions: [CorruptConfig; 7] = [
            ("unknown role", |payload| {
                payload["image_generation"]["roles"] = json!(["designer"]);
            }),
            ("empty roles", |payload| {
                payload["image_generation"]["roles"] = json!([]);
            }),
            ("duplicate roles", |payload| {
                payload["image_generation"]["roles"] = json!(["implementer", "implementer"]);
            }),
            ("zero bound", |payload| {
                payload["image_generation"]["max_generations"] = json!(0);
            }),
            ("absurd bound", |payload| {
                payload["image_generation"]["max_generations"] = json!(1_000_000);
            }),
            ("unknown field", |payload| {
                payload["image_generation"]["backend"] = json!("openai");
            }),
            ("block smuggled under v2", |payload| {
                payload["schema_version"] = json!(2);
            }),
        ];
        for (name, corrupt) in corruptions {
            let mut payload = sealed.payload().clone();
            corrupt(&mut payload);
            let version = payload["schema_version"].as_u64().unwrap();
            let snapshot = ResolvedConfigSnapshot::new(
                ConfigSnapshotId::new("corrupt").unwrap(),
                u32::try_from(version).unwrap(),
                payload,
                at,
            )
            .unwrap();
            assert!(
                ImageGenerationPlan::from_snapshot(&snapshot, &workflow).is_err(),
                "{name} must fail closed"
            );
            assert!(
                RoutingPlan::from_snapshot(&snapshot, &workflow).is_err(),
                "{name} must also refuse to route"
            );
        }
        // A v4 payload without the block is not "disabled", it is malformed.
        let mut payload = sealed.payload().clone();
        payload.as_object_mut().unwrap().remove("image_generation");
        let snapshot = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("v4-missing").unwrap(),
            4,
            payload,
            at,
        )
        .unwrap();
        assert!(ImageGenerationPlan::from_snapshot(&snapshot, &workflow).is_err());
    }
}
