use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{Role, StageId};

/// Identity of one opinionated built-in workflow family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowKind {
    Fast,
    Standard,
    Deep,
    Review,
    /// A mission's lead session: one read-only stage per exchange with the
    /// user, each answering over the current mission state. Never started
    /// from the run composer; a mission opens it.
    Lead,
}

/// Semantic work performed by a stage, independent from assigned role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Research,
    Architecture,
    Implementation,
    /// A post-implementation pass that edits the change in place to remove
    /// accidental complexity. Additive: it changes no persisted snapshot or
    /// database shape, the same way `Fix` and `FollowUp` did before it.
    Simplification,
    CodeQualityReview,
    SpecReview,
    /// Legacy/general review stage retained for persisted runs.
    Review,
    IndependentReview,
    DeepAnalysis,
    Synthesis,
    Decision,
    Fix,
    /// One turn of a mission's lead conversation: reads the mission brief
    /// and the user's message, answers in prose and a `## Plan changes`
    /// section. Read-only, like a review.
    Lead,
    /// A continue cycle's implementer stage, carrying an operator-supplied
    /// instruction rather than answering blocking findings. Additive: it
    /// changes no persisted snapshot or database shape, the same way `Fix`
    /// did when reviewer specialization introduced it.
    FollowUp,
    /// Runs the repository's own verification commands in the worktree and
    /// records every exit code. No provider judgment is involved: the stage
    /// completes only when every command exits zero, and a run whose
    /// verification failed never completes, so it can never be applied.
    /// Additive like `Simplification`: no persisted shape changes.
    Verify,
}

