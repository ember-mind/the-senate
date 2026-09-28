use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    Dependency, ModelId, ProviderId, Role, RunId, StageDefinition, StageId, StageKind,
    StageRehydrationData, StageResumeStatus, StageSuspensionOwner,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Ready,
    Running,
    NeedsUser,
    Paused,
    Interrupted,
    Completed,
    Skipped,
    Failed,
}

impl StageStatus {
    #[must_use]
    pub const fn is_terminal_outcome(self) -> bool {
        matches!(self, Self::Completed | Self::Skipped | Self::Failed)
    }

    #[must_use]
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Completed | Self::Skipped)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageTransition {
    MarkReady,
    Start,
    Pause,
    Interrupt,
    Resume,
    Recover,
    Complete,
    Skip,
    Fail,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum ResumableStageStatus {
    Running,
    NeedsUser,
}

impl ResumableStageStatus {
    const fn into_status(self) -> StageStatus {
        match self {
            Self::Running => StageStatus::Running,
            Self::NeedsUser => StageStatus::NeedsUser,
        }
    }
}

impl TryFrom<StageStatus> for ResumableStageStatus {
    type Error = ();

    fn try_from(status: StageStatus) -> Result<Self, Self::Error> {
        match status {
            StageStatus::Running => Ok(Self::Running),
            StageStatus::NeedsUser => Ok(Self::NeedsUser),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum SuspensionOwner {
    Stage,
    Run,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Suspension {
    owner: SuspensionOwner,
    resume_to: ResumableStageStatus,
}

/// An operator's explicit destination for one stage, replacing the route its
/// role was given at creation.
///
/// The configuration snapshot stays immutable: this is stage state, recorded
/// when a failed stage is retried somewhere else, and it outlives that retry
/// so a second retry of the same stage does not silently fall back to the
/// route that failed. It never spreads to other stages.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StageRouteOverride {
    provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_id: Option<ModelId>,
}

impl StageRouteOverride {
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

/// One run-bound stage with encapsulated lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Stage {
    run_id: RunId,
    id: StageId,
    kind: StageKind,
    role: Role,
    dependencies: Vec<Dependency>,
    status: StageStatus,
    suspension: Option<Suspension>,
    route_override: Option<StageRouteOverride>,
}

impl Stage {
    pub(crate) fn from_definition(run_id: RunId, definition: &StageDefinition) -> Self {
        Self {
            run_id,
            id: definition.id().clone(),
            kind: definition.kind(),
            role: definition.role(),
            dependencies: definition.dependencies().to_vec(),
            status: StageStatus::Pending,
            suspension: None,
            route_override: None,
        }
    }

    pub(crate) fn rehydrate(
        run_id: RunId,
        definition: &StageDefinition,
        data: &StageRehydrationData,
    ) -> Result<Self, StageRehydrationError> {
        let suspension = match (data.suspension_owner, data.resume_to) {
            (None, None) => None,
            (Some(owner), Some(resume_to)) => Some(Suspension {
                owner: match owner {
                    StageSuspensionOwner::Stage => SuspensionOwner::Stage,
                    StageSuspensionOwner::Run => SuspensionOwner::Run,
                },
                resume_to: match resume_to {
                    StageResumeStatus::Running => ResumableStageStatus::Running,
                    StageResumeStatus::NeedsUser => ResumableStageStatus::NeedsUser,
                },
            }),
            _ => return Err(StageRehydrationError::PartialSuspensionContext),
        };
        let is_suspended = matches!(data.status, StageStatus::Paused | StageStatus::Interrupted);
        if is_suspended != suspension.is_some() {
            return Err(StageRehydrationError::StatusSuspensionMismatch);
        }

        Ok(Self {
            run_id,
            id: definition.id().clone(),
            kind: definition.kind(),
            role: definition.role(),
            dependencies: definition.dependencies().to_vec(),
            status: data.status,
            suspension,
            route_override: data.route_override.clone(),
        })
    }

    #[must_use]
    pub(crate) fn rehydration_data(&self) -> StageRehydrationData {
        let (suspension_owner, resume_to) = self.suspension.map_or((None, None), |suspension| {
            let owner = match suspension.owner {
                SuspensionOwner::Stage => StageSuspensionOwner::Stage,
                SuspensionOwner::Run => StageSuspensionOwner::Run,
            };
            let resume_to = match suspension.resume_to {
                ResumableStageStatus::Running => StageResumeStatus::Running,
                ResumableStageStatus::NeedsUser => StageResumeStatus::NeedsUser,
            };
            (Some(owner), Some(resume_to))
        });
        StageRehydrationData {
            id: self.id.clone(),
            status: self.status,
            suspension_owner,
            resume_to,
            route_override: self.route_override.clone(),
        }
    }

    #[must_use]
    pub(crate) fn has_run_owned_suspension(&self) -> bool {
        self.suspension
            .is_some_and(|suspension| suspension.owner == SuspensionOwner::Run)
    }

    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
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

    #[must_use]
    pub const fn status(&self) -> StageStatus {
        self.status
    }

    /// The operator's explicit destination for this stage, when one was
    /// recorded; `None` means the role's configured route applies.
    #[must_use]
    pub const fn route_override(&self) -> Option<&StageRouteOverride> {
        self.route_override.as_ref()
    }

    /// Records where this stage runs from now on. Only a failed stage can be
    /// sent somewhere else: a running one has a provider session that would
    /// be orphaned, and a finished one has nothing left to run.
    pub(crate) fn override_route(
        &mut self,
        route: StageRouteOverride,
    ) -> Result<(), StageTransitionError> {
        match self.status {
            StageStatus::Failed => {
                self.route_override = Some(route);
                Ok(())
            }
            from => Err(StageTransitionError::RouteOverrideNotAllowed { from }),
        }
    }

    pub(crate) fn transition(
        &mut self,
        transition: StageTransition,
    ) -> Result<(), StageTransitionError> {
        let from = self.status;
        match (from, transition) {
            (StageStatus::Pending, StageTransition::MarkReady) => {
                self.status = StageStatus::Ready;
            }
            (StageStatus::Ready, StageTransition::Start) => {
                self.status = StageStatus::Running;
            }
            (StageStatus::Pending | StageStatus::Ready, StageTransition::Skip) => {
                self.status = StageStatus::Skipped;
            }
            (StageStatus::Running | StageStatus::NeedsUser, StageTransition::Pause) => {
                self.suspend(SuspensionOwner::Stage, StageStatus::Paused)?;
            }
            (StageStatus::Running | StageStatus::NeedsUser, StageTransition::Interrupt) => {
                self.suspend(SuspensionOwner::Stage, StageStatus::Interrupted)?;
            }
            (StageStatus::Paused, StageTransition::Resume)
            | (StageStatus::Interrupted, StageTransition::Recover) => {
                self.restore(SuspensionOwner::Stage)?;
            }
            (StageStatus::Running, StageTransition::Complete) => {
                self.status = StageStatus::Completed;
            }
            (StageStatus::Running | StageStatus::NeedsUser, StageTransition::Fail) => {
                self.status = StageStatus::Failed;
                self.suspension = None;
            }
            // A skipped stage is retried alongside the failure that skipped
            // it: it never ran, so returning it to pending discards nothing.
            (StageStatus::Failed | StageStatus::Skipped, StageTransition::Retry) => {
                self.status = StageStatus::Pending;
            }
            _ => {
                return Err(StageTransitionError::InvalidTransition { from, transition });
            }
        }
        Ok(())
    }

    pub(crate) fn request_attention(&mut self) -> Result<(), StageTransitionError> {
        match self.status {
            StageStatus::Running | StageStatus::NeedsUser => {
                self.status = StageStatus::NeedsUser;
                Ok(())
            }
            from => Err(StageTransitionError::AttentionNotAllowed { from }),
        }
    }

    pub(crate) fn attention_resolved(&mut self, still_pending: bool) {
        if still_pending {
            return;
        }
        match self.status {
            StageStatus::NeedsUser => self.status = StageStatus::Running,
            StageStatus::Paused | StageStatus::Interrupted => {
                if let Some(suspension) = &mut self.suspension
                    && suspension.resume_to == ResumableStageStatus::NeedsUser
                {
                    suspension.resume_to = ResumableStageStatus::Running;
                }
            }
            _ => {}
        }
    }

    pub(crate) fn pause_for_run(&mut self) {
        if matches!(self.status, StageStatus::Running | StageStatus::NeedsUser) {
            let result = self.suspend(SuspensionOwner::Run, StageStatus::Paused);
            debug_assert!(result.is_ok(), "active stage must be suspendable");
        }
    }

    pub(crate) fn interrupt_for_run(&mut self) {
        if matches!(self.status, StageStatus::Running | StageStatus::NeedsUser) {
            let result = self.suspend(SuspensionOwner::Run, StageStatus::Interrupted);
            debug_assert!(result.is_ok(), "active stage must be interruptible");
        }
    }

    pub(crate) fn resume_for_run(&mut self) {
        if self.status == StageStatus::Paused
            && self
                .suspension
                .is_some_and(|item| item.owner == SuspensionOwner::Run)
        {
            let result = self.restore(SuspensionOwner::Run);
            debug_assert!(result.is_ok(), "run-paused stage must be resumable");
        }
    }

    pub(crate) fn recover_for_run(&mut self) {
        if self.status == StageStatus::Interrupted
            && self
                .suspension
                .is_some_and(|item| item.owner == SuspensionOwner::Run)
        {
            let result = self.restore(SuspensionOwner::Run);
            debug_assert!(result.is_ok(), "run-interrupted stage must be recoverable");
        }
    }

    #[must_use]
    pub(crate) fn expects_attention(&self) -> bool {
        if self.status == StageStatus::NeedsUser {
            return true;
        }
        matches!(
            self.suspension,
            Some(Suspension {
                resume_to: ResumableStageStatus::NeedsUser,
                ..
            })
        )
    }

    fn suspend(
        &mut self,
        owner: SuspensionOwner,
        suspended_status: StageStatus,
    ) -> Result<(), StageTransitionError> {
        let resume_to = ResumableStageStatus::try_from(self.status)
            .map_err(|()| StageTransitionError::InvalidSuspensionSource { from: self.status })?;
        self.suspension = Some(Suspension { owner, resume_to });
        self.status = suspended_status;
        Ok(())
    }

    fn restore(&mut self, owner: SuspensionOwner) -> Result<(), StageTransitionError> {
        let suspension = self
            .suspension
            .ok_or(StageTransitionError::MissingSuspensionContext)?;
        if suspension.owner != owner {
            return Err(StageTransitionError::WrongSuspensionOwner);
        }
        self.status = suspension.resume_to.into_status();
        self.suspension = None;
        Ok(())
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum StageTransitionError {
    #[error("stage transition {transition:?} is invalid from {from:?}")]
    InvalidTransition {
        from: StageStatus,
        transition: StageTransition,
    },
    #[error("stage cannot request attention from {from:?}")]
    AttentionNotAllowed { from: StageStatus },
    #[error("only a failed stage can be sent to another provider; this one is {from:?}")]
    RouteOverrideNotAllowed { from: StageStatus },
    #[error("stage cannot suspend from {from:?}")]
    InvalidSuspensionSource { from: StageStatus },
    #[error("stage suspension context is missing")]
    MissingSuspensionContext,
    #[error("stage suspension belongs to a different lifecycle owner")]
    WrongSuspensionOwner,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum StageRehydrationError {
    #[error("stage suspension owner and resume target must both be present or absent")]
    PartialSuspensionContext,
    #[error("stage status and suspension context disagree")]
    StatusSuspensionMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage() -> Stage {
        Stage::from_definition(
            RunId::from_u128(1),
            &StageDefinition::new(
                StageId::new("implementation").unwrap(),
                StageKind::Implementation,
                Role::Implementer,
                vec![],
            ),
        )
    }

    #[test]
    fn happy_path_is_pending_ready_running_completed() {
        let mut stage = stage();
        for (transition, status) in [
            (StageTransition::MarkReady, StageStatus::Ready),
            (StageTransition::Start, StageStatus::Running),
            (StageTransition::Complete, StageStatus::Completed),
        ] {
            stage.transition(transition).unwrap();
            assert_eq!(stage.status(), status);
        }
    }

    #[test]
    fn pause_and_interruption_require_distinct_recovery_actions() {
        let mut paused = stage();
        paused.transition(StageTransition::MarkReady).unwrap();
        paused.transition(StageTransition::Start).unwrap();
        paused.transition(StageTransition::Pause).unwrap();
        assert_eq!(paused.status(), StageStatus::Paused);
        assert!(paused.transition(StageTransition::Recover).is_err());
        paused.transition(StageTransition::Resume).unwrap();
        assert_eq!(paused.status(), StageStatus::Running);

        let mut interrupted = stage();
        interrupted.transition(StageTransition::MarkReady).unwrap();
        interrupted.transition(StageTransition::Start).unwrap();
        interrupted.transition(StageTransition::Interrupt).unwrap();
        assert_eq!(interrupted.status(), StageStatus::Interrupted);
        assert!(interrupted.transition(StageTransition::Resume).is_err());
        interrupted.transition(StageTransition::Recover).unwrap();
        assert_eq!(interrupted.status(), StageStatus::Running);
    }

    #[test]
    fn failed_stage_requires_explicit_retry() {
        let mut stage = stage();
        stage.transition(StageTransition::MarkReady).unwrap();
        stage.transition(StageTransition::Start).unwrap();
        stage.transition(StageTransition::Fail).unwrap();
        assert_eq!(stage.status(), StageStatus::Failed);
        assert!(stage.transition(StageTransition::Start).is_err());
        stage.transition(StageTransition::Retry).unwrap();
        assert_eq!(stage.status(), StageStatus::Pending);
    }

    #[test]
    fn every_unspecified_transition_is_rejected() {
        let statuses = [
            StageStatus::Pending,
            StageStatus::Ready,
            StageStatus::Running,
            StageStatus::NeedsUser,
            StageStatus::Paused,
            StageStatus::Interrupted,
            StageStatus::Completed,
            StageStatus::Skipped,
            StageStatus::Failed,
        ];
        let transitions = [
            StageTransition::MarkReady,
            StageTransition::Start,
            StageTransition::Pause,
            StageTransition::Interrupt,
            StageTransition::Resume,
            StageTransition::Recover,
            StageTransition::Complete,
            StageTransition::Skip,
            StageTransition::Fail,
            StageTransition::Retry,
        ];
        let allowed = [
            (StageStatus::Pending, StageTransition::MarkReady),
            (StageStatus::Pending, StageTransition::Skip),
            (StageStatus::Ready, StageTransition::Start),
            (StageStatus::Ready, StageTransition::Skip),
            (StageStatus::Running, StageTransition::Pause),
            (StageStatus::Running, StageTransition::Interrupt),
            (StageStatus::Running, StageTransition::Complete),
            (StageStatus::Running, StageTransition::Fail),
            (StageStatus::NeedsUser, StageTransition::Pause),
            (StageStatus::NeedsUser, StageTransition::Interrupt),
            (StageStatus::NeedsUser, StageTransition::Fail),
            (StageStatus::Failed, StageTransition::Retry),
            (StageStatus::Skipped, StageTransition::Retry),
        ];

        for status in statuses {
            for transition in transitions {
                if matches!(
                    (status, transition),
                    (StageStatus::Paused, StageTransition::Resume)
                        | (StageStatus::Interrupted, StageTransition::Recover)
                ) {
                    continue;
                }
                let mut candidate = stage();
                candidate.status = status;
                candidate.suspension = None;
                let before = candidate.clone();
                let result = candidate.transition(transition);
                if allowed.contains(&(status, transition)) {
                    assert!(result.is_ok(), "expected {status:?} × {transition:?}");
                } else {
                    assert!(result.is_err(), "unexpected {status:?} × {transition:?}");
                    assert_eq!(candidate, before, "invalid transition mutated stage");
                }
            }
        }
    }
}
