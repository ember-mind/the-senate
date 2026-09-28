use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    AttentionError, AttentionRequest, AttentionRequestId, AttentionStatus, ConfigSnapshotId,
    DependencyKind, DomainEvent, DomainEventKind, EventMetadata, RunId, RunRehydrationData,
    RunResumeStatus, Stage, StageId, StageKind, StageRehydrationError, StageRouteOverride,
    StageStatus, StageTransition, StageTransitionError, WorkflowDefinition,
    WorkflowDefinitionError, WorkflowKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Created,
    Preparing,
    Ready,
    Running,
    NeedsUser,
    Paused,
    Interrupted,
    Completed,
    Applied,
    Discarded,
    Failed,
}

impl RunStatus {
    #[must_use]
    pub const fn is_execution_finished(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Applied | Self::Discarded | Self::Failed
        )
    }

    #[must_use]
    pub const fn is_lifecycle_closed(self) -> bool {
        matches!(self, Self::Applied | Self::Discarded)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTransition {
    BeginPreparation,
    FinishPreparation,
    Start,
    Pause,
    Interrupt,
    Resume,
    Recover,
    Complete,
    Fail,
    Apply,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum ResumableRunStatus {
    Running,
    NeedsUser,
}

impl ResumableRunStatus {
    const fn into_status(self) -> RunStatus {
        match self {
            Self::Running => RunStatus::Running,
            Self::NeedsUser => RunStatus::NeedsUser,
        }
    }
}

impl TryFrom<RunStatus> for ResumableRunStatus {
    type Error = ();

    fn try_from(status: RunStatus) -> Result<Self, Self::Error> {
        match status {
            RunStatus::Running => Ok(Self::Running),
            RunStatus::NeedsUser => Ok(Self::NeedsUser),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Run {
    id: RunId,
    workflow: WorkflowDefinition,
    config_snapshot_id: ConfigSnapshotId,
    status: RunStatus,
    suspended_from: Option<ResumableRunStatus>,
    stages: Vec<Stage>,
    attention_requests: Vec<AttentionRequest>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Run {
    /// Creates one run from a validated workflow definition.
    #[must_use]
    pub fn new(
        id: RunId,
        workflow: WorkflowDefinition,
        config_snapshot_id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> Self {
        let stages = workflow
            .stages()
            .iter()
            .map(|definition| Stage::from_definition(id, definition))
            .collect();
        Self {
            id,
            workflow,
            config_snapshot_id,
            status: RunStatus::Created,
            suspended_from: None,
            stages,
            attention_requests: Vec::new(),
            created_at,
            updated_at: created_at,
        }
    }

    /// Reconstructs one existing aggregate and validates current-state invariants.
    ///
    /// This intentionally does not replay lifecycle transitions. Persisted data
    /// remains untrusted until this method returns successfully.
    ///
    /// # Errors
    /// Rejects malformed workflows, mismatched stage state, and inconsistent
    /// lifecycle, attention, suspension, or timestamp state.
    pub fn rehydrate(data: RunRehydrationData) -> Result<Self, RunRehydrationError> {
        let workflow = WorkflowDefinition::new(data.workflow_kind, data.stage_definitions)?;
        let mut stage_states = HashMap::new();
        for stage in data.stages {
            let stage_id = stage.id.clone();
            if stage_states.insert(stage_id.clone(), stage).is_some() {
                return Err(RunRehydrationError::DuplicateStageState(stage_id));
            }
        }
        let mut stages = Vec::with_capacity(workflow.stages().len());
        for definition in workflow.stages() {
            let stage_id = definition.id().clone();
            let state = stage_states
                .remove(&stage_id)
                .ok_or_else(|| RunRehydrationError::MissingStageState(stage_id.clone()))?;
            stages.push(
                Stage::rehydrate(data.id, definition, &state).map_err(|source| {
                    RunRehydrationError::InvalidStageState {
                        stage_id: stage_id.clone(),
                        source,
                    }
                })?,
            );
        }
        if let Some(stage_id) = stage_states.into_keys().next() {
            return Err(RunRehydrationError::UnknownStageState(stage_id));
        }

        let run = Self {
            id: data.id,
            workflow,
            config_snapshot_id: data.config_snapshot_id,
            status: data.status,
            suspended_from: data.suspended_from.map(|status| match status {
                RunResumeStatus::Running => ResumableRunStatus::Running,
                RunResumeStatus::NeedsUser => ResumableRunStatus::NeedsUser,
            }),
            stages,
            attention_requests: data.attention_requests,
            created_at: data.created_at,
            updated_at: data.updated_at,
        };
        run.validate_invariants()?;
        Ok(run)
    }

    /// Captures persistence-neutral state for a versioned external snapshot.
    #[must_use]
    pub fn rehydration_data(&self) -> RunRehydrationData {
        RunRehydrationData {
            id: self.id,
            workflow_kind: self.workflow.kind(),
            stage_definitions: self.workflow.stages().to_vec(),
            config_snapshot_id: self.config_snapshot_id.clone(),
            status: self.status,
            suspended_from: self.suspended_from.map(|status| match status {
                ResumableRunStatus::Running => RunResumeStatus::Running,
                ResumableRunStatus::NeedsUser => RunResumeStatus::NeedsUser,
            }),
            stages: self.stages.iter().map(Stage::rehydration_data).collect(),
            attention_requests: self.attention_requests.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    #[must_use]
    pub const fn id(&self) -> RunId {
        self.id
    }

    #[must_use]
    pub const fn workflow_kind(&self) -> WorkflowKind {
        self.workflow.kind()
    }

    #[must_use]
    pub const fn workflow(&self) -> &WorkflowDefinition {
        &self.workflow
    }

    #[must_use]
    pub const fn config_snapshot_id(&self) -> &ConfigSnapshotId {
        &self.config_snapshot_id
    }

    #[must_use]
    pub const fn status(&self) -> RunStatus {
        self.status
    }

    #[must_use]
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    #[must_use]
    pub fn stage(&self, stage_id: &StageId) -> Option<&Stage> {
        self.stages.iter().find(|stage| stage.id() == stage_id)
    }

    /// Which stages, if any, presently stand between this run and
    /// completion: one that has not reached a terminal outcome yet, or a
    /// failed one the workflow graph cannot route around — a leaf with no
    /// dependents, or one with a dependent that requires it rather than
    /// merely tolerating its absence. A failed stage whose only dependents
    /// treat it as optional is not a blocker: the workflow already planned
    /// for exactly this, and something downstream is expected to carry on in
    /// degraded mode.
    ///
    /// Read-only and side-effect-free, unlike the identical check
    /// `transition(RunTransition::Complete, ..)` runs internally — this is
    /// the only way to ask the question without attempting the transition,
    /// which only a `Running` run can do in the first place. A caller that
    /// wants *the* blocking failed stage of an already-`Failed` run (whose
    /// other stages the fail transition already resolved to a terminal
    /// status) filters this for [`CompletionBlockerReason::FailedLeafStage`]
    /// or [`CompletionBlockerReason::RequiredStageFailed`].
    #[must_use]
    pub fn completion_blockers(&self) -> Vec<CompletionBlocker> {
        let mut blockers = Vec::new();
        for stage in &self.stages {
            if !stage.status().is_terminal_outcome() {
                blockers.push(CompletionBlocker {
                    stage_id: stage.id().clone(),
                    status: stage.status(),
                    reason: CompletionBlockerReason::StageNotTerminal,
                });
                continue;
            }
            if stage.status() != StageStatus::Failed {
                continue;
            }

            let dependent_edges = self
                .stages
                .iter()
                .flat_map(|dependent| {
                    dependent
                        .dependencies()
                        .iter()
                        .filter(move |dependency| dependency.stage_id() == stage.id())
                })
                .collect::<Vec<_>>();
            let reason = if dependent_edges.is_empty() {
                Some(CompletionBlockerReason::FailedLeafStage)
            } else if dependent_edges
                .iter()
                .any(|dependency| dependency.kind() == DependencyKind::Required)
            {
                Some(CompletionBlockerReason::RequiredStageFailed)
            } else {
                None
            };
            if let Some(reason) = reason {
                blockers.push(CompletionBlocker {
                    stage_id: stage.id().clone(),
                    status: stage.status(),
                    reason,
                });
            }
        }
        blockers
    }

    #[must_use]
    pub fn attention_requests(&self) -> &[AttentionRequest] {
        &self.attention_requests
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    #[must_use]
    pub fn created_event(&self, metadata: EventMetadata) -> DomainEvent {
        DomainEvent::new(
            metadata,
            self.id,
            None,
            DomainEventKind::RunCreated {
                workflow: self.workflow.kind(),
            },
        )
    }

    /// Applies one legal run lifecycle transition and returns its semantic event.
    ///
    /// # Errors
    /// Returns [`RunTransitionError`] without mutating run when transition or
    /// completion invariants are invalid.
    pub fn transition(
        &mut self,
        transition: RunTransition,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunTransitionError> {
        let from = self.status;
        let kind = match (from, transition) {
            (RunStatus::Created, RunTransition::BeginPreparation) => {
                self.status = RunStatus::Preparing;
                DomainEventKind::RunPreparationStarted
            }
            (RunStatus::Preparing, RunTransition::FinishPreparation) => {
                self.status = RunStatus::Ready;
                DomainEventKind::RunPrepared
            }
            (RunStatus::Ready, RunTransition::Start) => {
                self.status = RunStatus::Running;
                DomainEventKind::RunStarted
            }
            (RunStatus::Running | RunStatus::NeedsUser, RunTransition::Pause) => {
                self.suspend_run(RunStatus::Paused)?;
                for stage in &mut self.stages {
                    stage.pause_for_run();
                }
                DomainEventKind::RunPaused
            }
            (RunStatus::Running | RunStatus::NeedsUser, RunTransition::Interrupt) => {
                self.suspend_run(RunStatus::Interrupted)?;
                for stage in &mut self.stages {
                    stage.interrupt_for_run();
                }
                DomainEventKind::RunInterrupted
            }
            (RunStatus::Paused, RunTransition::Resume) => {
                self.restore_run()?;
                for stage in &mut self.stages {
                    stage.resume_for_run();
                }
                DomainEventKind::RunResumed
            }
            (RunStatus::Interrupted, RunTransition::Recover) => {
                self.restore_run()?;
                for stage in &mut self.stages {
                    stage.recover_for_run();
                }
                DomainEventKind::RunRecovered
            }
            (RunStatus::Running, RunTransition::Complete) => {
                self.ensure_completion_allowed()?;
                self.status = RunStatus::Completed;
                DomainEventKind::RunCompleted
            }
            (
                RunStatus::Preparing
                | RunStatus::Ready
                | RunStatus::Running
                | RunStatus::NeedsUser
                | RunStatus::Paused
                | RunStatus::Interrupted,
                RunTransition::Fail,
            ) => {
                self.cancel_all_pending(metadata.occurred_at())?;
                self.status = RunStatus::Failed;
                self.suspended_from = None;
                DomainEventKind::RunFailed
            }
            (RunStatus::Completed, RunTransition::Apply) => {
                self.status = RunStatus::Applied;
                DomainEventKind::RunApplied
            }
            (
                RunStatus::Created
                | RunStatus::Preparing
                | RunStatus::Ready
                | RunStatus::Running
                | RunStatus::NeedsUser
                | RunStatus::Paused
                | RunStatus::Interrupted
                | RunStatus::Completed
                | RunStatus::Failed,
                RunTransition::Discard,
            ) => {
                self.cancel_all_pending(metadata.occurred_at())?;
                self.status = RunStatus::Discarded;
                self.suspended_from = None;
                DomainEventKind::RunDiscarded
            }
            _ => {
                return Err(RunTransitionError::InvalidTransition { from, transition });
            }
        };

        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(metadata, self.id, None, kind))
    }

    /// Applies one legal stage transition owned by this active run.
    ///
    /// # Errors
    /// Rejects inactive runs, unknown stages, unsatisfied dependencies, retry
    /// invalidation, and invalid stage lifecycle changes.
    pub fn transition_stage(
        &mut self,
        stage_id: &StageId,
        transition: StageTransition,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunStageError> {
        let retries_failed_run =
            self.status == RunStatus::Failed && transition == StageTransition::Retry;
        if !matches!(self.status, RunStatus::Running | RunStatus::NeedsUser) && !retries_failed_run
        {
            return Err(RunStageError::RunNotActive(self.status));
        }
        let index = self.stage_index(stage_id)?;
        let degraded = if transition == StageTransition::MarkReady {
            self.ensure_stage_ready(index)?
        } else {
            Vec::new()
        };
        if transition == StageTransition::Retry {
            self.ensure_retry_safe(stage_id)?;
        }

        self.stages[index].transition(transition)?;
        if retries_failed_run {
            self.status = RunStatus::Running;
        }
        self.updated_at = metadata.occurred_at();
        let kind = match transition {
            StageTransition::MarkReady => DomainEventKind::StageReady {
                degraded: !degraded.is_empty(),
            },
            StageTransition::Start => DomainEventKind::StageStarted,
            StageTransition::Pause => DomainEventKind::StagePaused,
            StageTransition::Interrupt => DomainEventKind::StageInterrupted,
            StageTransition::Resume => DomainEventKind::StageResumed,
            StageTransition::Recover => DomainEventKind::StageRecovered,
            StageTransition::Complete => DomainEventKind::StageCompleted,
            StageTransition::Skip => DomainEventKind::StageSkipped,
            StageTransition::Fail => DomainEventKind::StageFailed,
            StageTransition::Retry => DomainEventKind::StageRetryScheduled,
        };
        Ok(DomainEvent::new(
            metadata,
            self.id,
            Some(stage_id.clone()),
            kind,
        ))
    }

    /// Sends one failed stage to a different provider before it is retried.
    ///
    /// The route the role was given at creation stays in the immutable
    /// snapshot; the exception lives on the stage and is recorded as its own
    /// event with the operator's reason. It is only ever an operator's act:
    /// The Senate never reroutes on its own, so a provider that went away is a
    /// clear failure until someone decides where the work goes next.
    ///
    /// The same guard as retry applies: a stage whose dependents already
    /// advanced cannot be sent anywhere, because retrying it there would
    /// invalidate them.
    ///
    /// # Errors
    /// Rejects runs that are not failed or active, unknown stages, stages
    /// that are not failed, and stages whose dependents have advanced.
    pub fn override_stage_route(
        &mut self,
        stage_id: &StageId,
        route: StageRouteOverride,
        reason: impl Into<String>,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunStageError> {
        if !matches!(
            self.status,
            RunStatus::Running | RunStatus::NeedsUser | RunStatus::Failed
        ) {
            return Err(RunStageError::RunNotActive(self.status));
        }
        let index = self.stage_index(stage_id)?;
        self.ensure_retry_safe(stage_id)?;
        let kind = DomainEventKind::StageRouteOverridden {
            provider_id: route.provider_id().clone(),
            model_id: route.model_id().cloned(),
            reason: reason.into(),
        };
        self.stages[index].override_route(route)?;
        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            Some(stage_id.clone()),
            kind,
        ))
    }

    /// Records that this run's workspace was moved onto a newer base commit.
    ///
    /// Not a lifecycle transition: the run stays `Completed`, keeps its
    /// stages and its artifacts, and only the commit its delta is read
    /// against has changed. What the event is for is elsewhere — a passed
    /// verification describes checks that ran over the old base, so the apply
    /// gate compares this event's position in the log with the latest
    /// verification's and refuses a run whose checks predate the move.
    ///
    /// # Errors
    /// Rejects runs that have not completed, and runs already applied or
    /// discarded: the first has a workspace still being written, and the
    /// other two have nothing left to apply the moved delta to.
    pub fn workspace_rebased(
        &mut self,
        metadata: EventMetadata,
        from_base: String,
        to_base: String,
    ) -> Result<DomainEvent, RunRebaseError> {
        if self.status != RunStatus::Completed {
            return Err(RunRebaseError::RunNotCompleted(self.status));
        }
        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            None,
            DomainEventKind::WorkspaceRebased { from_base, to_base },
        ))
    }

    /// Records that the run's delta reached `origin`.
    ///
    /// Not a lifecycle transition: publishing is transport, and the run stays
    /// `Completed` so apply, fix and discard all remain available after it.
    ///
    /// # Errors
    /// Rejects runs that have not completed: only a completed run publishes.
    pub fn published(
        &mut self,
        metadata: EventMetadata,
        branch: String,
        commit: String,
        pull_request_url: Option<String>,
    ) -> Result<DomainEvent, RunPublishError> {
        if self.status != RunStatus::Completed {
            return Err(RunPublishError::RunNotCompleted(self.status));
        }
        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            None,
            DomainEventKind::RunPublished {
                branch,
                commit,
                pull_request_url,
            },
        ))
    }

    /// Grows a completed run by one remediation cycle and reopens it.
    ///
    /// A run whose decision the operator rejects has everything the fix needs
    /// already: the workspace with the work in it, the artifacts the reviews
    /// produced, and the verdict itself. Extending that run keeps all of it
    /// under one identity that stays a single thing to apply or discard.
    /// Starting a second run instead would have to adopt this one's workspace,
    /// which is keyed by run, or begin again from a clean checkout.
    ///
    /// This is the second and last place a finished run reopens; the other is
    /// retrying a stage of a failed one. Both are deliberate operator acts, and
    /// neither happens on its own.
    ///
    /// Nothing here reads the decision. The verdict is prose written for a
    /// person, and the run has no basis to classify it, so the operator decides
    /// and this records that they did. The fix stage's context is the verdict
    /// itself, which it depends on, plus the run's immutable task.
    ///
    /// # Errors
    /// Rejects runs that have not completed, runs already applied or discarded,
    /// and cycles that would not form a valid DAG.
    pub fn request_fix(&mut self, metadata: EventMetadata) -> Result<DomainEvent, RunFixError> {
        if self.status != RunStatus::Completed {
            return Err(RunFixError::RunNotCompleted(self.status));
        }
        let last_decision = self
            .workflow
            .stages()
            .iter()
            .rev()
            .find(|stage| stage.kind() == StageKind::Decision)
            .map(|stage| stage.id().clone())
            .ok_or(RunFixError::NoDecisionToAnswer)?;
        let index = u32::try_from(
            self.workflow
                .stages()
                .iter()
                .filter(|stage| stage.kind() == StageKind::Fix)
                .count(),
        )
        .map_err(|_| RunFixError::TooManyCycles)?
            + 1;
        let additional = super::fix_cycle_stages(index, &last_decision);
        let stage_ids = additional
            .iter()
            .map(|stage| stage.id().clone())
            .collect::<Vec<_>>();
        let workflow = self.workflow.extended(additional)?;
        let new_stages = workflow
            .stages()
            .iter()
            .filter(|stage| stage_ids.contains(stage.id()))
            .map(|definition| Stage::from_definition(self.id, definition))
            .collect::<Vec<_>>();
        self.workflow = workflow;
        self.stages.extend(new_stages);
        self.status = RunStatus::Running;
        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            None,
            DomainEventKind::RunFixRequested { stage_ids },
        ))
    }

    /// Grows a completed run by one continue cycle and reopens it.
    ///
    /// The sibling of [`Self::request_fix`]: same shape, same reopening, and
    /// the same reason it exists — the workspace, artifacts, and identity a
    /// fresh run would have to adopt or begin without are already here. It
    /// differs in what the appended implementer stage answers to. A fix
    /// resolves the verdict's own blocking findings; a continue cycle carries
    /// the operator's own instruction instead, so this event carries no
    /// instruction text of its own — same as `request_fix` never reads the
    /// verdict it grows from, the run has no basis to hold that text, and the
    /// operator asking is the whole signal. The instruction reaches the
    /// stage's agent as immutable run-private stdin content, addressed by the
    /// exact stage identity [`super::next_follow_up_stage_id`] predicts.
    ///
    /// # Errors
    /// Rejects runs that have not completed, runs already applied or
    /// discarded, and cycles that would not form a valid DAG.
    pub fn request_continue(
        &mut self,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunFixError> {
        if self.status != RunStatus::Completed {
            return Err(RunFixError::RunNotCompleted(self.status));
        }
        let last_decision = self
            .workflow
            .stages()
            .iter()
            .rev()
            .find(|stage| stage.kind() == StageKind::Decision)
            .map(|stage| stage.id().clone())
            .ok_or(RunFixError::NoDecisionToAnswer)?;
        let index = u32::try_from(
            self.workflow
                .stages()
                .iter()
                .filter(|stage| stage.kind() == StageKind::FollowUp)
                .count(),
        )
        .map_err(|_| RunFixError::TooManyCycles)?
            + 1;
        let additional = super::continue_cycle_stages(index, &last_decision);
        let stage_ids = additional
            .iter()
            .map(|stage| stage.id().clone())
            .collect::<Vec<_>>();
        let workflow = self.workflow.extended(additional)?;
        let new_stages = workflow
            .stages()
            .iter()
            .filter(|stage| stage_ids.contains(stage.id()))
            .map(|definition| Stage::from_definition(self.id, definition))
            .collect::<Vec<_>>();
        self.workflow = workflow;
        self.stages.extend(new_stages);
        self.status = RunStatus::Running;
        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            None,
            DomainEventKind::RunContinueRequested { stage_ids },
        ))
    }

    /// Appends one more turn to a lead session: a lead stage that depends
    /// on the previous one. The run's status returns to `Running` so the
    /// scheduler picks the turn up; the user's message is stage-private
    /// content the provider embeds, persisted by the caller under the
    /// identity [`super::next_lead_stage_id`] predicts.
    ///
    /// # Errors
    /// Rejects runs that have not completed, runs with no lead stage, and
    /// turns that cannot be numbered.
    pub fn request_lead_turn(
        &mut self,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunFixError> {
        if self.status != RunStatus::Completed {
            return Err(RunFixError::RunNotCompleted(self.status));
        }
        let previous = self
            .workflow
            .stages()
            .iter()
            .rev()
            .find(|stage| stage.kind() == StageKind::Lead)
            .map(|stage| stage.id().clone())
            .ok_or(RunFixError::NotALeadSession)?;
        let index = u32::try_from(
            self.workflow
                .stages()
                .iter()
                .filter(|stage| stage.kind() == StageKind::Lead)
                .count(),
        )
        .map_err(|_| RunFixError::TooManyCycles)?
            + 1;
        let additional = super::lead_turn_stages(index, &previous);
        let stage_ids = additional
            .iter()
            .map(|stage| stage.id().clone())
            .collect::<Vec<_>>();
        let workflow = self.workflow.extended(additional)?;
        let new_stages = workflow
            .stages()
            .iter()
            .filter(|stage| stage_ids.contains(stage.id()))
            .map(|definition| Stage::from_definition(self.id, definition))
            .collect::<Vec<_>>();
        self.workflow = workflow;
        self.stages.extend(new_stages);
        self.status = RunStatus::Running;
        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            None,
            DomainEventKind::RunContinueRequested { stage_ids },
        ))
    }

    /// Adds one pending attention request and moves stage/run to `NeedsUser`.
    ///
    /// # Errors
    /// Rejects inactive runs, mismatched identities, duplicate request IDs, or
    /// stages that are not currently executing.
    pub fn request_attention(
        &mut self,
        request: AttentionRequest,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunAttentionError> {
        if !matches!(self.status, RunStatus::Running | RunStatus::NeedsUser) {
            return Err(RunAttentionError::RunNotActive(self.status));
        }
        if request.run_id() != self.id {
            return Err(RunAttentionError::WrongRun {
                expected: self.id,
                actual: request.run_id(),
            });
        }
        if !request.status().is_pending() {
            return Err(RunAttentionError::RequestNotPending(request.id()));
        }
        if self
            .attention_requests
            .iter()
            .any(|existing| existing.id() == request.id())
        {
            return Err(RunAttentionError::DuplicateRequest(request.id()));
        }
        let stage_index = self.stage_index(request.stage_id())?;
        self.stages[stage_index].request_attention()?;

        let request_id = request.id();
        let kind = request.kind();
        let stage_id = request.stage_id().clone();
        self.attention_requests.push(request);
        self.status = RunStatus::NeedsUser;
        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            Some(stage_id),
            DomainEventKind::NeedsUser {
                attention_request_id: request_id,
                kind,
            },
        ))
    }

    /// Resolves one pending request. Last resolution restores affected stage/run.
    ///
    /// # Errors
    /// Rejects unknown or already-closed requests and inconsistent ownership.
    pub fn resolve_attention(
        &mut self,
        request_id: AttentionRequestId,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunAttentionError> {
        self.close_attention(request_id, metadata, false)
    }

    /// Cancels one pending request without pretending a human answered it.
    ///
    /// # Errors
    /// Rejects unknown or already-closed requests and inconsistent ownership.
    pub fn cancel_attention(
        &mut self,
        request_id: AttentionRequestId,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunAttentionError> {
        self.close_attention(request_id, metadata, true)
    }

    /// Records one provider checkpoint against its matching stage lifecycle.
    ///
    /// Provider checkpoints advance `updated_at`, making consumption atomic
    /// with the run snapshot and event append at the persistence boundary.
    ///
    /// # Errors
    /// Rejects non-provider event kinds, unknown stages, inactive runs,
    /// lifecycle mismatches, or empty progress messages.
    pub fn record_provider_event(
        &mut self,
        stage_id: &StageId,
        kind: DomainEventKind,
        metadata: EventMetadata,
    ) -> Result<DomainEvent, RunProviderEventError> {
        if !matches!(self.status, RunStatus::Running | RunStatus::NeedsUser) {
            return Err(RunProviderEventError::RunNotActive(self.status));
        }
        let stage = self
            .stage(stage_id)
            .ok_or_else(|| RunProviderEventError::UnknownStage(stage_id.clone()))?;
        let expected = match &kind {
            DomainEventKind::ProviderStarted { .. }
            | DomainEventKind::ProviderResumed { .. }
            | DomainEventKind::ProviderProgress { .. }
            | DomainEventKind::ProviderUsageUpdated { .. }
            | DomainEventKind::ProviderRuntimeObserved { .. }
            | DomainEventKind::UsageUpdated => StageStatus::Running,
            DomainEventKind::ProviderNeedsUser { .. } => StageStatus::NeedsUser,
            DomainEventKind::ProviderPaused { .. } => StageStatus::Paused,
            DomainEventKind::ProviderInterrupted { .. } => StageStatus::Interrupted,
            DomainEventKind::ProviderCompleted { .. } => StageStatus::Completed,
            DomainEventKind::ProviderFailed { .. } => StageStatus::Failed,
            _ => return Err(RunProviderEventError::InvalidEventKind),
        };
        if matches!(
            &kind,
            DomainEventKind::ProviderProgress { message, .. } if message.trim().is_empty()
        ) {
            return Err(RunProviderEventError::EmptyProgress);
        }
        if stage.status() != expected {
            return Err(RunProviderEventError::StageStatusMismatch {
                stage_id: stage_id.clone(),
                expected,
                actual: stage.status(),
            });
        }

        self.updated_at = metadata.occurred_at();
        Ok(DomainEvent::new(
            metadata,
            self.id,
            Some(stage_id.clone()),
            kind,
        ))
    }

    /// Checks cross-aggregate invariants used by rehydration and persistence.
    ///
    /// # Errors
    /// Returns a typed invariant violation when owned stages, attention, or
    /// lifecycle aggregates contradict one another.
    pub fn validate_invariants(&self) -> Result<(), RunInvariantError> {
        if self.updated_at < self.created_at {
            return Err(RunInvariantError::UpdatedBeforeCreated);
        }
        let mut stage_ids = HashSet::new();
        for stage in &self.stages {
            if stage.run_id() != self.id {
                return Err(RunInvariantError::StageOwnedByDifferentRun(
                    stage.id().clone(),
                ));
            }
            if !stage_ids.insert(stage.id().clone()) {
                return Err(RunInvariantError::DuplicateStage(stage.id().clone()));
            }
        }
        let expected_stage_ids = self
            .workflow
            .stages()
            .iter()
            .map(|stage| stage.id().clone())
            .collect::<HashSet<_>>();
        if expected_stage_ids != stage_ids {
            return Err(RunInvariantError::WorkflowStageSetMismatch);
        }

        let mut attention_ids = HashSet::new();
        for request in &self.attention_requests {
            if !attention_ids.insert(request.id()) {
                return Err(RunInvariantError::DuplicateAttention(request.id()));
            }
            if request.run_id() != self.id || self.stage(request.stage_id()).is_none() {
                return Err(RunInvariantError::InvalidAttentionOwner(request.id()));
            }
            let closed_at = match request.status() {
                AttentionStatus::Pending => None,
                AttentionStatus::Resolved(at) | AttentionStatus::Cancelled(at) => Some(at),
            };
            if request.created_at() < &self.created_at
                || request.created_at() > &self.updated_at
                || closed_at.is_some_and(|at| at > &self.updated_at)
            {
                return Err(RunInvariantError::AttentionOutsideRunTimeline(request.id()));
            }
        }

        let pending = self.pending_attention_count();
        let run_is_suspended = matches!(self.status, RunStatus::Paused | RunStatus::Interrupted);
        if run_is_suspended != self.suspended_from.is_some() {
            return Err(RunInvariantError::RunSuspensionMismatch);
        }
        let run_expects_attention = match self.status {
            RunStatus::NeedsUser => true,
            RunStatus::Paused | RunStatus::Interrupted => {
                self.suspended_from == Some(ResumableRunStatus::NeedsUser)
            }
            _ => false,
        };
        if run_expects_attention != (pending > 0) {
            return Err(RunInvariantError::RunAttentionMismatch);
        }
        for stage in &self.stages {
            self.validate_stage_current_state(stage)?;
        }
        if matches!(
            self.status,
            RunStatus::Created | RunStatus::Preparing | RunStatus::Ready
        ) && (self
            .stages
            .iter()
            .any(|stage| stage.status() != StageStatus::Pending)
            || !self.attention_requests.is_empty())
        {
            return Err(RunInvariantError::PreExecutionStateContainsHistory);
        }
        if matches!(self.status, RunStatus::Completed | RunStatus::Applied) {
            self.ensure_completion_allowed()
                .map_err(|_| RunInvariantError::InvalidCompletedRun)?;
        }
        if self.status.is_lifecycle_closed() && pending > 0 {
            return Err(RunInvariantError::ClosedRunHasPendingAttention);
        }
        Ok(())
    }

    fn validate_stage_current_state(&self, stage: &Stage) -> Result<(), RunInvariantError> {
        if stage.expects_attention() != self.pending_for_stage(stage.id()) {
            return Err(RunInvariantError::StageAttentionMismatch(
                stage.id().clone(),
            ));
        }
        let run_suspension_matches = matches!(
            (self.status, stage.status()),
            (RunStatus::Paused, StageStatus::Paused)
                | (RunStatus::Interrupted, StageStatus::Interrupted)
                | (RunStatus::Failed | RunStatus::Discarded, _)
        );
        if stage.has_run_owned_suspension() && !run_suspension_matches {
            return Err(RunInvariantError::StageRunSuspensionMismatch(
                stage.id().clone(),
            ));
        }
        if matches!(self.status, RunStatus::Paused | RunStatus::Interrupted)
            && matches!(
                stage.status(),
                StageStatus::Running | StageStatus::NeedsUser
            )
        {
            return Err(RunInvariantError::SuspendedRunHasActiveStage(
                stage.id().clone(),
            ));
        }
        if !matches!(
            stage.status(),
            StageStatus::Running
                | StageStatus::NeedsUser
                | StageStatus::Paused
                | StageStatus::Interrupted
                | StageStatus::Completed
                | StageStatus::Failed
        ) {
            return Ok(());
        }
        for dependency in stage.dependencies() {
            let dependency_status = self
                .stage(dependency.stage_id())
                .ok_or(RunInvariantError::WorkflowStageSetMismatch)?
                .status();
            let valid = match dependency.kind() {
                DependencyKind::Required => dependency_status == StageStatus::Completed,
                DependencyKind::Optional => dependency_status.is_terminal_outcome(),
            };
            if !valid {
                return Err(RunInvariantError::AdvancedStageHasInvalidDependency {
                    stage_id: stage.id().clone(),
                    dependency_id: dependency.stage_id().clone(),
                });
            }
        }
        Ok(())
    }

    fn stage_index(&self, stage_id: &StageId) -> Result<usize, RunStageError> {
        self.stages
            .iter()
            .position(|stage| stage.id() == stage_id)
            .ok_or_else(|| RunStageError::UnknownStage(stage_id.clone()))
    }

    /// Buckets one stage's dependencies into waiting / blocked / degraded.
    ///
    /// The single source of truth for dependency readiness: both the mutating
    /// [`Self::ensure_stage_ready`] and the read-only
    /// [`Self::stage_dependency_report`] call this, so they cannot drift.
    fn dependency_buckets(&self, index: usize) -> Result<DependencyBuckets, RunStageError> {
        let stage = &self.stages[index];
        let mut waiting = Vec::new();
        let mut blocked = Vec::new();
        let mut degraded = Vec::new();
        for dependency in stage.dependencies() {
            let dependency_stage = self
                .stage(dependency.stage_id())
                .ok_or_else(|| RunStageError::UnknownStage(dependency.stage_id().clone()))?;
            match (dependency.kind(), dependency_stage.status()) {
                (_, StageStatus::Completed) => {}
                (DependencyKind::Required, StageStatus::Failed) => {
                    blocked.push(BlockedDependency {
                        stage_id: dependency.stage_id().clone(),
                        outcome: DependencyOutcome::Failed,
                    });
                }
                (DependencyKind::Required, StageStatus::Skipped) => {
                    blocked.push(BlockedDependency {
                        stage_id: dependency.stage_id().clone(),
                        outcome: DependencyOutcome::Skipped,
                    });
                }
                (DependencyKind::Optional, StageStatus::Failed | StageStatus::Skipped) => {
                    degraded.push(dependency.stage_id().clone());
                }
                _ => waiting.push(dependency.stage_id().clone()),
            }
        }
        Ok(DependencyBuckets {
            waiting,
            blocked,
            degraded,
        })
    }

    fn ensure_stage_ready(&self, index: usize) -> Result<Vec<StageId>, RunStageError> {
        let stage = &self.stages[index];
        let buckets = self.dependency_buckets(index)?;
        if !buckets.blocked.is_empty() {
            return Err(RunStageError::RequiredDependenciesBlocked {
                stage_id: stage.id().clone(),
                dependencies: buckets
                    .blocked
                    .into_iter()
                    .map(|blocked| blocked.stage_id)
                    .collect(),
            });
        }
        if !buckets.waiting.is_empty() {
            return Err(RunStageError::DependenciesNotFinished {
                stage_id: stage.id().clone(),
                dependencies: buckets.waiting,
            });
        }
        Ok(buckets.degraded)
    }

    /// Read-only recomputation of why a Pending/Ready stage is not running.
    ///
    /// Reuses the exact bucketing [`Self::ensure_stage_ready`] applies when it
    /// decides, so this can never disagree with the scheduler's own verdict.
    /// `None` for an unknown stage, one that isn't Pending/Ready (dependency
    /// readiness is meaningless once a stage has moved past waiting), or a
    /// Pending/Ready stage whose dependencies are all already satisfied (the
    /// scheduler just hasn't marked it `Ready` yet) — there is nothing to
    /// report, so callers must never read `Some` as "this stage is waiting".
    #[must_use]
    pub fn stage_dependency_report(&self, stage_id: &StageId) -> Option<StageDependencyReport> {
        let index = self
            .stages
            .iter()
            .position(|stage| stage.id() == stage_id)?;
        if !matches!(
            self.stages[index].status(),
            StageStatus::Pending | StageStatus::Ready
        ) {
            return None;
        }
        let buckets = self.dependency_buckets(index).ok()?;
        if buckets.waiting.is_empty() && buckets.blocked.is_empty() && buckets.degraded.is_empty() {
            return None;
        }
        Some(StageDependencyReport {
            waiting_on: buckets.waiting,
            blocked_by: buckets.blocked,
            degraded: buckets.degraded,
        })
    }

    /// Every stage downstream of `stage_id`, transitively, in definition
    /// order.
    fn descendants(&self, stage_id: &StageId) -> Vec<StageId> {
        let mut reached = vec![stage_id.clone()];
        let mut descendants = Vec::new();
        for stage in &self.stages {
            if stage
                .dependencies()
                .iter()
                .any(|dependency| reached.contains(dependency.stage_id()))
            {
                reached.push(stage.id().clone());
                descendants.push(stage.id().clone());
            }
        }
        descendants
    }

    /// The stages a retry of `stage_id` would return to pending with it:
    /// every downstream stage that was skipped, which a failure upstream
    /// is the only way to become. They never ran, so nothing they hold is
    /// invalidated; leaving them skipped would let the retried stage
    /// succeed into a run that can never complete.
    #[must_use]
    pub fn skipped_descendants(&self, stage_id: &StageId) -> Vec<StageId> {
        self.descendants(stage_id)
            .into_iter()
            .filter(|id| {
                self.stage(id)
                    .is_some_and(|stage| stage.status() == StageStatus::Skipped)
            })
            .collect()
    }

    /// A retry may discard nothing: every downstream stage, direct or not,
    /// must still be pending, ready, or skipped. Skipped counts as nothing
    /// done because it is exactly that, and the retry un-skips it.
    fn ensure_retry_safe(&self, stage_id: &StageId) -> Result<(), RunStageError> {
        let advanced = self
            .descendants(stage_id)
            .into_iter()
            .filter(|id| {
                self.stage(id).is_some_and(|stage| {
                    !matches!(
                        stage.status(),
                        StageStatus::Pending | StageStatus::Ready | StageStatus::Skipped
                    )
                })
            })
            .collect::<Vec<_>>();
        if advanced.is_empty() {
            Ok(())
        } else {
            Err(RunStageError::RetryWouldInvalidate {
                stage_id: stage_id.clone(),
                advanced_dependents: advanced,
            })
        }
    }

    fn ensure_completion_allowed(&self) -> Result<(), RunTransitionError> {
        let blockers = self.completion_blockers();
        if self.pending_attention_count() > 0 {
            return Err(RunTransitionError::PendingAttention);
        }
        if blockers.is_empty() {
            Ok(())
        } else {
            Err(RunTransitionError::CompletionBlocked(blockers))
        }
    }

    fn suspend_run(&mut self, suspended_status: RunStatus) -> Result<(), RunTransitionError> {
        let resume_to = ResumableRunStatus::try_from(self.status)
            .map_err(|()| RunTransitionError::InvalidSuspensionSource { from: self.status })?;
        self.suspended_from = Some(resume_to);
        self.status = suspended_status;
        Ok(())
    }

    fn restore_run(&mut self) -> Result<(), RunTransitionError> {
        let resume_to = self
            .suspended_from
            .ok_or(RunTransitionError::MissingSuspensionContext)?;
        self.status = resume_to.into_status();
        self.suspended_from = None;
        Ok(())
    }

    fn close_attention(
        &mut self,
        request_id: AttentionRequestId,
        metadata: EventMetadata,
        cancel: bool,
    ) -> Result<DomainEvent, RunAttentionError> {
        let index = self
            .attention_requests
            .iter()
            .position(|request| request.id() == request_id)
            .ok_or(RunAttentionError::UnknownRequest(request_id))?;
        if !self.attention_requests[index].status().is_pending() {
            return Err(RunAttentionError::Attention(AttentionError::AlreadyClosed(
                request_id,
            )));
        }
        let stage_id = self.attention_requests[index].stage_id().clone();
        let stage_index = self.stage_index(&stage_id)?;
        if cancel {
            self.attention_requests[index].cancel(metadata.occurred_at())?;
        } else {
            self.attention_requests[index].resolve(metadata.occurred_at())?;
        }

        let stage_pending = self.pending_for_stage(&stage_id);
        self.stages[stage_index].attention_resolved(stage_pending);
        if self.pending_attention_count() == 0 {
            match self.status {
                RunStatus::NeedsUser => self.status = RunStatus::Running,
                RunStatus::Paused | RunStatus::Interrupted
                    if self.suspended_from == Some(ResumableRunStatus::NeedsUser) =>
                {
                    self.suspended_from = Some(ResumableRunStatus::Running);
                }
                _ => {}
            }
        }
        self.updated_at = metadata.occurred_at();
        let kind = if cancel {
            DomainEventKind::AttentionCancelled {
                attention_request_id: request_id,
            }
        } else {
            DomainEventKind::AttentionResolved {
                attention_request_id: request_id,
            }
        };
        Ok(DomainEvent::new(metadata, self.id, Some(stage_id), kind))
    }

    fn pending_for_stage(&self, stage_id: &StageId) -> bool {
        self.attention_requests.iter().any(|request| {
            request.stage_id() == stage_id && request.status() == &AttentionStatus::Pending
        })
    }

    fn pending_attention_count(&self) -> usize {
        self.attention_requests
            .iter()
            .filter(|request| request.status().is_pending())
            .count()
    }

    fn cancel_all_pending(&mut self, at: DateTime<Utc>) -> Result<(), AttentionError> {
        if let Some(request) = self
            .attention_requests
            .iter()
            .find(|request| request.status().is_pending() && request.created_at() > &at)
        {
            return Err(AttentionError::ClosedBeforeCreation {
                id: request.id(),
                created_at: *request.created_at(),
                closed_at: at,
            });
        }
        for request in &mut self.attention_requests {
            if request.status().is_pending() {
                request.cancel(at)?;
            }
        }
        for stage in &mut self.stages {
            stage.attention_resolved(false);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunRehydrationError {
    #[error(transparent)]
    Workflow(#[from] WorkflowDefinitionError),
    #[error("duplicate rehydrated stage state: {0}")]
    DuplicateStageState(StageId),
    #[error("missing rehydrated stage state: {0}")]
    MissingStageState(StageId),
    #[error("rehydrated state references unknown stage: {0}")]
    UnknownStageState(StageId),
    #[error("invalid rehydrated stage state for {stage_id}: {source}")]
    InvalidStageState {
        stage_id: StageId,
        source: StageRehydrationError,
    },
    #[error(transparent)]
    Invariant(#[from] RunInvariantError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionBlockerReason {
    StageNotTerminal,
    FailedLeafStage,
    RequiredStageFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionBlocker {
    pub stage_id: StageId,
    pub status: StageStatus,
    pub reason: CompletionBlockerReason,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunTransitionError {
    #[error("run transition {transition:?} is invalid from {from:?}")]
    InvalidTransition {
        from: RunStatus,
        transition: RunTransition,
    },
    #[error("run cannot suspend from {from:?}")]
    InvalidSuspensionSource { from: RunStatus },
    #[error("run suspension context is missing")]
    MissingSuspensionContext,
    #[error("run completion is blocked: {0:?}")]
    CompletionBlocked(Vec<CompletionBlocker>),
    #[error("run completion is blocked by unresolved attention")]
    PendingAttention,
    #[error(transparent)]
    Attention(#[from] AttentionError),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunFixError {
    #[error("only a completed run can be sent back for a fix; this run is {0:?}")]
    RunNotCompleted(RunStatus),
    #[error("run has no decision stage to answer")]
    NoDecisionToAnswer,
    #[error("only a lead session takes another turn; this run has no lead stage")]
    NotALeadSession,
    #[error("run has grown more remediation cycles than can be numbered")]
    TooManyCycles,
    #[error(transparent)]
    Workflow(#[from] WorkflowDefinitionError),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunPublishError {
    #[error("only a completed run can be published; this run is {0:?}")]
    RunNotCompleted(RunStatus),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunRebaseError {
    #[error("only a completed run's workspace can be rebased; this run is {0:?}")]
    RunNotCompleted(RunStatus),
}

/// Private working set shared by the mutating and read-only dependency
/// paths. Never exposed: callers get either a [`RunStageError`] (mutating) or
/// a [`StageDependencyReport`] (read-only).
struct DependencyBuckets {
    waiting: Vec<StageId>,
    blocked: Vec<BlockedDependency>,
    degraded: Vec<StageId>,
}

/// Why a required dependency landed in `blocked_by`: it either failed
/// outright, or it was skipped because one of *its own* required
/// dependencies was blocked. Rendering must not conflate the two — a skipped
/// dependency never itself failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyOutcome {
    Failed,
    Skipped,
}

/// One required dependency that is holding a stage back, with the outcome
/// that put it there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedDependency {
    pub stage_id: StageId,
    pub outcome: DependencyOutcome,
}

/// Why a Pending/Ready stage is not running yet, recomputed read-only.
///
/// Mirrors the buckets [`Run::ensure_stage_ready`] computes when it decides
/// whether a stage may become `Ready`: `waiting_on` has not finished yet,
/// `blocked_by` is a required dependency that failed or was skipped (this
/// stage will be skipped in turn), `degraded` is an optional dependency that
/// failed or was skipped (this stage will still run, without it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageDependencyReport {
    pub waiting_on: Vec<StageId>,
    pub blocked_by: Vec<BlockedDependency>,
    pub degraded: Vec<StageId>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunStageError {
    #[error("run is not active: {0:?}")]
    RunNotActive(RunStatus),
    #[error("unknown stage: {0}")]
    UnknownStage(StageId),
    #[error("stage {stage_id} is waiting for dependencies: {dependencies:?}")]
    DependenciesNotFinished {
        stage_id: StageId,
        dependencies: Vec<StageId>,
    },
    #[error("stage {stage_id} has blocked required dependencies: {dependencies:?}")]
    RequiredDependenciesBlocked {
        stage_id: StageId,
        dependencies: Vec<StageId>,
    },
    #[error(
        "retrying stage {stage_id} would invalidate advanced dependents: {advanced_dependents:?}"
    )]
    RetryWouldInvalidate {
        stage_id: StageId,
        advanced_dependents: Vec<StageId>,
    },
    #[error(transparent)]
    Stage(#[from] StageTransitionError),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunAttentionError {
    #[error("run is not active for new attention: {0:?}")]
    RunNotActive(RunStatus),
    #[error("attention request belongs to run {actual}, expected {expected}")]
    WrongRun { expected: RunId, actual: RunId },
    #[error("duplicate attention request: {0}")]
    DuplicateRequest(AttentionRequestId),
    #[error("attention request is already resolved or cancelled: {0}")]
    RequestNotPending(AttentionRequestId),
    #[error("unknown attention request: {0}")]
    UnknownRequest(AttentionRequestId),
    #[error(transparent)]
    Stage(#[from] RunStageError),
    #[error(transparent)]
    StageLifecycle(#[from] StageTransitionError),
    #[error(transparent)]
    Attention(#[from] AttentionError),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunProviderEventError {
    #[error("run is not active for provider events: {0:?}")]
    RunNotActive(RunStatus),
    #[error("unknown provider-event stage: {0}")]
    UnknownStage(StageId),
    #[error("event kind is not a provider checkpoint")]
    InvalidEventKind,
    #[error("provider progress message must not be empty")]
    EmptyProgress,
    #[error("provider event for stage {stage_id} requires {expected:?}, found {actual:?}")]
    StageStatusMismatch {
        stage_id: StageId,
        expected: StageStatus,
        actual: StageStatus,
    },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunInvariantError {
    #[error("run updated_at precedes created_at")]
    UpdatedBeforeCreated,
    #[error("stage {0} belongs to another run")]
    StageOwnedByDifferentRun(StageId),
    #[error("duplicate stage in run: {0}")]
    DuplicateStage(StageId),
    #[error("run stage set does not match its workflow definition")]
    WorkflowStageSetMismatch,
    #[error("duplicate attention request in run: {0}")]
    DuplicateAttention(AttentionRequestId),
    #[error("attention request has invalid run or stage ownership: {0}")]
    InvalidAttentionOwner(AttentionRequestId),
    #[error("attention request falls outside run timeline: {0}")]
    AttentionOutsideRunTimeline(AttentionRequestId),
    #[error("run status and pending attention disagree")]
    RunAttentionMismatch,
    #[error("run status and suspension context disagree")]
    RunSuspensionMismatch,
    #[error("stage {0} status and pending attention disagree")]
    StageAttentionMismatch(StageId),
    #[error("stage {0} has run-owned suspension inconsistent with run status")]
    StageRunSuspensionMismatch(StageId),
    #[error("suspended run contains active stage {0}")]
    SuspendedRunHasActiveStage(StageId),
    #[error("advanced stage {stage_id} has invalid dependency outcome for {dependency_id}")]
    AdvancedStageHasInvalidDependency {
        stage_id: StageId,
        dependency_id: StageId,
    },
    #[error("pre-execution run contains stage or attention history")]
    PreExecutionStateContainsHistory,
    #[error("completed/applied run contains invalid stage outcomes")]
    InvalidCompletedRun,
    #[error("closed run contains pending attention")]
    ClosedRunHasPendingAttention,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::domain::{
        AttentionKind, AttentionRequestId, Dependency, EventId, Role, StageDefinition, StageKind,
        WorkflowDefinitionError,
    };
    use crate::domain::{ModelId, ProviderId};

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 14, 8, 0, second)
            .single()
            .unwrap()
    }

    fn metadata(value: u128, second: u32) -> EventMetadata {
        EventMetadata::new(EventId::from_u128(value), at(second))
    }

    fn id(value: &str) -> StageId {
        StageId::new(value).unwrap()
    }

    fn fast_workflow() -> WorkflowDefinition {
        WorkflowDefinition::new(
            WorkflowKind::Fast,
            vec![StageDefinition::new(
                id("implementation"),
                StageKind::Implementation,
                Role::Implementer,
                vec![],
            )],
        )
        .unwrap()
    }

    fn new_run(workflow: WorkflowDefinition) -> Run {
        Run::new(
            RunId::from_u128(10),
            workflow,
            ConfigSnapshotId::new("recommended-2026.08").unwrap(),
            at(0),
        )
    }

    fn start_run(run: &mut Run) {
        for (transition, event, second) in [
            (RunTransition::BeginPreparation, 1, 1),
            (RunTransition::FinishPreparation, 2, 2),
            (RunTransition::Start, 3, 3),
        ] {
            run.transition(transition, metadata(event, second)).unwrap();
        }
    }

    fn complete_stage(run: &mut Run, stage_id: &StageId, event: u128) {
        run.transition_stage(stage_id, StageTransition::MarkReady, metadata(event, 4))
            .unwrap();
        run.transition_stage(stage_id, StageTransition::Start, metadata(event + 1, 5))
            .unwrap();
        run.transition_stage(stage_id, StageTransition::Complete, metadata(event + 2, 6))
            .unwrap();
    }

    /// A completed run whose decision the operator rejects.
    fn decided_run() -> Run {
        let mut run = new_run(WorkflowDefinition::built_in(WorkflowKind::Standard));
        start_run(&mut run);
        let mut event = 10;
        for stage in [
            "architecture",
            "implementation",
            "simplification",
            "quality_review",
            "spec_review",
            "verify",
            "decision",
        ] {
            complete_stage(&mut run, &id(stage), event);
            event += 10;
        }
        run.transition(RunTransition::Complete, metadata(100, 7))
            .unwrap();
        run
    }

    fn dependency_ids(run: &Run, stage_id: &StageId) -> Vec<String> {
        run.workflow()
            .stage(stage_id)
            .expect("stage exists")
            .dependencies()
            .iter()
            .map(|dependency| dependency.stage_id().to_string())
            .collect()
    }

    #[test]
    fn a_rejected_run_grows_a_fix_and_a_fresh_decision_over_it_and_reopens() {
        let mut run = decided_run();
        assert_eq!(run.stages().len(), 7);

        let event = run.request_fix(metadata(200, 8)).unwrap();

        // The run reopens rather than a second run adopting its workspace.
        assert_eq!(run.status(), RunStatus::Running);
        assert_eq!(run.stages().len(), 10);
        assert!(matches!(
            event.kind(),
            DomainEventKind::RunFixRequested { stage_ids }
                if stage_ids.iter().map(ToString::to_string).collect::<Vec<_>>()
                    == vec!["fix_1".to_owned(), "verify_1".to_owned(), "decision_1".to_owned()]
        ));

        // The fix answers the verdict that sent the work back, verification
        // checks the fix, and the new decision reads all of it.
        assert_eq!(dependency_ids(&run, &id("fix_1")), vec!["decision"]);
        assert_eq!(dependency_ids(&run, &id("verify_1")), vec!["fix_1"]);
        assert_eq!(
            dependency_ids(&run, &id("decision_1")),
            vec!["fix_1", "verify_1", "decision"]
        );

        // Nothing already done is disturbed, and the new work has not started.
        assert_eq!(
            run.stage(&id("implementation")).unwrap().status(),
            StageStatus::Completed
        );
        for stage in ["fix_1", "verify_1", "decision_1"] {
            assert_eq!(
                run.stage(&id(stage)).unwrap().status(),
                StageStatus::Pending
            );
        }
        run.validate_invariants().unwrap();
    }

    /// Reopening is for a run that finished and was judged, not for one that is
    /// still going, already disposed of, or never decided.
    #[test]
    fn only_a_completed_run_can_be_sent_back_for_a_fix() {
        for (build, expected) in [
            (
                (|| {
                    let mut run = new_run(WorkflowDefinition::built_in(WorkflowKind::Standard));
                    start_run(&mut run);
                    run
                }) as fn() -> Run,
                RunStatus::Running,
            ),
            (
                || {
                    let mut run = decided_run();
                    run.transition(RunTransition::Apply, metadata(150, 8))
                        .unwrap();
                    run
                },
                RunStatus::Applied,
            ),
            (
                || {
                    let mut run = decided_run();
                    run.transition(RunTransition::Discard, metadata(150, 8))
                        .unwrap();
                    run
                },
                RunStatus::Discarded,
            ),
        ] {
            let mut run = build();
            let before = run.stages().len();
            assert_eq!(
                run.request_fix(metadata(200, 9)).unwrap_err(),
                RunFixError::RunNotCompleted(expected)
            );
            assert_eq!(run.stages().len(), before, "a refusal changes nothing");
            assert_eq!(run.status(), expected);
        }

        // A workflow with no decision has no verdict to answer.
        let mut run = new_run(fast_workflow());
        start_run(&mut run);
        complete_stage(&mut run, &id("implementation"), 10);
        run.transition(RunTransition::Complete, metadata(100, 7))
            .unwrap();
        assert_eq!(
            run.request_fix(metadata(200, 8)).unwrap_err(),
            RunFixError::NoDecisionToAnswer
        );
    }

    /// Pressing fix again answers the verdict that rejected the last fix, not
    /// the one that rejected the original work.
    #[test]
    fn a_second_cycle_answers_the_decision_that_rejected_the_first() {
        let mut run = decided_run();
        run.request_fix(metadata(200, 8)).unwrap();
        let mut event = 210;
        for stage in ["fix_1", "verify_1", "decision_1"] {
            complete_stage(&mut run, &id(stage), event);
            event += 10;
        }
        run.transition(RunTransition::Complete, metadata(300, 9))
            .unwrap();

        run.request_fix(metadata(400, 10)).unwrap();
        assert_eq!(run.stages().len(), 13);
        assert_eq!(dependency_ids(&run, &id("fix_2")), vec!["decision_1"]);
        assert_eq!(
            dependency_ids(&run, &id("decision_2")),
            vec!["fix_2", "verify_2", "decision_1"]
        );
        run.validate_invariants().unwrap();
    }

    #[test]
    fn a_rejected_run_grows_a_follow_up_and_a_fresh_decision_over_it_and_reopens() {
        let mut run = decided_run();
        assert_eq!(run.stages().len(), 7);

        let event = run.request_continue(metadata(200, 8)).unwrap();

        // The run reopens rather than a second run adopting its workspace.
        assert_eq!(run.status(), RunStatus::Running);
        assert_eq!(run.stages().len(), 10);
        assert!(matches!(
            event.kind(),
            DomainEventKind::RunContinueRequested { stage_ids }
                if stage_ids.iter().map(ToString::to_string).collect::<Vec<_>>()
                    == vec![
                        "followup_1".to_owned(),
                        "followup_verify_1".to_owned(),
                        "followup_decision_1".to_owned()
                    ]
        ));

        assert_eq!(dependency_ids(&run, &id("followup_1")), vec!["decision"]);
        assert_eq!(
            dependency_ids(&run, &id("followup_verify_1")),
            vec!["followup_1"]
        );
        assert_eq!(
            dependency_ids(&run, &id("followup_decision_1")),
            vec!["followup_1", "followup_verify_1", "decision"]
        );

        assert_eq!(
            run.stage(&id("implementation")).unwrap().status(),
            StageStatus::Completed
        );
        for stage in ["followup_1", "followup_verify_1", "followup_decision_1"] {
            assert_eq!(
                run.stage(&id(stage)).unwrap().status(),
                StageStatus::Pending
            );
        }
        run.validate_invariants().unwrap();
    }

    /// Same guard as `request_fix`, reused rather than duplicated: reopening
    /// is for a run that finished and was judged.
    #[test]
    fn only_a_completed_run_can_be_sent_back_to_continue() {
        let mut run = new_run(WorkflowDefinition::built_in(WorkflowKind::Standard));
        start_run(&mut run);
        assert_eq!(
            run.request_continue(metadata(200, 8)).unwrap_err(),
            RunFixError::RunNotCompleted(RunStatus::Running)
        );

        let mut run = new_run(fast_workflow());
        start_run(&mut run);
        complete_stage(&mut run, &id("implementation"), 10);
        run.transition(RunTransition::Complete, metadata(100, 7))
            .unwrap();
        assert_eq!(
            run.request_continue(metadata(200, 8)).unwrap_err(),
            RunFixError::NoDecisionToAnswer
        );
    }

    /// A fix cycle and a continue cycle can both grow the same run without
    /// their stage identities ever colliding.
    #[test]
    fn a_run_can_grow_both_a_fix_cycle_and_a_continue_cycle_over_the_same_decision() {
        let mut run = decided_run();
        run.request_fix(metadata(200, 8)).unwrap();
        let mut event = 210;
        for stage in ["fix_1", "verify_1", "decision_1"] {
            complete_stage(&mut run, &id(stage), event);
            event += 10;
        }
        run.transition(RunTransition::Complete, metadata(300, 9))
            .unwrap();

        run.request_continue(metadata(400, 10)).unwrap();

        assert_eq!(run.stages().len(), 13);
        assert_eq!(dependency_ids(&run, &id("fix_1")), vec!["decision"]);
        assert_eq!(
            dependency_ids(&run, &id("followup_1")),
            vec!["decision_1"],
            "the continue cycle answers the decision the run most recently reached"
        );
        run.validate_invariants().unwrap();
    }

    /// A failure skips everything downstream of it. Retrying the failure
    /// returns those skipped stages to pending with it — they never ran, so
    /// nothing is discarded — while anything downstream that did run, even
    /// through an optional edge, still refuses the retry.
    #[test]
    fn retrying_a_failed_stage_returns_its_skipped_descendants_to_pending() {
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Fast,
            vec![
                StageDefinition::new(
                    id("implementation"),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![],
                ),
                StageDefinition::new(
                    id("verify"),
                    StageKind::Verify,
                    Role::Verifier,
                    vec![Dependency::required(id("implementation"))],
                ),
                StageDefinition::new(
                    id("decision"),
                    StageKind::Decision,
                    Role::EngineeringLead,
                    vec![Dependency::required(id("verify"))],
                ),
                StageDefinition::new(
                    id("synthesis"),
                    StageKind::Synthesis,
                    Role::EngineeringLead,
                    vec![Dependency::optional(id("verify"))],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);
        run.transition_stage(
            &id("implementation"),
            StageTransition::MarkReady,
            metadata(10, 4),
        )
        .unwrap();
        run.transition_stage(
            &id("implementation"),
            StageTransition::Start,
            metadata(11, 5),
        )
        .unwrap();
        run.transition_stage(
            &id("implementation"),
            StageTransition::Fail,
            metadata(12, 6),
        )
        .unwrap();
        for stage in ["verify", "decision"] {
            run.transition_stage(&id(stage), StageTransition::Skip, metadata(13, 7))
                .unwrap();
        }
        assert_eq!(
            run.skipped_descendants(&id("implementation")),
            vec![id("verify"), id("decision")]
        );

        let mut retried = run.clone();
        for stage in ["implementation", "verify", "decision"] {
            retried
                .transition_stage(&id(stage), StageTransition::Retry, metadata(14, 8))
                .unwrap();
            assert_eq!(
                retried.stage(&id(stage)).unwrap().status(),
                StageStatus::Pending
            );
        }

        // Synthesis only optionally needed verification, so it ran to
        // completion over the skip; that outcome is what a retry would
        // now invalidate.
        run.transition_stage(
            &id("synthesis"),
            StageTransition::MarkReady,
            metadata(15, 8),
        )
        .unwrap();
        run.transition_stage(&id("synthesis"), StageTransition::Start, metadata(16, 9))
            .unwrap();
        run.transition_stage(
            &id("synthesis"),
            StageTransition::Complete,
            metadata(17, 10),
        )
        .unwrap();
        assert!(matches!(
            run.transition_stage(&id("implementation"), StageTransition::Retry, metadata(18, 11)),
            Err(RunStageError::RetryWouldInvalidate { advanced_dependents, .. })
                if advanced_dependents == vec![id("synthesis")]
        ));
    }

    #[test]
    fn run_happy_path_reaches_applied_through_atomic_ready_boundary() {
        let mut run = new_run(fast_workflow());
        assert_eq!(run.status(), RunStatus::Created);
        start_run(&mut run);
        assert_eq!(run.status(), RunStatus::Running);
        complete_stage(&mut run, &id("implementation"), 10);
        run.transition(RunTransition::Complete, metadata(20, 7))
            .unwrap();
        run.transition(RunTransition::Apply, metadata(21, 8))
            .unwrap();

        assert_eq!(run.status(), RunStatus::Applied);
        assert!(run.status().is_lifecycle_closed());
        assert!(run.validate_invariants().is_ok());
    }

    #[test]
    fn completed_and_discarded_runs_reject_execution() {
        let mut completed = new_run(fast_workflow());
        start_run(&mut completed);
        complete_stage(&mut completed, &id("implementation"), 30);
        completed
            .transition(RunTransition::Complete, metadata(40, 7))
            .unwrap();
        let before = completed.clone();
        assert!(
            completed
                .transition(RunTransition::Start, metadata(41, 8))
                .is_err()
        );
        assert_eq!(completed, before);

        completed
            .transition(RunTransition::Discard, metadata(42, 9))
            .unwrap();
        assert_eq!(completed.status(), RunStatus::Discarded);
        assert!(
            completed
                .transition(RunTransition::Start, metadata(43, 10))
                .is_err()
        );
    }

    #[test]
    fn run_pause_and_interruption_restore_distinctly() {
        let mut paused = new_run(fast_workflow());
        start_run(&mut paused);
        paused
            .transition(RunTransition::Pause, metadata(50, 4))
            .unwrap();
        assert_eq!(paused.status(), RunStatus::Paused);
        assert!(
            paused
                .transition(RunTransition::Recover, metadata(51, 5))
                .is_err()
        );
        paused
            .transition(RunTransition::Resume, metadata(52, 6))
            .unwrap();
        assert_eq!(paused.status(), RunStatus::Running);

        let mut interrupted = new_run(fast_workflow());
        start_run(&mut interrupted);
        interrupted
            .transition(RunTransition::Interrupt, metadata(53, 4))
            .unwrap();
        assert_eq!(interrupted.status(), RunStatus::Interrupted);
        assert!(
            interrupted
                .transition(RunTransition::Resume, metadata(54, 5))
                .is_err()
        );
        interrupted
            .transition(RunTransition::Recover, metadata(55, 6))
            .unwrap();
        assert_eq!(interrupted.status(), RunStatus::Running);
    }

    #[test]
    fn multiple_attention_requests_resolve_in_order_without_losing_state() {
        let mut run = new_run(fast_workflow());
        start_run(&mut run);
        let stage_id = id("implementation");
        run.transition_stage(&stage_id, StageTransition::MarkReady, metadata(60, 4))
            .unwrap();
        run.transition_stage(&stage_id, StageTransition::Start, metadata(61, 5))
            .unwrap();
        let first = AttentionRequest::new(
            AttentionRequestId::from_u128(1),
            run.id(),
            stage_id.clone(),
            AttentionKind::Permission,
            "Run database reset",
            at(6),
        )
        .unwrap();
        let second = AttentionRequest::new(
            AttentionRequestId::from_u128(2),
            run.id(),
            stage_id.clone(),
            AttentionKind::Question,
            "Choose fallback",
            at(7),
        )
        .unwrap();
        run.request_attention(first, metadata(62, 6)).unwrap();
        run.request_attention(second, metadata(63, 7)).unwrap();
        assert_eq!(run.status(), RunStatus::NeedsUser);
        assert_eq!(
            run.stage(&stage_id).unwrap().status(),
            StageStatus::NeedsUser
        );

        run.resolve_attention(AttentionRequestId::from_u128(1), metadata(64, 8))
            .unwrap();
        assert_eq!(run.status(), RunStatus::NeedsUser);
        run.resolve_attention(AttentionRequestId::from_u128(2), metadata(65, 9))
            .unwrap();
        assert_eq!(run.status(), RunStatus::Running);
        assert_eq!(run.stage(&stage_id).unwrap().status(), StageStatus::Running);
        assert!(run.validate_invariants().is_ok());
        assert!(
            run.resolve_attention(AttentionRequestId::from_u128(2), metadata(66, 10))
                .is_err()
        );
    }

    #[test]
    fn closed_attention_request_cannot_enter_active_queue() {
        let mut run = new_run(fast_workflow());
        start_run(&mut run);
        let stage_id = id("implementation");
        run.transition_stage(&stage_id, StageTransition::MarkReady, metadata(67, 4))
            .unwrap();
        run.transition_stage(&stage_id, StageTransition::Start, metadata(68, 5))
            .unwrap();
        let mut request = AttentionRequest::new(
            AttentionRequestId::from_u128(3),
            run.id(),
            stage_id,
            AttentionKind::Question,
            "Already answered",
            at(6),
        )
        .unwrap();
        request.resolve(at(7)).unwrap();
        let before = run.clone();

        assert_eq!(
            run.request_attention(request, metadata(69, 8)),
            Err(RunAttentionError::RequestNotPending(
                AttentionRequestId::from_u128(3)
            ))
        );
        assert_eq!(run, before);
    }

    #[test]
    fn closing_run_cannot_backdate_attention_or_partially_mutate() {
        let mut run = new_run(fast_workflow());
        start_run(&mut run);
        let stage_id = id("implementation");
        run.transition_stage(&stage_id, StageTransition::MarkReady, metadata(76, 4))
            .unwrap();
        run.transition_stage(&stage_id, StageTransition::Start, metadata(77, 5))
            .unwrap();
        run.request_attention(
            AttentionRequest::new(
                AttentionRequestId::from_u128(8),
                run.id(),
                stage_id,
                AttentionKind::Permission,
                "Approve change",
                at(8),
            )
            .unwrap(),
            metadata(78, 8),
        )
        .unwrap();
        let before = run.clone();

        assert!(matches!(
            run.transition(RunTransition::Discard, metadata(79, 7)),
            Err(RunTransitionError::Attention(
                AttentionError::ClosedBeforeCreation { .. }
            ))
        ));
        assert_eq!(run, before);
    }

    #[test]
    fn attention_resolution_while_paused_changes_resume_target() {
        let mut run = new_run(fast_workflow());
        start_run(&mut run);
        let stage_id = id("implementation");
        run.transition_stage(&stage_id, StageTransition::MarkReady, metadata(70, 4))
            .unwrap();
        run.transition_stage(&stage_id, StageTransition::Start, metadata(71, 5))
            .unwrap();
        let request_id = AttentionRequestId::from_u128(7);
        run.request_attention(
            AttentionRequest::new(
                request_id,
                run.id(),
                stage_id.clone(),
                AttentionKind::Decision,
                "Proceed?",
                at(6),
            )
            .unwrap(),
            metadata(72, 6),
        )
        .unwrap();
        run.transition(RunTransition::Pause, metadata(73, 7))
            .unwrap();
        run.resolve_attention(request_id, metadata(74, 8)).unwrap();
        run.transition(RunTransition::Resume, metadata(75, 9))
            .unwrap();

        assert_eq!(run.status(), RunStatus::Running);
        assert_eq!(run.stage(&stage_id).unwrap().status(), StageStatus::Running);
        assert!(run.validate_invariants().is_ok());
    }

    #[test]
    fn required_dependencies_block_while_optional_failure_allows_degraded_ready() {
        let research = id("research");
        let independent = id("independent");
        let synthesis = id("synthesis");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                StageDefinition::new(
                    research.clone(),
                    StageKind::DeepAnalysis,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    independent.clone(),
                    StageKind::IndependentReview,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    synthesis.clone(),
                    StageKind::Synthesis,
                    Role::Reviewer,
                    vec![
                        Dependency::required(research.clone()),
                        Dependency::optional(independent.clone()),
                    ],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);
        assert!(matches!(
            run.transition_stage(&synthesis, StageTransition::MarkReady, metadata(80, 4)),
            Err(RunStageError::DependenciesNotFinished { .. })
        ));
        complete_stage(&mut run, &research, 81);
        run.transition_stage(&independent, StageTransition::MarkReady, metadata(84, 7))
            .unwrap();
        run.transition_stage(&independent, StageTransition::Start, metadata(85, 8))
            .unwrap();
        run.transition_stage(&independent, StageTransition::Fail, metadata(86, 9))
            .unwrap();
        let event = run
            .transition_stage(&synthesis, StageTransition::MarkReady, metadata(87, 10))
            .unwrap();

        assert_eq!(
            event.kind(),
            &DomainEventKind::StageReady { degraded: true }
        );
    }

    /// The read-only report agrees with the mutating path it shares its
    /// bucketing with: unfinished dependencies show up as `waiting_on`, and
    /// `MarkReady` still refuses for the same reason.
    #[test]
    fn stage_dependency_report_lists_unfinished_dependencies_as_waiting_on() {
        let research = id("research");
        let independent = id("independent");
        let synthesis = id("synthesis");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                StageDefinition::new(
                    research.clone(),
                    StageKind::DeepAnalysis,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    independent.clone(),
                    StageKind::IndependentReview,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    synthesis.clone(),
                    StageKind::Synthesis,
                    Role::Reviewer,
                    vec![
                        Dependency::required(research.clone()),
                        Dependency::optional(independent.clone()),
                    ],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);

        let report = run
            .stage_dependency_report(&synthesis)
            .expect("a pending stage reports its dependency readiness");
        assert_eq!(
            report.waiting_on,
            vec![research.clone(), independent.clone()]
        );
        assert!(report.blocked_by.is_empty());
        assert!(report.degraded.is_empty());

        assert!(matches!(
            run.transition_stage(&synthesis, StageTransition::MarkReady, metadata(80, 4)),
            Err(RunStageError::DependenciesNotFinished { .. })
        ));
    }

    /// A failed required dependency reports as `blocked_by`, matching the
    /// mutating path's `RequiredDependenciesBlocked`.
    #[test]
    fn stage_dependency_report_lists_failed_required_dependency_as_blocked() {
        let research = id("research");
        let synthesis = id("synthesis");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                StageDefinition::new(
                    research.clone(),
                    StageKind::DeepAnalysis,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    synthesis.clone(),
                    StageKind::Synthesis,
                    Role::Reviewer,
                    vec![Dependency::required(research.clone())],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);
        run.transition_stage(&research, StageTransition::MarkReady, metadata(80, 4))
            .unwrap();
        run.transition_stage(&research, StageTransition::Start, metadata(81, 5))
            .unwrap();
        run.transition_stage(&research, StageTransition::Fail, metadata(82, 6))
            .unwrap();

        let report = run
            .stage_dependency_report(&synthesis)
            .expect("a pending stage reports its dependency readiness");
        assert_eq!(
            report.blocked_by,
            vec![BlockedDependency {
                stage_id: research.clone(),
                outcome: DependencyOutcome::Failed,
            }]
        );
        assert!(report.waiting_on.is_empty());
        assert!(report.degraded.is_empty());

        assert!(matches!(
            run.transition_stage(&synthesis, StageTransition::MarkReady, metadata(83, 7)),
            Err(RunStageError::RequiredDependenciesBlocked { .. })
        ));
    }

    /// A required dependency that was skipped (rather than failed outright)
    /// still blocks, but the report must not conflate the two outcomes: a
    /// skipped dependency never itself failed.
    #[test]
    fn stage_dependency_report_lists_skipped_required_dependency_as_blocked() {
        let research = id("research");
        let synthesis = id("synthesis");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                StageDefinition::new(
                    research.clone(),
                    StageKind::DeepAnalysis,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    synthesis.clone(),
                    StageKind::Synthesis,
                    Role::Reviewer,
                    vec![Dependency::required(research.clone())],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);
        run.transition_stage(&research, StageTransition::Skip, metadata(80, 4))
            .unwrap();

        let report = run
            .stage_dependency_report(&synthesis)
            .expect("a pending stage reports its dependency readiness");
        assert_eq!(
            report.blocked_by,
            vec![BlockedDependency {
                stage_id: research.clone(),
                outcome: DependencyOutcome::Skipped,
            }]
        );
        assert!(report.waiting_on.is_empty());
        assert!(report.degraded.is_empty());

        assert!(matches!(
            run.transition_stage(&synthesis, StageTransition::MarkReady, metadata(81, 5)),
            Err(RunStageError::RequiredDependenciesBlocked { .. })
        ));
    }

    /// A failed optional dependency reports as `degraded`, never as
    /// `waiting_on` or `blocked_by` — it does not hold the stage back.
    #[test]
    fn stage_dependency_report_lists_failed_optional_dependency_as_degraded() {
        let research = id("research");
        let independent = id("independent");
        let synthesis = id("synthesis");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                StageDefinition::new(
                    research.clone(),
                    StageKind::DeepAnalysis,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    independent.clone(),
                    StageKind::IndependentReview,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    synthesis.clone(),
                    StageKind::Synthesis,
                    Role::Reviewer,
                    vec![
                        Dependency::required(research.clone()),
                        Dependency::optional(independent.clone()),
                    ],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);
        complete_stage(&mut run, &research, 81);
        run.transition_stage(&independent, StageTransition::MarkReady, metadata(84, 7))
            .unwrap();
        run.transition_stage(&independent, StageTransition::Start, metadata(85, 8))
            .unwrap();
        run.transition_stage(&independent, StageTransition::Fail, metadata(86, 9))
            .unwrap();

        let report = run
            .stage_dependency_report(&synthesis)
            .expect("a pending stage reports its dependency readiness");
        assert!(report.waiting_on.is_empty());
        assert!(report.blocked_by.is_empty());
        assert_eq!(report.degraded, vec![independent.clone()]);
    }

    /// `None` means "nothing to report", not "not Pending/Ready": a stage
    /// whose dependencies are all satisfied has nothing to report the moment
    /// they are satisfied, even before the scheduler gets around to marking
    /// it `Ready` — and stays `None` through `Ready` and once it starts
    /// running.
    #[test]
    fn stage_dependency_report_is_none_once_dependencies_are_satisfied_or_the_stage_leaves_pending_or_ready()
     {
        let research = id("research");
        let synthesis = id("synthesis");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                StageDefinition::new(
                    research.clone(),
                    StageKind::DeepAnalysis,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    synthesis.clone(),
                    StageKind::Synthesis,
                    Role::Reviewer,
                    vec![Dependency::required(research.clone())],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);

        assert!(
            run.stage_dependency_report(&synthesis).is_some(),
            "a pending stage with an unmet dependency has something to report"
        );

        complete_stage(&mut run, &research, 80);
        assert!(
            run.stage_dependency_report(&synthesis).is_none(),
            "a pending stage whose dependencies are all satisfied has nothing left \
             to report, even before the scheduler marks it ready"
        );

        run.transition_stage(&synthesis, StageTransition::MarkReady, metadata(83, 7))
            .unwrap();
        assert!(
            run.stage_dependency_report(&synthesis).is_none(),
            "a ready stage with satisfied dependencies has nothing left to report"
        );

        run.transition_stage(&synthesis, StageTransition::Start, metadata(84, 8))
            .unwrap();
        assert!(
            run.stage_dependency_report(&synthesis).is_none(),
            "a running stage has nothing left to wait on"
        );
    }

    #[test]
    fn stage_dependency_report_is_none_for_unknown_stage() {
        let run = new_run(fast_workflow());
        assert!(run.stage_dependency_report(&id("does-not-exist")).is_none());
    }

    #[test]
    fn optional_failed_branch_can_complete_but_required_or_leaf_failure_cannot() {
        let optional = id("optional_review");
        let synthesis = id("synthesis");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Review,
            vec![
                StageDefinition::new(
                    optional.clone(),
                    StageKind::IndependentReview,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    synthesis.clone(),
                    StageKind::Synthesis,
                    Role::Reviewer,
                    vec![Dependency::optional(optional.clone())],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);
        run.transition_stage(&optional, StageTransition::MarkReady, metadata(90, 4))
            .unwrap();
        run.transition_stage(&optional, StageTransition::Start, metadata(91, 5))
            .unwrap();
        run.transition_stage(&optional, StageTransition::Fail, metadata(92, 6))
            .unwrap();
        complete_stage(&mut run, &synthesis, 93);
        run.transition(RunTransition::Complete, metadata(96, 10))
            .unwrap();
        assert_eq!(run.status(), RunStatus::Completed);

        let mut failed_leaf = new_run(fast_workflow());
        start_run(&mut failed_leaf);
        let implementation = id("implementation");
        failed_leaf
            .transition_stage(&implementation, StageTransition::MarkReady, metadata(97, 4))
            .unwrap();
        failed_leaf
            .transition_stage(&implementation, StageTransition::Start, metadata(98, 5))
            .unwrap();
        failed_leaf
            .transition_stage(&implementation, StageTransition::Fail, metadata(99, 6))
            .unwrap();
        assert!(matches!(
            failed_leaf.transition(RunTransition::Complete, metadata(100, 7)),
            Err(RunTransitionError::CompletionBlocked(_))
        ));
    }

    #[test]
    fn unknown_stage_and_retry_execution_boundary_are_enforced() {
        let first = id("first");
        let second = id("second");
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Standard,
            vec![
                StageDefinition::new(
                    first.clone(),
                    StageKind::Architecture,
                    Role::Architect,
                    vec![],
                ),
                StageDefinition::new(
                    second.clone(),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![Dependency::optional(first.clone())],
                ),
            ],
        )
        .unwrap();
        let mut run = new_run(workflow);
        start_run(&mut run);
        assert_eq!(
            run.transition_stage(&id("missing"), StageTransition::MarkReady, metadata(110, 4)),
            Err(RunStageError::UnknownStage(id("missing")))
        );
        run.transition_stage(&first, StageTransition::MarkReady, metadata(111, 4))
            .unwrap();
        run.transition_stage(&first, StageTransition::Start, metadata(112, 5))
            .unwrap();
        run.transition_stage(&first, StageTransition::Fail, metadata(113, 6))
            .unwrap();
        run.transition_stage(&second, StageTransition::MarkReady, metadata(114, 7))
            .unwrap();

        let mut ready_dependent = run.clone();
        ready_dependent
            .transition_stage(&first, StageTransition::Retry, metadata(115, 8))
            .unwrap();
        assert_eq!(
            ready_dependent.stage(&first).unwrap().status(),
            StageStatus::Pending
        );
        assert_eq!(
            ready_dependent.stage(&second).unwrap().status(),
            StageStatus::Ready
        );

        run.transition_stage(&second, StageTransition::Start, metadata(116, 8))
            .unwrap();
        assert!(matches!(
            run.transition_stage(&first, StageTransition::Retry, metadata(117, 9)),
            Err(RunStageError::RetryWouldInvalidate { .. })
        ));
    }

    #[test]
    fn status_and_transition_enums_use_stable_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&RunStatus::NeedsUser).unwrap(),
            "\"needs_user\""
        );
        let decoded: RunStatus = serde_json::from_str("\"interrupted\"").unwrap();
        assert_eq!(decoded, RunStatus::Interrupted);
    }

    #[test]
    fn invariant_validation_rejects_orphaned_suspension_context() {
        let mut run = new_run(fast_workflow());
        run.suspended_from = Some(ResumableRunStatus::Running);

        assert_eq!(
            run.validate_invariants(),
            Err(RunInvariantError::RunSuspensionMismatch)
        );
    }

    #[test]
    fn workflow_constructor_error_type_remains_distinct() {
        let error = WorkflowDefinition::new(WorkflowKind::Fast, vec![]).unwrap_err();
        assert_eq!(error, WorkflowDefinitionError::NoStages);
    }

    fn candidate_for_status(status: RunStatus) -> Run {
        let mut run = new_run(fast_workflow());
        match status {
            RunStatus::Created => {}
            RunStatus::Preparing => {
                run.transition(RunTransition::BeginPreparation, metadata(200, 1))
                    .unwrap();
            }
            RunStatus::Ready => {
                run.transition(RunTransition::BeginPreparation, metadata(201, 1))
                    .unwrap();
                run.transition(RunTransition::FinishPreparation, metadata(202, 2))
                    .unwrap();
            }
            RunStatus::Running => {
                start_run(&mut run);
                complete_stage(&mut run, &id("implementation"), 203);
            }
            RunStatus::NeedsUser => {
                start_run(&mut run);
                let stage_id = id("implementation");
                run.transition_stage(&stage_id, StageTransition::MarkReady, metadata(206, 4))
                    .unwrap();
                run.transition_stage(&stage_id, StageTransition::Start, metadata(207, 5))
                    .unwrap();
                run.request_attention(
                    AttentionRequest::new(
                        AttentionRequestId::from_u128(200),
                        run.id(),
                        stage_id,
                        AttentionKind::Question,
                        "Need input",
                        at(6),
                    )
                    .unwrap(),
                    metadata(208, 6),
                )
                .unwrap();
            }
            RunStatus::Paused => {
                start_run(&mut run);
                complete_stage(&mut run, &id("implementation"), 209);
                run.transition(RunTransition::Pause, metadata(212, 7))
                    .unwrap();
            }
            RunStatus::Interrupted => {
                start_run(&mut run);
                complete_stage(&mut run, &id("implementation"), 213);
                run.transition(RunTransition::Interrupt, metadata(216, 7))
                    .unwrap();
            }
            RunStatus::Completed => {
                start_run(&mut run);
                complete_stage(&mut run, &id("implementation"), 217);
                run.transition(RunTransition::Complete, metadata(220, 7))
                    .unwrap();
            }
            RunStatus::Applied => {
                run = candidate_for_status(RunStatus::Completed);
                run.transition(RunTransition::Apply, metadata(221, 8))
                    .unwrap();
            }
            RunStatus::Discarded => {
                run.transition(RunTransition::Discard, metadata(222, 1))
                    .unwrap();
            }
            RunStatus::Failed => {
                run.transition(RunTransition::BeginPreparation, metadata(223, 1))
                    .unwrap();
                run.transition(RunTransition::Fail, metadata(224, 2))
                    .unwrap();
            }
        }
        run
    }

    #[test]
    fn run_transition_table_rejects_every_unspecified_pair_without_mutation() {
        let statuses = [
            RunStatus::Created,
            RunStatus::Preparing,
            RunStatus::Ready,
            RunStatus::Running,
            RunStatus::NeedsUser,
            RunStatus::Paused,
            RunStatus::Interrupted,
            RunStatus::Completed,
            RunStatus::Applied,
            RunStatus::Discarded,
            RunStatus::Failed,
        ];
        let transitions = [
            RunTransition::BeginPreparation,
            RunTransition::FinishPreparation,
            RunTransition::Start,
            RunTransition::Pause,
            RunTransition::Interrupt,
            RunTransition::Resume,
            RunTransition::Recover,
            RunTransition::Complete,
            RunTransition::Fail,
            RunTransition::Apply,
            RunTransition::Discard,
        ];

        for status in statuses {
            for transition in transitions {
                let mut run = candidate_for_status(status);
                let before = run.clone();
                let result = run.transition(transition, metadata(300, 30));
                let expected = match (status, transition) {
                    (RunStatus::Created, RunTransition::BeginPreparation) => {
                        Some(RunStatus::Preparing)
                    }
                    (RunStatus::Preparing, RunTransition::FinishPreparation) => {
                        Some(RunStatus::Ready)
                    }
                    (RunStatus::Ready, RunTransition::Start)
                    | (RunStatus::Paused, RunTransition::Resume)
                    | (RunStatus::Interrupted, RunTransition::Recover) => Some(RunStatus::Running),
                    (RunStatus::Running | RunStatus::NeedsUser, RunTransition::Pause) => {
                        Some(RunStatus::Paused)
                    }
                    (RunStatus::Running | RunStatus::NeedsUser, RunTransition::Interrupt) => {
                        Some(RunStatus::Interrupted)
                    }
                    (RunStatus::Running, RunTransition::Complete) => Some(RunStatus::Completed),
                    (
                        RunStatus::Preparing
                        | RunStatus::Ready
                        | RunStatus::Running
                        | RunStatus::NeedsUser
                        | RunStatus::Paused
                        | RunStatus::Interrupted,
                        RunTransition::Fail,
                    ) => Some(RunStatus::Failed),
                    (RunStatus::Completed, RunTransition::Apply) => Some(RunStatus::Applied),
                    (
                        RunStatus::Created
                        | RunStatus::Preparing
                        | RunStatus::Ready
                        | RunStatus::Running
                        | RunStatus::NeedsUser
                        | RunStatus::Paused
                        | RunStatus::Interrupted
                        | RunStatus::Completed
                        | RunStatus::Failed,
                        RunTransition::Discard,
                    ) => Some(RunStatus::Discarded),
                    _ => None,
                };

                if let Some(expected) = expected {
                    assert!(result.is_ok(), "expected {status:?} × {transition:?}");
                    assert_eq!(run.status(), expected);
                } else {
                    assert!(result.is_err(), "unexpected {status:?} × {transition:?}");
                    assert_eq!(run, before, "invalid run transition mutated state");
                }
            }
        }
    }

    /// A route override is the operator's answer to "where does this failed
    /// stage go now?", so it is only ever recorded on a failed stage, and it
    /// is part of the stage's state: a snapshot round trip keeps it.
    #[test]
    fn route_override_only_on_a_failed_stage_and_survives_rehydration() {
        let mut run = new_run(fast_workflow());
        start_run(&mut run);
        let implementation = id("implementation");
        let route = StageRouteOverride::new(
            ProviderId::new("claude").unwrap(),
            Some(ModelId::new("opus").unwrap()),
        );
        run.transition_stage(&implementation, StageTransition::MarkReady, metadata(1, 1))
            .unwrap();
        run.transition_stage(&implementation, StageTransition::Start, metadata(2, 2))
            .unwrap();
        assert!(matches!(
            run.override_stage_route(&implementation, route.clone(), "why", metadata(3, 3)),
            Err(RunStageError::Stage(
                StageTransitionError::RouteOverrideNotAllowed {
                    from: StageStatus::Running
                }
            ))
        ));
        assert!(
            run.stage(&implementation)
                .unwrap()
                .route_override()
                .is_none()
        );

        run.transition_stage(&implementation, StageTransition::Fail, metadata(4, 4))
            .unwrap();
        let event = run
            .override_stage_route(
                &implementation,
                route.clone(),
                "operator_override",
                metadata(5, 5),
            )
            .unwrap();
        assert_eq!(event.stage_id(), Some(&implementation));
        assert_eq!(
            event.kind(),
            &DomainEventKind::StageRouteOverridden {
                provider_id: ProviderId::new("claude").unwrap(),
                model_id: Some(ModelId::new("opus").unwrap()),
                reason: "operator_override".to_owned(),
            }
        );
        assert_eq!(
            run.stage(&implementation).unwrap().route_override(),
            Some(&route)
        );

        let rehydrated = Run::rehydrate(run.rehydration_data()).unwrap();
        assert_eq!(rehydrated, run);
        assert_eq!(
            rehydrated.stage(&implementation).unwrap().route_override(),
            Some(&route)
        );
        // Retrying keeps the override: the stage is pending again, but
        // where it runs has already been decided.
        run.transition_stage(&implementation, StageTransition::Retry, metadata(6, 6))
            .unwrap();
        assert_eq!(
            run.stage(&implementation).unwrap().route_override(),
            Some(&route)
        );
    }
}