impl StageKind {
    /// Whether a stage of this kind may modify repository content. The one
    /// predicate behind writable-workspace requirements, provider read-only
    /// guards, and which stages draft the pull request when a run publishes.
    #[must_use]
    pub const fn edits_workspace(self) -> bool {
        matches!(
            self,
            Self::Implementation | Self::Simplification | Self::Fix | Self::FollowUp
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Required,
    Optional,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dependency {
    stage_id: StageId,
    kind: DependencyKind,
}

impl Dependency {
    #[must_use]
    pub const fn new(stage_id: StageId, kind: DependencyKind) -> Self {
        Self { stage_id, kind }
    }

    #[must_use]
    pub fn required(stage_id: StageId) -> Self {
        Self::new(stage_id, DependencyKind::Required)
    }

    #[must_use]
    pub fn optional(stage_id: StageId) -> Self {
        Self::new(stage_id, DependencyKind::Optional)
    }

    #[must_use]
    pub const fn stage_id(&self) -> &StageId {
        &self.stage_id
    }

    #[must_use]
    pub const fn kind(&self) -> DependencyKind {
        self.kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageDefinition {
    id: StageId,
    kind: StageKind,
    role: Role,
    dependencies: Vec<Dependency>,
}

impl StageDefinition {
    #[must_use]
    pub const fn new(
        id: StageId,
        kind: StageKind,
        role: Role,
        dependencies: Vec<Dependency>,
    ) -> Self {
        Self {
            id,
            kind,
            role,
            dependencies,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &StageId {
        &self.id
    }

    #[must_use]
    pub const fn kind(&self) -> StageKind {
        self.kind
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }
}

/// Validated dependency DAG for one workflow kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkflowDefinition {
    kind: WorkflowKind,
    stages: Vec<StageDefinition>,
}

impl WorkflowDefinition {
    /// Creates and validates a workflow dependency graph.
    ///
    /// # Errors
    /// Rejects empty workflows, duplicate stages/dependencies, self references,
    /// unknown dependencies, and cycles.
    pub fn new(
        kind: WorkflowKind,
        stages: Vec<StageDefinition>,
    ) -> Result<Self, WorkflowDefinitionError> {
        validate_stages(&stages)?;
        Ok(Self { kind, stages })
    }

    /// Returns one validated built-in workflow definition.
    ///
    /// Built-ins are ordinary DAG data. The scheduler never branches on
    /// [`WorkflowKind`] and therefore treats user-defined definitions with the
    /// same graph shape identically.
    ///
    /// # Panics
    /// Panics only when a source-controlled built-in definition violates DAG
    /// invariants. Tests cover every built-in definition.
    #[must_use]
    pub fn built_in(kind: WorkflowKind) -> Self {
        let stages = match kind {
            WorkflowKind::Fast => fast_stages(),
            WorkflowKind::Standard => standard_stages(),
            WorkflowKind::Deep => deep_stages(),
            WorkflowKind::Review => review_stages(),
            WorkflowKind::Lead => lead_stages(),
        };
        Self::new(kind, stages).expect("built-in workflow must remain a valid DAG")
    }

    /// Returns this workflow with further stages appended, revalidated whole.
    ///
    /// A workflow is data, and the scheduler never branches on
    /// [`WorkflowKind`], so a run that grows a remediation cycle is still an
    /// ordinary DAG. Every invariant is rechecked against the combined set —
    /// duplicate IDs, unknown dependencies and cycles are rejected here rather
    /// than discovered by the scheduler later.
    ///
    /// # Errors
    /// Rejects an empty extension and any extension that would break the DAG.
    pub fn extended(
        &self,
        additional: Vec<StageDefinition>,
    ) -> Result<Self, WorkflowDefinitionError> {
        if additional.is_empty() {
            return Err(WorkflowDefinitionError::NoStages);
        }
        let mut stages = self.stages.clone();
        stages.extend(additional);
        Self::new(self.kind, stages)
    }

    /// This workflow with every verification stage removed and the edges
    /// into those stages dropped, revalidated whole.
    ///
    /// Evaluations use it. They measure one role's output against a fixture
    /// through synthetic support roles, and running the fixture's own checks
    /// is not part of that measurement — a planted bug is supposed to fail
    /// them, and a build tree left in the worktree would show up in the
    /// scope the scorer inspects.
    ///
    /// # Errors
    /// Rejects a workflow made only of verification stages, or one the
    /// removal leaves structurally invalid.
    pub fn without_verification(&self) -> Result<Self, WorkflowDefinitionError> {
        let stages = self
            .stages
            .iter()
            .filter(|stage| stage.kind() != StageKind::Verify)
            .map(|stage| {
                StageDefinition::new(
                    stage.id().clone(),
                    stage.kind(),
                    stage.role(),
                    stage
                        .dependencies()
                        .iter()
                        .filter(|dependency| {
                            self.stage(dependency.stage_id())
                                .is_none_or(|target| target.kind() != StageKind::Verify)
                        })
                        .cloned()
                        .collect(),
                )
            })
            .collect();
        Self::new(self.kind, stages)
    }

    #[must_use]
    pub const fn kind(&self) -> WorkflowKind {
        self.kind
    }

    #[must_use]
    pub fn stages(&self) -> &[StageDefinition] {
        &self.stages
    }

    #[must_use]
    pub fn stage(&self, stage_id: &StageId) -> Option<&StageDefinition> {
        self.stages.iter().find(|stage| stage.id == *stage_id)
    }

    /// Whether any stage can modify repository content.
    #[must_use]
    pub fn requires_writable_workspace(&self) -> bool {
        self.stages
            .iter()
            .any(|stage| stage.kind().edits_workspace())
    }
}

fn fast_stages() -> Vec<StageDefinition> {
    vec![
        stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            vec![],
        ),
        stage(
            "verify",
            StageKind::Verify,
            Role::Verifier,
            vec![required("implementation")],
        ),
    ]
}

fn standard_stages() -> Vec<StageDefinition> {
    vec![
        stage(
            "architecture",
            StageKind::Architecture,
            Role::Architect,
            vec![],
        ),
        stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            vec![required("architecture")],
        ),
        stage(
            "simplification",
            StageKind::Simplification,
            Role::Simplifier,
            vec![required("implementation")],
        ),
        stage(
            "quality_review",
            StageKind::CodeQualityReview,
            Role::CodeQualityReviewer,
            vec![required("implementation"), required("simplification")],
        ),
        stage(
            "spec_review",
            StageKind::SpecReview,
            Role::SpecReviewer,
            vec![
                required("implementation"),
                required("simplification"),
                required("architecture"),
            ],
        ),
        // Verification runs after the last editing stage and beside the
        // reviews, not after them: the reviews judge the change and the
        // checks judge the build, and neither needs the other's answer. It
        // is listed after them so the scheduler, which takes eligible
        // stages in definition order, lets the reviews go first. The
        // decision waits for all three but only *requires* the reviews:
        // a failed verification must still reach the lead — as the
        // artifact showing the failure — and leave the run completable,
        // so a fix cycle can answer it. What a failed verification can
        // never do is get applied; that gate lives in the workspace.
        stage(
            "verify",
            StageKind::Verify,
            Role::Verifier,
            vec![required("simplification")],
        ),
        stage(
            "decision",
            StageKind::Decision,
            Role::EngineeringLead,
            vec![
                required("quality_review"),
                required("spec_review"),
                optional("verify"),
            ],
        ),
    ]
}

fn deep_stages() -> Vec<StageDefinition> {
    vec![
        stage("research", StageKind::Research, Role::Researcher, vec![]),
        stage(
            "architecture",
            StageKind::Architecture,
            Role::Architect,
            vec![required("research")],
        ),
        stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            vec![required("architecture")],
        ),
        stage(
            "simplification",
            StageKind::Simplification,
            Role::Simplifier,
            vec![required("implementation")],
        ),
        stage(
            "quality_review",
            StageKind::CodeQualityReview,
            Role::CodeQualityReviewer,
            vec![required("implementation"), required("simplification")],
        ),
        stage(
            "spec_review",
            StageKind::SpecReview,
            Role::SpecReviewer,
            vec![
                required("implementation"),
                required("simplification"),
                required("architecture"),
            ],
        ),
        // Verification runs after the last editing stage and beside the
        // reviews, not after them: the reviews judge the change and the
        // checks judge the build, and neither needs the other's answer. It
        // is listed after them so the scheduler, which takes eligible
        // stages in definition order, lets the reviews go first. The
        // decision waits for all three but only *requires* the reviews:
        // a failed verification must still reach the lead — as the
        // artifact showing the failure — and leave the run completable,
        // so a fix cycle can answer it. What a failed verification can
        // never do is get applied; that gate lives in the workspace.
        stage(
            "verify",
            StageKind::Verify,
            Role::Verifier,
            vec![required("simplification")],
        ),
        stage(
            "decision",
            StageKind::Decision,
            Role::EngineeringLead,
            vec![
                required("quality_review"),
                required("spec_review"),
                optional("verify"),
            ],
        ),
    ]
}

fn lead_stages() -> Vec<StageDefinition> {
    vec![stage(
        "lead_1",
        StageKind::Lead,
        Role::EngineeringLead,
        vec![],
    )]
}

fn lead_id(index: u32) -> StageId {
    StageId::new(format!("lead_{index}")).expect("lead stage ID must remain valid")
}

/// The stage of one further exchange with a mission's lead: it depends on
/// the previous turn, so the lead always answers with its earlier answer in
/// evidence and never two turns at once.
#[must_use]
pub fn lead_turn_stages(index: u32, after: &StageId) -> Vec<StageDefinition> {
    vec![StageDefinition::new(
        lead_id(index),
        StageKind::Lead,
        Role::EngineeringLead,
        vec![Dependency::required(after.clone())],
    )]
}

/// The stage identity the next lead turn on this workflow would use, for
/// the same reason as [`next_follow_up_stage_id`]: the user's message is
/// persisted under it before the stage exists.
#[must_use]
pub fn next_lead_stage_id(workflow: &WorkflowDefinition) -> Option<StageId> {
    let count = workflow
        .stages()
        .iter()
        .filter(|stage| stage.kind() == StageKind::Lead)
        .count();
    let index = u32::try_from(count).ok()?.checked_add(1)?;
    Some(lead_id(index))
}

fn review_stages() -> Vec<StageDefinition> {
    vec![
        stage("research", StageKind::Research, Role::Researcher, vec![]),
        stage(
            "quality_review",
            StageKind::CodeQualityReview,
            Role::CodeQualityReviewer,
            vec![required("research")],
        ),
        stage(
            "spec_review",
            StageKind::SpecReview,
            Role::SpecReviewer,
            vec![required("research")],
        ),
        stage(
            "synthesis",
            StageKind::Synthesis,
            Role::EngineeringLead,
            vec![optional("quality_review"), optional("spec_review")],
        ),
        stage(
            "decision",
            StageKind::Decision,
            Role::EngineeringLead,
            vec![required("synthesis")],
        ),
    ]
}

/// The stages of one remediation cycle: an implementer fix, a verification
/// of the result, then a fresh decision over both.
///
/// The fix depends on the decision it is answering, so it cannot start before
/// that verdict exists. The new decision depends on the fix, so no verdict is
/// ever recorded over work that has not happened. Nothing here re-runs the
/// reviews: the lead reads the new diff against findings it already has, and
/// an operator who wants the full review back can say so by starting a review
/// run over the result. Verification is re-run, because a fix is an edit and
/// every edit has to pass the repository's own checks before the run can
/// complete.
///
/// # Panics
/// Panics only if `index` could produce an invalid stage identity, which it
/// cannot: the identities are built from a decimal integer.
#[must_use]
pub fn fix_cycle_stages(index: u32, after_decision: &StageId) -> Vec<StageDefinition> {
    let fix_id = StageId::new(format!("fix_{index}")).expect("fix stage ID must remain valid");
    let verify_id =
        StageId::new(format!("verify_{index}")).expect("verify stage ID must remain valid");
    let decision_id =
        StageId::new(format!("decision_{index}")).expect("decision stage ID must remain valid");
    vec![
        StageDefinition::new(
            fix_id.clone(),
            StageKind::Fix,
            Role::Implementer,
            vec![Dependency::required(after_decision.clone())],
        ),
        StageDefinition::new(
            verify_id.clone(),
            StageKind::Verify,
            Role::Verifier,
            vec![Dependency::required(fix_id.clone())],
        ),
        StageDefinition::new(
            decision_id,
            StageKind::Decision,
            Role::EngineeringLead,
            // The fix and the prior verdict are required: the fix alone
            // does not say what it was meant to resolve, and the verdict
            // alone does not say whether it was. Verification is optional
            // so a fix that still fails the checks reaches a decision — and
            // another fix — instead of a dead run; the apply gate keeps it
            // from going anywhere until a later verification passes.
            vec![
                Dependency::required(fix_id),
                Dependency::optional(verify_id),
                Dependency::required(after_decision.clone()),
            ],
        ),
    ]
}

/// Identity of the follow-up stage one continue cycle at this index would
/// use. Kept in one place so [`continue_cycle_stages`] and
/// [`next_follow_up_stage_id`] can never disagree about it.
fn follow_up_id(index: u32) -> StageId {
    StageId::new(format!("followup_{index}")).expect("follow-up stage ID must remain valid")
}

/// The stages of one continue cycle: an implementer follow-up stage carrying
/// the operator's own instruction, a verification of the result, then a
/// fresh decision over both.
///
/// Named and numbered independently from [`fix_cycle_stages`] — `followup_N`,
/// `followup_verify_N` and `followup_decision_N` rather than `fix_N`,
/// `verify_N` and `decision_N` — so a run that grows both kinds of cycle, in
/// either order, never collides two stages onto the same identity. The
/// dependency wiring mirrors the fix cycle exactly: the follow-up depends on
/// the decision it grows from, verification depends on the follow-up, and
/// the new decision requires the follow-up, its verification and that same
/// prior decision, so no verdict is ever recorded over work that has not
/// happened or has not been checked.
///
/// # Panics
/// Panics only if `index` could produce an invalid stage identity, which it
/// cannot: the identities are built from a decimal integer.
#[must_use]
pub fn continue_cycle_stages(index: u32, after_decision: &StageId) -> Vec<StageDefinition> {
    let follow_up_id = follow_up_id(index);
    let verify_id = StageId::new(format!("followup_verify_{index}"))
        .expect("follow-up verify stage ID must remain valid");
    let decision_id = StageId::new(format!("followup_decision_{index}"))
        .expect("follow-up decision stage ID must remain valid");
    vec![
        StageDefinition::new(
            follow_up_id.clone(),
            StageKind::FollowUp,
            Role::Implementer,
            vec![Dependency::required(after_decision.clone())],
        ),
        StageDefinition::new(
            verify_id.clone(),
            StageKind::Verify,
            Role::Verifier,
            vec![Dependency::required(follow_up_id.clone())],
        ),
        StageDefinition::new(
            decision_id,
            StageKind::Decision,
            Role::EngineeringLead,
            vec![
                Dependency::required(follow_up_id),
                Dependency::optional(verify_id),
                Dependency::required(after_decision.clone()),
            ],
        ),
    ]
}

/// The follow-up stage identity the next continue cycle on this workflow
/// would use, without appending anything.
///
/// A continue request has to persist the operator's instruction under this
/// exact key before the stage exists, because the instruction is immutable
/// run-private content rather than domain event data — the same reason an
/// attention response is written before the resolution it answers commits.
/// [`Run::request_continue`](super::Run::request_continue) independently
/// derives the identical identity when it actually appends the cycle, from
/// the same count of existing `FollowUp` stages, so the two can never
/// address different stages. `None` only on an unreachable stage-count
/// overflow, which `Run::request_continue` also refuses.
#[must_use]
pub fn next_follow_up_stage_id(workflow: &WorkflowDefinition) -> Option<StageId> {
    let count = workflow
        .stages()
        .iter()
        .filter(|stage| stage.kind() == StageKind::FollowUp)
        .count();
    let index = u32::try_from(count).ok()?.checked_add(1)?;
    Some(follow_up_id(index))
}

fn stage(id: &str, kind: StageKind, role: Role, dependencies: Vec<Dependency>) -> StageDefinition {
    StageDefinition::new(
        StageId::new(id).expect("built-in stage ID must remain valid"),
        kind,
        role,
        dependencies,
    )
}

fn required(id: &str) -> Dependency {
    Dependency::required(StageId::new(id).expect("built-in dependency ID must remain valid"))
}

fn optional(id: &str) -> Dependency {
    Dependency::optional(StageId::new(id).expect("built-in dependency ID must remain valid"))
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WorkflowDefinitionError {
    #[error("workflow must contain at least one stage")]
    NoStages,
    #[error("duplicate stage ID: {0}")]
    DuplicateStageId(StageId),
    #[error("stage {stage_id} depends on itself")]
    SelfDependency { stage_id: StageId },
    #[error("stage {stage_id} repeats dependency {dependency_id}")]
    DuplicateDependency {
        stage_id: StageId,
        dependency_id: StageId,
    },
    #[error("stage {stage_id} references unknown dependency {dependency_id}")]
    UnknownDependency {
        stage_id: StageId,
        dependency_id: StageId,
    },
    #[error("workflow contains a dependency cycle")]
    DependencyCycle,
    #[error("validated dependency graph lost stage {0}")]
    InconsistentGraph(StageId),
}

fn validate_stages(stages: &[StageDefinition]) -> Result<(), WorkflowDefinitionError> {
    if stages.is_empty() {
        return Err(WorkflowDefinitionError::NoStages);
    }

    let mut known = HashSet::new();
    for stage in stages {
        if !known.insert(stage.id.clone()) {
            return Err(WorkflowDefinitionError::DuplicateStageId(stage.id.clone()));
        }
    }

    for stage in stages {
        let mut dependencies = HashSet::new();
        for dependency in &stage.dependencies {
            if dependency.stage_id == stage.id {
                return Err(WorkflowDefinitionError::SelfDependency {
                    stage_id: stage.id.clone(),
                });
            }
            if !dependencies.insert(dependency.stage_id.clone()) {
                return Err(WorkflowDefinitionError::DuplicateDependency {
                    stage_id: stage.id.clone(),
                    dependency_id: dependency.stage_id.clone(),
                });
            }
            if !known.contains(&dependency.stage_id) {
                return Err(WorkflowDefinitionError::UnknownDependency {
                    stage_id: stage.id.clone(),
                    dependency_id: dependency.stage_id.clone(),
                });
            }
        }
    }

    validate_acyclic(stages)
}

fn validate_acyclic(stages: &[StageDefinition]) -> Result<(), WorkflowDefinitionError> {
    let mut indegree = stages
        .iter()
        .map(|stage| (stage.id.clone(), stage.dependencies.len()))
        .collect::<HashMap<_, _>>();
    let mut dependents = HashMap::<StageId, Vec<StageId>>::new();
    for stage in stages {
        for dependency in &stage.dependencies {
            dependents
                .entry(dependency.stage_id.clone())
                .or_default()
                .push(stage.id.clone());
        }
    }

    let mut ready = indegree
        .iter()
        .filter_map(|(stage_id, count)| (*count == 0).then_some(stage_id.clone()))
        .collect::<VecDeque<_>>();
    let mut visited = 0;
    while let Some(stage_id) = ready.pop_front() {
        visited += 1;
        if let Some(children) = dependents.get(&stage_id) {
            for child in children {
                let count = indegree
                    .get_mut(child)
                    .ok_or_else(|| WorkflowDefinitionError::InconsistentGraph(child.clone()))?;
                *count -= 1;
                if *count == 0 {
                    ready.push_back(child.clone());
                }
            }
        }
    }

    if visited == stages.len() {
        Ok(())
    } else {
        Err(WorkflowDefinitionError::DependencyCycle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lead session is one stage, and every further turn is one more
    /// stage after the last, named by its number.
    #[test]
    fn a_lead_session_grows_one_turn_at_a_time() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Lead);
        assert_eq!(workflow.stages().len(), 1);
        assert_eq!(workflow.stages()[0].kind(), StageKind::Lead);
        assert!(!workflow.requires_writable_workspace());
        let next = next_lead_stage_id(&workflow).unwrap();
        assert_eq!(next.to_string(), "lead_2");
        let grown = workflow
            .extended(lead_turn_stages(2, workflow.stages()[0].id()))
            .unwrap();
        assert_eq!(grown.stages().len(), 2);
        assert_eq!(grown.stages()[1].id(), &next);
        assert_eq!(next_lead_stage_id(&grown).unwrap().to_string(), "lead_3");
    }

    fn id(value: &str) -> StageId {
        StageId::new(value).unwrap()
    }

    fn stage(value: &str, dependencies: Vec<Dependency>) -> StageDefinition {
        StageDefinition::new(id(value), StageKind::Review, Role::Reviewer, dependencies)
    }

    #[test]
    fn required_and_optional_dependencies_are_preserved() {
        let definition = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                stage("analysis", vec![]),
                stage("independent", vec![]),
                stage(
                    "synthesis",
                    vec![
                        Dependency::required(id("analysis")),
                        Dependency::optional(id("independent")),
                    ],
                ),
            ],
        )
        .unwrap();

        assert_eq!(definition.stages()[2].dependencies().len(), 2);
        assert_eq!(
            definition.stages()[2].dependencies()[1].kind(),
            DependencyKind::Optional
        );
    }

    #[test]
    fn invalid_workflow_definitions_are_rejected() {
        assert_eq!(
            WorkflowDefinition::new(WorkflowKind::Fast, vec![]),
            Err(WorkflowDefinitionError::NoStages)
        );

        let duplicate = stage("same", vec![]);
        assert_eq!(
            WorkflowDefinition::new(WorkflowKind::Fast, vec![duplicate.clone(), duplicate]),
            Err(WorkflowDefinitionError::DuplicateStageId(id("same")))
        );

        assert_eq!(
            WorkflowDefinition::new(
                WorkflowKind::Fast,
                vec![stage("self", vec![Dependency::required(id("self"))])]
            ),
            Err(WorkflowDefinitionError::SelfDependency {
                stage_id: id("self")
            })
        );

        assert_eq!(
            WorkflowDefinition::new(
                WorkflowKind::Fast,
                vec![stage("known", vec![Dependency::required(id("missing"))])]
            ),
            Err(WorkflowDefinitionError::UnknownDependency {
                stage_id: id("known"),
                dependency_id: id("missing")
            })
        );
    }

    #[test]
    fn duplicate_dependencies_and_cycles_are_rejected() {
        assert_eq!(
            WorkflowDefinition::new(
                WorkflowKind::Review,
                vec![
                    stage("source", vec![]),
                    stage(
                        "consumer",
                        vec![
                            Dependency::required(id("source")),
                            Dependency::optional(id("source")),
                        ],
                    ),
                ],
            ),
            Err(WorkflowDefinitionError::DuplicateDependency {
                stage_id: id("consumer"),
                dependency_id: id("source")
            })
        );

        assert_eq!(
            WorkflowDefinition::new(
                WorkflowKind::Review,
                vec![
                    stage("one", vec![Dependency::required(id("two"))]),
                    stage("two", vec![Dependency::required(id("one"))]),
                ],
            ),
            Err(WorkflowDefinitionError::DependencyCycle)
        );
    }

    #[test]
    fn built_in_review_is_a_data_driven_fan_out_and_join() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Review);
        let quality = workflow.stage(&id("quality_review")).unwrap();
        let spec = workflow.stage(&id("spec_review")).unwrap();
        let synthesis = workflow.stage(&id("synthesis")).unwrap();

        assert_eq!(quality.kind(), StageKind::CodeQualityReview);
        assert_eq!(quality.role(), Role::CodeQualityReviewer);
        assert_eq!(
            quality.dependencies(),
            &[Dependency::required(id("research"))]
        );
        assert_eq!(spec.kind(), StageKind::SpecReview);
        assert_eq!(spec.role(), Role::SpecReviewer);
        assert_eq!(spec.dependencies(), &[Dependency::required(id("research"))]);
        assert_eq!(
            synthesis.dependencies(),
            &[
                Dependency::optional(id("quality_review")),
                Dependency::optional(id("spec_review")),
            ]
        );
    }

    #[test]
    fn new_built_ins_specialize_reviewers_and_direct_artifact_dependencies() {
        let fast = WorkflowDefinition::built_in(WorkflowKind::Fast);
        assert_eq!(fast.stages().len(), 2);
        assert_eq!(fast.stages()[0].id(), &id("implementation"));

        let standard = WorkflowDefinition::built_in(WorkflowKind::Standard);
        assert!(
            standard
                .stage(&id("architecture"))
                .unwrap()
                .dependencies()
                .is_empty()
        );
        let deep = WorkflowDefinition::built_in(WorkflowKind::Deep);
        assert!(
            deep.stage(&id("research"))
                .unwrap()
                .dependencies()
                .is_empty()
        );
        assert_eq!(
            deep.stage(&id("architecture")).unwrap().dependencies(),
            &[Dependency::required(id("research"))]
        );

        for kind in [WorkflowKind::Standard, WorkflowKind::Deep] {
            let workflow = WorkflowDefinition::built_in(kind);
            assert!(workflow.stage(&id("review")).is_none());
            let simplification = workflow.stage(&id("simplification")).unwrap();
            let quality = workflow.stage(&id("quality_review")).unwrap();
            let spec = workflow.stage(&id("spec_review")).unwrap();
            let decision = workflow.stage(&id("decision")).unwrap();
            assert_eq!(simplification.kind(), StageKind::Simplification);
            assert_eq!(simplification.role(), Role::Simplifier);
            assert_eq!(
                simplification.dependencies(),
                &[Dependency::required(id("implementation"))]
            );
            assert_eq!(quality.kind(), StageKind::CodeQualityReview);
            assert_eq!(quality.role(), Role::CodeQualityReviewer);
            assert_eq!(
                quality.dependencies(),
                &[
                    Dependency::required(id("implementation")),
                    Dependency::required(id("simplification")),
                ]
            );
            assert_eq!(spec.kind(), StageKind::SpecReview);
            assert_eq!(spec.role(), Role::SpecReviewer);
            assert_eq!(
                spec.dependencies(),
                &[
                    Dependency::required(id("implementation")),
                    Dependency::required(id("simplification")),
                    Dependency::required(id("architecture")),
                ]
            );
            assert_eq!(
                decision.dependencies(),
                &[
                    Dependency::required(id("quality_review")),
                    Dependency::required(id("spec_review")),
                    Dependency::optional(id("verify")),
                ]
            );
        }
    }

    /// Verification follows the last stage that edits the worktree; the
    /// decision waits for it but does not require it, and the reviews keep
    /// their own dependencies and run beside it. A review run edits
    /// nothing, so it has nothing to verify.
    #[test]
    fn built_ins_verify_after_the_last_editing_stage_and_before_the_decision() {
        let fast = WorkflowDefinition::built_in(WorkflowKind::Fast);
        let verify = fast.stage(&id("verify")).unwrap();
        assert_eq!(verify.kind(), StageKind::Verify);
        assert_eq!(verify.role(), Role::Verifier);
        assert_eq!(
            verify.dependencies(),
            &[Dependency::required(id("implementation"))]
        );

        for kind in [WorkflowKind::Standard, WorkflowKind::Deep] {
            let workflow = WorkflowDefinition::built_in(kind);
            let verify = workflow.stage(&id("verify")).unwrap();
            assert_eq!(verify.kind(), StageKind::Verify);
            assert_eq!(verify.role(), Role::Verifier);
            assert_eq!(
                verify.dependencies(),
                &[Dependency::required(id("simplification"))]
            );
            let decision = workflow.stage(&id("decision")).unwrap();
            assert!(
                decision
                    .dependencies()
                    .contains(&Dependency::optional(id("verify"))),
                "the decision waits for verification but a failed one still reaches it"
            );
            for review in ["quality_review", "spec_review"] {
                let review = workflow.stage(&id(review)).unwrap();
                assert!(
                    review
                        .dependencies()
                        .iter()
                        .all(|dependency| dependency.stage_id() != &id("verify")),
                    "reviews run beside verification, not after it"
                );
            }
        }

        let review = WorkflowDefinition::built_in(WorkflowKind::Review);
        assert!(
            review
                .stages()
                .iter()
                .all(|stage| stage.kind() != StageKind::Verify)
        );
    }

    /// Evaluations run the built-ins without their verification stages;
    /// what remains is the pre-verification graph exactly.
    #[test]
    fn without_verification_drops_the_verify_stages_and_the_edges_into_them() {
        let fast = WorkflowDefinition::built_in(WorkflowKind::Fast)
            .without_verification()
            .unwrap();
        assert_eq!(fast.stages().len(), 1);
        assert_eq!(fast.stages()[0].id(), &id("implementation"));

        let standard = WorkflowDefinition::built_in(WorkflowKind::Standard)
            .without_verification()
            .unwrap();
        assert!(standard.stage(&id("verify")).is_none());
        assert_eq!(
            standard.stage(&id("decision")).unwrap().dependencies(),
            &[
                Dependency::required(id("quality_review")),
                Dependency::required(id("spec_review")),
            ]
        );

        let review = WorkflowDefinition::built_in(WorkflowKind::Review);
        assert_eq!(review.without_verification().unwrap(), review);

        let only_verify = WorkflowDefinition::new(
            WorkflowKind::Fast,
            vec![StageDefinition::new(
                id("verify"),
                StageKind::Verify,
                Role::Verifier,
                vec![],
            )],
        )
        .unwrap();
        assert_eq!(
            only_verify.without_verification(),
            Err(WorkflowDefinitionError::NoStages)
        );
    }

    /// Additive like `Simplification` before it: the new variant round-trips
    /// through the same inspectable snake-case shape, changing no persisted
    /// schema.
    #[test]
    fn verify_stage_kind_round_trips_through_snake_case() {
        assert_eq!(
            serde_json::to_string(&StageKind::Verify).unwrap(),
            "\"verify\""
        );
        assert_eq!(
            serde_json::from_str::<StageKind>("\"verify\"").unwrap(),
            StageKind::Verify
        );
    }

    /// A fix cycle verifies the fix before the fresh decision judges it,
    /// and the decision is reached even when that verification fails.
    #[test]
    fn a_fix_cycle_verifies_the_fix_between_it_and_the_fresh_decision() {
        let after = id("decision");
        let stages = fix_cycle_stages(1, &after);

        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].id(), &id("fix_1"));
        let verify = &stages[1];
        assert_eq!(verify.id(), &id("verify_1"));
        assert_eq!(verify.kind(), StageKind::Verify);
        assert_eq!(verify.role(), Role::Verifier);
        assert_eq!(verify.dependencies(), &[Dependency::required(id("fix_1"))]);
        let decision = &stages[2];
        assert_eq!(decision.id(), &id("decision_1"));
        assert_eq!(
            decision.dependencies(),
            &[
                Dependency::required(id("fix_1")),
                Dependency::optional(id("verify_1")),
                Dependency::required(after),
            ]
        );
    }

    #[test]
    fn stage_kind_serialization_preserves_legacy_and_specialized_values() {
        assert_eq!(
            serde_json::from_str::<StageKind>("\"review\"").unwrap(),
            StageKind::Review
        );
        assert_eq!(
            serde_json::to_string(&StageKind::CodeQualityReview).unwrap(),
            "\"code_quality_review\""
        );
        assert_eq!(
            serde_json::to_string(&StageKind::SpecReview).unwrap(),
            "\"spec_review\""
        );
    }

    /// Additive like `Fix` before it: the new variant round-trips through
    /// the same inspectable snake-case shape, changing no persisted schema.
    #[test]
    fn simplification_stage_kind_round_trips_through_snake_case() {
        assert_eq!(
            serde_json::to_string(&StageKind::Simplification).unwrap(),
            "\"simplification\""
        );
        assert_eq!(
            serde_json::from_str::<StageKind>("\"simplification\"").unwrap(),
            StageKind::Simplification
        );
    }

    /// Additive like `Fix` before it: the new variant round-trips through
    /// the same inspectable snake-case shape, changing no persisted schema.
    #[test]
    fn follow_up_stage_kind_round_trips_through_snake_case() {
        assert_eq!(
            serde_json::to_string(&StageKind::FollowUp).unwrap(),
            "\"follow_up\""
        );
        assert_eq!(
            serde_json::from_str::<StageKind>("\"follow_up\"").unwrap(),
            StageKind::FollowUp
        );
    }

    #[test]
    fn every_built_in_is_a_valid_nonempty_dag() {
        for kind in [
            WorkflowKind::Fast,
            WorkflowKind::Standard,
            WorkflowKind::Deep,
            WorkflowKind::Review,
        ] {
            let workflow = WorkflowDefinition::built_in(kind);
            assert_eq!(workflow.kind(), kind);
            assert!(!workflow.stages().is_empty());
        }
    }

    #[test]
    fn writable_workspace_capability_depends_on_stage_semantics_not_workflow_name() {
        let read_only =
            WorkflowDefinition::new(WorkflowKind::Standard, vec![stage("review", vec![])]).unwrap();
        let mutating = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![StageDefinition::new(
                id("fix"),
                StageKind::Fix,
                Role::Implementer,
                vec![],
            )],
        )
        .unwrap();
        let follow_up = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![StageDefinition::new(
                id("followup_1"),
                StageKind::FollowUp,
                Role::Implementer,
                vec![],
            )],
        )
        .unwrap();

        assert!(!read_only.requires_writable_workspace());
        assert!(mutating.requires_writable_workspace());
        assert!(follow_up.requires_writable_workspace());
    }

    /// A continue cycle wires exactly like a fix cycle, but under identities
    /// that cannot collide with it: `followup_N`/`followup_decision_N` rather
    /// than `fix_N`/`decision_N`.
    #[test]
    fn a_continue_cycle_wires_a_follow_up_stage_and_a_fresh_decision_over_it() {
        let after = id("decision");
        let stages = continue_cycle_stages(1, &after);

        assert_eq!(stages.len(), 3);
        let follow_up = &stages[0];
        assert_eq!(follow_up.id(), &id("followup_1"));
        assert_eq!(follow_up.kind(), StageKind::FollowUp);
        assert_eq!(follow_up.role(), Role::Implementer);
        assert_eq!(
            follow_up.dependencies(),
            &[Dependency::required(after.clone())]
        );

        let verify = &stages[1];
        assert_eq!(verify.id(), &id("followup_verify_1"));
        assert_eq!(verify.kind(), StageKind::Verify);
        assert_eq!(verify.role(), Role::Verifier);
        assert_eq!(
            verify.dependencies(),
            &[Dependency::required(id("followup_1"))]
        );

        let decision = &stages[2];
        assert_eq!(decision.id(), &id("followup_decision_1"));
        assert_eq!(decision.kind(), StageKind::Decision);
        assert_eq!(decision.role(), Role::EngineeringLead);
        assert_eq!(
            decision.dependencies(),
            &[
                Dependency::required(id("followup_1")),
                Dependency::optional(id("followup_verify_1")),
                Dependency::required(after),
            ]
        );
    }

    /// A fix cycle and a continue cycle over the very same decision must not
    /// address the same stage.
    #[test]
    fn fix_and_continue_cycles_over_the_same_decision_never_collide() {
        let after = id("decision");
        let fix_ids = fix_cycle_stages(1, &after)
            .iter()
            .map(StageDefinition::id)
            .cloned()
            .collect::<Vec<_>>();
        let continue_ids = continue_cycle_stages(1, &after)
            .iter()
            .map(StageDefinition::id)
            .cloned()
            .collect::<Vec<_>>();

        assert!(fix_ids.iter().all(|id| !continue_ids.contains(id)));
    }

    /// The identity a caller predicts before the cycle exists must be the
    /// same one the cycle actually uses, on the first cycle and after one
    /// already grew.
    #[test]
    fn next_follow_up_stage_id_predicts_what_a_continue_cycle_would_append() {
        let base = WorkflowDefinition::new(WorkflowKind::Standard, vec![stage("decision", vec![])])
            .unwrap();
        assert_eq!(
            next_follow_up_stage_id(&base),
            Some(id("followup_1")),
            "no follow-up cycle exists yet"
        );

        let grown = base
            .extended(continue_cycle_stages(1, &id("decision")))
            .unwrap();
        assert_eq!(
            next_follow_up_stage_id(&grown),
            Some(id("followup_2")),
            "one cycle already grew, so the next one is the second"
        );
    }
}
