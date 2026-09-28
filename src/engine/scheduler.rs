use chrono::{DateTime, Utc};

use crate::domain::{
    AttentionRequest, AttentionRequestId, AttentionStatus, DomainEvent, DomainEventKind, EventId,
    EventMetadata, ProviderId, ProviderSessionId, Role, Run, RunId, RunStageError, RunStatus,
    RunTransition, RunTransitionError, StageId, StageRouteOverride, StageStatus, StageTransition,
};
use crate::store::{LoadedRun, RunRevision, SequencedEvent, SqliteStore};
use crate::workspace::{ApplyStatus, RunWorkspace, WorkspaceStatus};

use super::{
    EngineError, Provider, ProviderAttentionContext, ProviderPoll, ProviderRequest, ProviderSignal,
};

const DEFAULT_DRIVE_LIMIT: usize = 10_000;

/// Supplies IDs and monotonic event timestamps without coupling domain logic to
/// wall clock access. Tests can inject exact deterministic values.
pub trait ExecutionContext {
    fn next_event_metadata(&mut self, not_before: DateTime<Utc>) -> EventMetadata;
    fn next_attention_id(&mut self) -> AttentionRequestId;
}

#[derive(Default)]
pub struct SystemExecutionContext;

impl ExecutionContext for SystemExecutionContext {
    fn next_event_metadata(&mut self, not_before: DateTime<Utc>) -> EventMetadata {
        let now: DateTime<Utc> = std::time::SystemTime::now().into();
        EventMetadata::new(EventId::new(), now.max(not_before))
    }

    fn next_attention_id(&mut self) -> AttentionRequestId {
        AttentionRequestId::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineStatus {
    Advanced {
        run_status: RunStatus,
    },
    WaitingForProvider {
        stage_id: StageId,
        keep_attached: bool,
    },
    NeedsUser {
        requests: Vec<AttentionRequestId>,
    },
    Paused {
        stages: Vec<StageId>,
    },
    Interrupted {
        stages: Vec<StageId>,
    },
    Finished {
        run_status: RunStatus,
    },
}

/// The run's event log as this engine has read it so far.
///
/// One drive is a loop of ticks, and every tick that polls a stage needs the
/// log to rebuild that stage's provider checkpoint. Re-reading and re-parsing
/// the whole history each time made a drive quadratic in the events it was
/// itself appending. The log is append-only and, for the length of one drive,
/// this engine is the only thing writing to it, so the prefix already read
/// stays true and only the tail has to be fetched.
struct EventLog {
    run_id: RunId,
    events: Vec<SequencedEvent>,
}

impl EventLog {
    fn last_sequence(&self) -> u64 {
        self.events.last().map_or(0, |event| event.sequence)
    }
}

/// Deterministic synchronous DAG scheduler for one resolved provider.
pub struct WorkflowEngine<P, C = SystemExecutionContext> {
    provider: P,
    task: String,
    context: C,
    drive_limit: usize,
    observe_only: bool,
    events: Option<EventLog>,
}

impl<P> WorkflowEngine<P, SystemExecutionContext>
where
    P: Provider,
{
    #[must_use]
    pub fn new(provider: P, task: impl Into<String>) -> Self {
        Self::with_context(provider, task.into(), SystemExecutionContext)
    }
}

impl<P, C> WorkflowEngine<P, C>
where
    P: Provider,
    C: ExecutionContext,
{
    #[must_use]
    pub const fn with_context(provider: P, task: String, context: C) -> Self {
        Self {
            provider,
            task,
            context,
            drive_limit: DEFAULT_DRIVE_LIMIT,
            observe_only: false,
            events: None,
        }
    }

    /// The run's events, reading only what this engine has not already seen.
    fn events(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<&[SequencedEvent], EngineError> {
        let log = match self.events.take() {
            Some(log) if log.run_id == run_id => log,
            // A different run means a different log, and no prefix worth
            // keeping.
            _ => EventLog {
                run_id,
                events: Vec::new(),
            },
        };
        let mut log = log;
        let tail = store.load_events_after(run_id, log.last_sequence())?;
        log.events.extend(tail);
        Ok(&self.events.insert(log).events)
    }

    /// Drives without ever starting or resuming provider work.
    ///
    /// Used by the control plane when stopping a run: the processes have
    /// already been interrupted and the engine is asked only to record that,
    /// never to continue the conversation it just halted.
    ///
    /// # Errors
    /// Returns the same lifecycle, persistence, and provider errors as
    /// [`Self::drive`].
    pub fn drive_observing(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<EngineStatus, EngineError> {
        self.observe_only = true;
        let outcome = self.drive(store, run_id);
        self.observe_only = false;
        outcome
    }

    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    #[must_use]
    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    /// Executes at most one persisted scheduler/provider transition.
    ///
    /// # Errors
    /// Rejects invalid infrastructure state, active apply intent, unsupported
    /// roles, provider protocol violations, stale writes, and domain failures.
    pub fn tick(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<EngineStatus, EngineError> {
        let (loaded, workspace) = load_execution_boundary(store, run_id)?;
        match loaded.run.status() {
            RunStatus::Ready => self.start_run(store, loaded),
            RunStatus::Running => self.tick_running(store, loaded, &workspace),
            RunStatus::NeedsUser => Ok(needs_user_status(&loaded.run)),
            RunStatus::Paused => Ok(EngineStatus::Paused { stages: Vec::new() }),
            RunStatus::Interrupted => Ok(EngineStatus::Interrupted { stages: Vec::new() }),
            status if status.is_execution_finished() => {
                Ok(EngineStatus::Finished { run_status: status })
            }
            status => Err(EngineError::RunNotPrepared(status)),
        }
    }

    /// Runs deterministic ticks until work blocks or run execution finishes.
    ///
    /// # Errors
    /// Returns first tick failure or a safety-limit error for a malformed
    /// provider that never reaches a blocking/finished condition.
    pub fn drive(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<EngineStatus, EngineError> {
        for _ in 0..self.drive_limit {
            let status = self.tick(store, run_id)?;
            if !matches!(status, EngineStatus::Advanced { .. }) {
                return Ok(status);
            }
        }
        Err(EngineError::DriveLimit(self.drive_limit))
    }

    /// Resolves one persisted request through same guarded execution boundary.
    ///
    /// # Errors
    /// Returns infrastructure, domain, concurrency, or persistence failures.
    pub fn resolve_attention(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        request_id: AttentionRequestId,
    ) -> Result<EngineStatus, EngineError> {
        self.resolve_attention_with_response(store, run_id, request_id, None)
    }

    /// Checks whether provider-specific disposable-eval policy permits resolving
    /// one attention request without human input.
    ///
    /// # Errors
    /// Returns execution-boundary, provider, or persistence failures.
    pub fn can_auto_resolve_attention(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        request_id: AttentionRequestId,
    ) -> Result<bool, EngineError> {
        let (loaded, _) = load_execution_boundary(store, run_id)?;
        let attention = loaded
            .run
            .attention_requests()
            .iter()
            .find(|request| request.id() == request_id)
            .ok_or(crate::domain::RunAttentionError::UnknownRequest(request_id))?;
        let stage = loaded.run.stage(attention.stage_id()).ok_or_else(|| {
            EngineError::ProviderProtocol {
                stage_id: attention.stage_id().clone(),
                message: "attention references unknown stage".to_owned(),
            }
        })?;
        let context = ProviderAttentionContext::new(
            run_id,
            stage.id().clone(),
            stage.kind(),
            stage.role(),
            request_id,
        )
        .with_route_override(stage.route_override().cloned());
        self.provider
            .can_auto_resolve_attention(store, &context)
            .map_err(EngineError::from)
    }

    /// Resolves attention after provider safely stages optional response input.
    ///
    /// # Errors
    /// Returns provider, infrastructure, domain, concurrency, or persistence failures.
    pub fn resolve_attention_with_response(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        request_id: AttentionRequestId,
        response: Option<&str>,
    ) -> Result<EngineStatus, EngineError> {
        let (mut loaded, _) = load_execution_boundary(store, run_id)?;
        let attention = loaded
            .run
            .attention_requests()
            .iter()
            .find(|request| request.id() == request_id)
            .ok_or(crate::domain::RunAttentionError::UnknownRequest(request_id))?;
        let stage = loaded.run.stage(attention.stage_id()).ok_or_else(|| {
            EngineError::ProviderProtocol {
                stage_id: attention.stage_id().clone(),
                message: "attention references unknown stage".to_owned(),
            }
        })?;
        let context = ProviderAttentionContext::new(
            run_id,
            stage.id().clone(),
            stage.kind(),
            stage.role(),
            request_id,
        )
        .with_route_override(stage.route_override().cloned());
        self.provider
            .stage_attention_response(store, &context, response)?;
        let metadata = self.metadata_for(&loaded.run);
        let event = loaded.run.resolve_attention(request_id, metadata)?;
        commit_execution(store, &loaded.run, loaded.revision, &[event])?;
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    /// Resumes one provider-paused stage through guarded execution boundary.
    ///
    /// # Errors
    /// Returns infrastructure, lifecycle, concurrency, or persistence failures.
    pub fn resume_stage(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        stage_id: &StageId,
    ) -> Result<EngineStatus, EngineError> {
        self.transition_stage(store, run_id, stage_id, StageTransition::Resume)
    }

    /// Recovers one provider-interrupted stage through guarded boundary.
    ///
    /// # Errors
    /// Returns infrastructure, lifecycle, concurrency, or persistence failures.
    pub fn recover_stage(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        stage_id: &StageId,
    ) -> Result<EngineStatus, EngineError> {
        self.transition_stage(store, run_id, stage_id, StageTransition::Recover)
    }

    /// Resumes one run-level pause through guarded execution boundary.
    ///
    /// # Errors
    /// Returns infrastructure, lifecycle, concurrency, or persistence failures.
    pub fn resume_run(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<EngineStatus, EngineError> {
        self.transition_run(store, run_id, RunTransition::Resume)
    }

    /// Recovers one run-level interruption through guarded execution boundary.
    ///
    /// # Errors
    /// Returns infrastructure, lifecycle, concurrency, or persistence failures.
    pub fn recover_run(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<EngineStatus, EngineError> {
        self.transition_run(store, run_id, RunTransition::Recover)
    }

    /// Schedules explicit safe retry for one failed stage.
    ///
    /// # Errors
    /// Returns infrastructure, retry-safety, concurrency, or persistence failures.
    /// Returns one failed stage to pending, together with every downstream
    /// stage its failure skipped, in one commit: a retried implementation
    /// whose verification stayed skipped would succeed into a run that can
    /// never complete.
    ///
    /// With `route`, the stage is first sent to that provider and model; the
    /// override is committed in the same transaction as the retry, so a
    /// crash between the two cannot leave a stage rerouted but not retried.
    /// Only the retried stage moves: the descendants it un-skips keep the
    /// routes their roles were configured with.
    pub fn retry_stage(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        stage_id: &StageId,
        route: Option<(StageRouteOverride, &str)>,
    ) -> Result<EngineStatus, EngineError> {
        let (mut loaded, _) = load_execution_boundary(store, run_id)?;
        let skipped = loaded.run.skipped_descendants(stage_id);
        let mut events = Vec::with_capacity(skipped.len() + 2);
        if let Some((route, reason)) = route {
            let metadata = self.metadata_for(&loaded.run);
            events.push(
                loaded
                    .run
                    .override_stage_route(stage_id, route, reason, metadata)?,
            );
        }
        for id in std::iter::once(stage_id).chain(skipped.iter()) {
            let metadata = self.metadata_for(&loaded.run);
            events.push(
                loaded
                    .run
                    .transition_stage(id, StageTransition::Retry, metadata)?,
            );
        }
        commit_execution(store, &loaded.run, loaded.revision, &events)?;
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    /// Reopens a completed run with one remediation cycle appended.
    ///
    /// Goes through the same execution boundary as every other mutation, so a
    /// run whose workspace is gone or whose apply is already under way is
    /// refused here rather than discovered halfway through the cycle.
    ///
    /// # Errors
    /// Returns boundary, lifecycle, or persistence failures.
    pub fn request_fix(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<EngineStatus, EngineError> {
        let (mut loaded, _) = load_execution_boundary(store, run_id)?;
        let metadata = self.metadata_for(&loaded.run);
        let event = loaded.run.request_fix(metadata)?;
        commit_execution(store, &loaded.run, loaded.revision, &[event])?;
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    /// Reopens a completed run with one continue cycle appended, carrying the
    /// operator's own instruction rather than answering blocking findings.
    ///
    /// Same execution boundary as [`Self::request_fix`], for the same reason:
    /// a run whose workspace is gone or whose apply is already under way is
    /// refused here rather than discovered halfway through the cycle. Stages
    /// the instruction with the provider — under the exact stage identity the
    /// cycle is about to use — before the domain commit that creates that
    /// stage, mirroring [`Self::resolve_attention_with_response`]'s ordering
    /// one step earlier: nothing here can create a stage whose agent finds no
    /// instruction waiting for it.
    ///
    /// A refusal must stay side-effect-free. If the domain transition is
    /// rejected, or its commit loses a concurrency race, the just-staged
    /// instruction is walked back before the error returns — otherwise it
    /// would durably occupy the exact stage identity a retry, possibly with
    /// different text, is about to reuse, and that retry would fail against
    /// content no stage ever actually read.
    ///
    /// That cleanup must not be unconditional. The predicted stage identity
    /// is deterministic, so two concurrent callers issuing the same request
    /// can stage the identical instruction under the identical path before
    /// either commits; one commit wins the race and the other loses it. If
    /// the loser discarded on sight, it would delete the file the winner's
    /// own follow-up stage is about to read. So a failed commit is followed
    /// by one more read: only when the predicted stage still does not exist
    /// afterward is it safe to conclude nothing durable ever claimed this
    /// call's write, and only then does cleanup proceed. An unreadable state
    /// after the failure counts as "someone else's" and skips cleanup too —
    /// an orphaned file is a retry-time conflict to resolve, never a reason
    /// to risk deleting a winner's instruction.
    ///
    /// # Errors
    /// Returns boundary, provider, lifecycle, or persistence failures.
    pub fn request_continue(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        instruction: &str,
    ) -> Result<EngineStatus, EngineError> {
        let (mut loaded, _) = load_execution_boundary(store, run_id)?;
        let follow_up_stage_id = crate::domain::next_follow_up_stage_id(loaded.run.workflow());
        if let Some(stage_id) = &follow_up_stage_id {
            self.provider.stage_continue_instruction(
                store,
                run_id,
                stage_id,
                Role::Implementer,
                instruction,
            )?;
        }
        let metadata = self.metadata_for(&loaded.run);
        let outcome = loaded
            .run
            .request_continue(metadata)
            .map_err(EngineError::from)
            .and_then(|event| commit_execution(store, &loaded.run, loaded.revision, &[event]));
        if let Err(error) = outcome {
            if let Some(stage_id) = &follow_up_stage_id {
                // Best-effort, and only when nothing durable now owns this
                // identity — see the doc comment above for why an
                // unconditional discard is unsafe under a concurrent winner.
                let claimed_by_someone_else = store
                    .load_run(run_id)
                    .map_or(true, |reloaded| reloaded.run.stage(stage_id).is_some());
                if !claimed_by_someone_else {
                    let _ = self
                        .provider
                        .discard_continue_instruction(store, run_id, stage_id);
                }
            }
            return Err(error);
        }
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    /// One more exchange with a mission's lead: stages the user's message
    /// under the lead stage identity about to be created, then appends
    /// that stage. Same ordering and same walk-back on refusal as
    /// [`Self::request_continue`], for the same reasons.
    ///
    /// # Errors
    /// Returns boundary, provider, lifecycle, or persistence failures.
    pub fn request_lead_turn(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        message: &str,
    ) -> Result<EngineStatus, EngineError> {
        let (mut loaded, _) = load_execution_boundary(store, run_id)?;
        let lead_stage_id = crate::domain::next_lead_stage_id(loaded.run.workflow());
        if let Some(stage_id) = &lead_stage_id {
            self.provider.stage_continue_instruction(
                store,
                run_id,
                stage_id,
                Role::EngineeringLead,
                message,
            )?;
        }
        let metadata = self.metadata_for(&loaded.run);
        let outcome = loaded
            .run
            .request_lead_turn(metadata)
            .map_err(EngineError::from)
            .and_then(|event| commit_execution(store, &loaded.run, loaded.revision, &[event]));
        if let Err(error) = outcome {
            if let Some(stage_id) = &lead_stage_id {
                let claimed_by_someone_else = store
                    .load_run(run_id)
                    .map_or(true, |reloaded| reloaded.run.stage(stage_id).is_some());
                if !claimed_by_someone_else {
                    let _ = self
                        .provider
                        .discard_continue_instruction(store, run_id, stage_id);
                }
            }
            return Err(error);
        }
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    fn transition_stage(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        stage_id: &StageId,
        transition: StageTransition,
    ) -> Result<EngineStatus, EngineError> {
        let (mut loaded, _) = load_execution_boundary(store, run_id)?;
        let metadata = self.metadata_for(&loaded.run);
        let event = loaded
            .run
            .transition_stage(stage_id, transition, metadata)?;
        commit_execution(store, &loaded.run, loaded.revision, &[event])?;
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    fn transition_run(
        &mut self,
        store: &mut SqliteStore,
        run_id: RunId,
        transition: RunTransition,
    ) -> Result<EngineStatus, EngineError> {
        let (mut loaded, _) = load_execution_boundary(store, run_id)?;
        let metadata = self.metadata_for(&loaded.run);
        let event = loaded.run.transition(transition, metadata)?;
        commit_execution(store, &loaded.run, loaded.revision, &[event])?;
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    fn start_run(
        &mut self,
        store: &mut SqliteStore,
        mut loaded: LoadedRun,
    ) -> Result<EngineStatus, EngineError> {
        let metadata = self.metadata_for(&loaded.run);
        let event = loaded.run.transition(RunTransition::Start, metadata)?;
        commit_execution(store, &loaded.run, loaded.revision, &[event])?;
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    fn tick_running(
        &mut self,
        store: &mut SqliteStore,
        mut loaded: LoadedRun,
        workspace: &RunWorkspace,
    ) -> Result<EngineStatus, EngineError> {
        let readiness_events = self.evaluate_dependencies(&mut loaded.run)?;
        if !readiness_events.is_empty() {
            commit_execution(store, &loaded.run, loaded.revision, &readiness_events)?;
            return Ok(EngineStatus::Advanced {
                run_status: loaded.run.status(),
            });
        }

        let stage = loaded
            .run
            .stages()
            .iter()
            .find(|stage| stage.status() == StageStatus::Running)
            .or_else(|| {
                loaded
                    .run
                    .stages()
                    .iter()
                    .find(|stage| stage.status() == StageStatus::Ready)
            })
            .cloned();
        if let Some(stage) = stage {
            return self.poll_stage(store, loaded, workspace, stage.id());
        }

        if loaded
            .run
            .stages()
            .iter()
            .all(|stage| stage.status().is_terminal_outcome())
        {
            return self.finish_run(store, loaded);
        }

        let paused = stages_with_status(&loaded.run, StageStatus::Paused);
        if !paused.is_empty() {
            return Ok(EngineStatus::Paused { stages: paused });
        }
        let interrupted = stages_with_status(&loaded.run, StageStatus::Interrupted);
        if !interrupted.is_empty() {
            return Ok(EngineStatus::Interrupted {
                stages: interrupted,
            });
        }
        Err(EngineError::NoProgress(loaded.run.id()))
    }

    fn evaluate_dependencies(&mut self, run: &mut Run) -> Result<Vec<DomainEvent>, EngineError> {
        let pending = run
            .stages()
            .iter()
            .filter(|stage| stage.status() == StageStatus::Pending)
            .map(|stage| stage.id().clone())
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        for stage_id in pending {
            let metadata = self.metadata_for(run);
            match run.transition_stage(&stage_id, StageTransition::MarkReady, metadata) {
                Ok(event) => events.push(event),
                Err(RunStageError::DependenciesNotFinished { .. }) => {}
                Err(RunStageError::RequiredDependenciesBlocked { .. }) => {
                    let metadata = self.metadata_for(run);
                    events.push(run.transition_stage(
                        &stage_id,
                        StageTransition::Skip,
                        metadata,
                    )?);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(events)
    }

    fn poll_stage(
        &mut self,
        store: &mut SqliteStore,
        mut loaded: LoadedRun,
        workspace: &RunWorkspace,
        stage_id: &StageId,
    ) -> Result<EngineStatus, EngineError> {
        let checkpoint = reduce_checkpoint(self.events(store, loaded.run.id())?, stage_id)?;
        let stage = loaded
            .run
            .stage(stage_id)
            .expect("selected stage must remain in loaded run");
        let request = ProviderRequest::new(
            loaded.run.id(),
            stage.id().clone(),
            stage.kind(),
            stage.status(),
            stage.role(),
            self.task.clone(),
            workspace.worktree_path().to_path_buf(),
            checkpoint.attempt,
            checkpoint.signal_index,
            checkpoint.session_id.clone(),
            stage
                .dependencies()
                .iter()
                .map(|dependency| dependency.stage_id().clone())
                .collect(),
        )
        .with_route_override(stage.route_override().cloned());
        // A stop pass observes; it must never become a reason to resume.
        let request = if self.observe_only {
            request.observing()
        } else {
            request
        };
        let provider_id = self.provider.provider_id_for(&request)?;
        if let Some(previous) = checkpoint
            .provider_id
            .as_ref()
            .filter(|previous| *previous != &provider_id)
        {
            return Err(EngineError::ProviderChanged {
                stage_id: stage_id.clone(),
                previous: previous.to_string(),
                current: provider_id.to_string(),
            });
        }
        if !self.provider.supports_request(&request)? {
            return Err(EngineError::UnsupportedRole(stage.role()));
        }
        match self.provider.poll(store, &request)? {
            ProviderPoll::Pending => Ok(EngineStatus::WaitingForProvider {
                stage_id: stage_id.clone(),
                keep_attached: self.provider.keep_attached_for(&request)?,
            }),
            ProviderPoll::Checkpoint(commit) => {
                store.commit_provider_checkpoint(&commit)?;
                Ok(EngineStatus::Advanced {
                    run_status: loaded.run.status(),
                })
            }
            ProviderPoll::Signal(signal) => {
                let emitted = self.consume_signal(
                    &mut loaded.run,
                    stage_id,
                    &provider_id,
                    checkpoint.session_id,
                    signal,
                )?;
                // A signal can be legitimately unrecordable — a stage
                // interrupted before it ever started has nothing to transition
                // and nothing to record. Committing an empty batch is refused
                // by the store, so skip the write instead of failing the call.
                if !emitted.is_empty() {
                    commit_execution(store, &loaded.run, loaded.revision, &emitted)?;
                }
                if self.observe_only && emitted.is_empty() {
                    return self.observation_settled(&request, stage_id);
                }
                Ok(EngineStatus::Advanced {
                    run_status: loaded.run.status(),
                })
            }
            ProviderPoll::Emission { signals, commit } => {
                if signals.is_empty() {
                    return Err(EngineError::ProviderProtocol {
                        stage_id: stage_id.clone(),
                        message: "provider emitted an empty semantic batch".to_owned(),
                    });
                }
                let mut emitted = Vec::new();
                for signal in signals {
                    emitted.extend(self.consume_signal(
                        &mut loaded.run,
                        stage_id,
                        &provider_id,
                        checkpoint.session_id.clone(),
                        signal,
                    )?);
                }
                commit_emission(store, &loaded, &emitted, &commit)?;
                if self.observe_only && emitted.is_empty() {
                    return self.observation_settled(&request, stage_id);
                }
                Ok(EngineStatus::Advanced {
                    run_status: loaded.run.status(),
                })
            }
        }
    }

    /// Ends an observing drive that has stopped learning anything.
    ///
    /// Observation reports what it finds; it never moves the run forward. A
    /// pass that records no event has reached its fixed point, and reporting it
    /// as progress would spin the drive loop until the limit — a stage that is
    /// still Ready keeps producing the same terminal fact about a process that
    /// died before it ever started.
    ///
    /// # Errors
    /// Returns provider failures raised while reading attachment.
    fn observation_settled(
        &mut self,
        request: &ProviderRequest,
        stage_id: &StageId,
    ) -> Result<EngineStatus, EngineError> {
        Ok(EngineStatus::WaitingForProvider {
            stage_id: stage_id.clone(),
            keep_attached: self.provider.keep_attached_for(request)?,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "single exhaustive provider protocol match keeps lifecycle mapping auditable"
    )]
    fn consume_signal(
        &mut self,
        run: &mut Run,
        stage_id: &StageId,
        provider_id: &ProviderId,
        session_id: Option<ProviderSessionId>,
        signal: ProviderSignal,
    ) -> Result<Vec<DomainEvent>, EngineError> {
        let stage_status = run
            .stage(stage_id)
            .ok_or_else(|| EngineError::ProviderProtocol {
                stage_id: stage_id.clone(),
                message: "stage disappeared before signal consumption".to_owned(),
            })?
            .status();
        let mut events = Vec::new();
        match signal {
            ProviderSignal::Started {
                model_id,
                session_id,
            } if stage_status == StageStatus::Ready => {
                let metadata = self.metadata_for(run);
                events.push(run.transition_stage(stage_id, StageTransition::Start, metadata)?);
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderStarted {
                        provider_id: provider_id.clone(),
                        model_id,
                        session_id,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::Progress(message) if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderProgress {
                        provider_id: provider_id.clone(),
                        message,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::RuntimeObserved {
                model_id,
                native_effort,
            } if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderRuntimeObserved {
                        provider_id: provider_id.clone(),
                        model_id,
                        native_effort,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::Usage(usage) if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderUsageUpdated {
                        provider_id: provider_id.clone(),
                        input_units: usage.input_units,
                        output_units: usage.output_units,
                        cache_read_units: usage.cache_read_units,
                        cache_write_units: usage.cache_write_units,
                        reasoning_output_units: usage.reasoning_output_units,
                        native_models: usage.native_models,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::NeedsUser {
                kind,
                summary,
                request_id,
            } if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                let request = AttentionRequest::new(
                    request_id.unwrap_or_else(|| self.context.next_attention_id()),
                    run.id(),
                    stage_id.clone(),
                    kind,
                    summary,
                    metadata.occurred_at(),
                )?;
                let request_id = request.id();
                events.push(run.request_attention(request, metadata)?);
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderNeedsUser {
                        provider_id: provider_id.clone(),
                        session_id,
                        attention_request_id: request_id,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::Paused if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                events.push(run.transition_stage(stage_id, StageTransition::Pause, metadata)?);
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderPaused {
                        provider_id: provider_id.clone(),
                        session_id,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::Interrupted if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                events.push(run.transition_stage(
                    stage_id,
                    StageTransition::Interrupt,
                    metadata,
                )?);
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderInterrupted {
                        provider_id: provider_id.clone(),
                        session_id,
                    },
                    metadata,
                )?);
            }
            // A stage can be interrupted before it ever starts: stopping a run
            // signals every active managed process, and one may belong to a
            // stage the domain still holds as Ready. That stage never ran, so
            // there is nothing to record against it — the domain refuses both
            // a stage interruption and a provider event here, and it is right
            // to. The interruption is already durable where it belongs, on the
            // provider session the adapter marked. Treating the signal as
            // nothing to record keeps the stop from failing and leaving the
            // run torn: process interrupted while the run still reads Running.
            ProviderSignal::Interrupted if stage_status == StageStatus::Ready => {}
            // The same window, reached by a launch that died rather than one
            // that was interrupted. Only observation may treat this as nothing
            // to record: a stop needs to finish, and the stage genuinely never
            // started. Execution keeps raising, because there a launch that
            // keeps dying is something the user has to be told about rather
            // than something to silently retry.
            ProviderSignal::Failed(_)
                if self.observe_only && stage_status == StageStatus::Ready => {}
            ProviderSignal::Resumed if stage_status == StageStatus::Running => {
                let session_id = session_id.ok_or_else(|| EngineError::ProviderProtocol {
                    stage_id: stage_id.clone(),
                    message: "resumed provider has no native session ID".to_owned(),
                })?;
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderResumed {
                        provider_id: provider_id.clone(),
                        session_id,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::Completed if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                events.push(run.transition_stage(stage_id, StageTransition::Complete, metadata)?);
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderCompleted {
                        provider_id: provider_id.clone(),
                        session_id,
                    },
                    metadata,
                )?);
            }
            ProviderSignal::Failed(reason) if stage_status == StageStatus::Running => {
                let metadata = self.metadata_for(run);
                events.push(run.transition_stage(stage_id, StageTransition::Fail, metadata)?);
                let metadata = self.metadata_for(run);
                events.push(run.record_provider_event(
                    stage_id,
                    DomainEventKind::ProviderFailed {
                        provider_id: provider_id.clone(),
                        session_id,
                        reason: Some(reason),
                    },
                    metadata,
                )?);
            }
            signal => {
                return Err(EngineError::ProviderProtocol {
                    stage_id: stage_id.clone(),
                    message: format!("signal {signal:?} is invalid from {stage_status:?}"),
                });
            }
        }
        Ok(events)
    }

    fn finish_run(
        &mut self,
        store: &mut SqliteStore,
        mut loaded: LoadedRun,
    ) -> Result<EngineStatus, EngineError> {
        let metadata = self.metadata_for(&loaded.run);
        let event = match loaded.run.transition(RunTransition::Complete, metadata) {
            Ok(event) => event,
            Err(RunTransitionError::CompletionBlocked(_)) => {
                let metadata = self.metadata_for(&loaded.run);
                loaded.run.transition(RunTransition::Fail, metadata)?
            }
            Err(error) => return Err(error.into()),
        };
        commit_execution(store, &loaded.run, loaded.revision, &[event])?;
        Ok(EngineStatus::Advanced {
            run_status: loaded.run.status(),
        })
    }

    fn metadata_for(&mut self, run: &Run) -> EventMetadata {
        self.context.next_event_metadata(*run.updated_at())
    }
}

fn load_execution_boundary(
    store: &mut SqliteStore,
    run_id: RunId,
) -> Result<(LoadedRun, RunWorkspace), EngineError> {
    let loaded = store.load_run(run_id)?;
    let workspace = store
        .load_workspace(run_id)?
        .ok_or(EngineError::MissingWorkspace(run_id))?;
    if workspace.status() != WorkspaceStatus::Ready {
        return Err(EngineError::WorkspaceNotReady {
            run_id,
            status: workspace.status(),
        });
    }
    if let Some(operation) = store.load_apply_operation(run_id)?.filter(|operation| {
        matches!(
            operation.status(),
            ApplyStatus::Prepared | ApplyStatus::AppliedToSource
        )
    }) {
        return Err(EngineError::ApplyInProgress {
            run_id,
            status: operation.status(),
        });
    }
    Ok((loaded, workspace))
}

/// Persists one provider emission.
///
/// A signal can be legitimately unrecordable: a stage interrupted before it
/// ever started has nothing to transition and nothing to record, and the store
/// refuses an empty batch. The output that produced the signal must still be
/// consumed, or it would be read again on every poll.
fn commit_emission(
    store: &mut SqliteStore,
    loaded: &LoadedRun,
    emitted: &[DomainEvent],
    commit: &crate::providers::ProviderCommit,
) -> Result<(), EngineError> {
    if emitted.is_empty() {
        store.commit_provider_checkpoint(commit)?;
    } else {
        store.commit_provider_execution_update(&loaded.run, loaded.revision, emitted, commit)?;
    }
    Ok(())
}

fn commit_execution(
    store: &mut SqliteStore,
    run: &Run,
    revision: RunRevision,
    events: &[DomainEvent],
) -> Result<(), EngineError> {
    store.commit_run_execution_update(run, revision, events)?;
    Ok(())
}

fn needs_user_status(run: &Run) -> EngineStatus {
    EngineStatus::NeedsUser {
        requests: run
            .attention_requests()
            .iter()
            .filter(|request| request.status() == &AttentionStatus::Pending)
            .map(AttentionRequest::id)
            .collect(),
    }
}

fn stages_with_status(run: &Run, status: StageStatus) -> Vec<StageId> {
    run.stages()
        .iter()
        .filter(|stage| stage.status() == status)
        .map(|stage| stage.id().clone())
        .collect()
}

#[derive(Default)]
struct ProviderCheckpoint {
    attempt: u32,
    signal_index: usize,
    provider_id: Option<ProviderId>,
    session_id: Option<ProviderSessionId>,
}

fn reduce_checkpoint(
    events: &[SequencedEvent],
    stage_id: &StageId,
) -> Result<ProviderCheckpoint, EngineError> {
    let mut checkpoint = ProviderCheckpoint {
        attempt: 1,
        ..ProviderCheckpoint::default()
    };
    for sequenced in events {
        let event = &sequenced.event;
        if event.stage_id() != Some(stage_id) {
            continue;
        }
        match event.kind() {
            DomainEventKind::StageRetryScheduled => {
                // A stage un-skipped by an upstream retry never ran; only a
                // stage that had a provider starts a new attempt.
                if checkpoint.provider_id.is_some() {
                    checkpoint.attempt = checkpoint
                        .attempt
                        .checked_add(1)
                        .ok_or(EngineError::CheckpointOverflow)?;
                }
                checkpoint.signal_index = 0;
                checkpoint.provider_id = None;
                checkpoint.session_id = None;
            }
            DomainEventKind::ProviderStarted {
                provider_id,
                session_id,
                ..
            } => {
                register_provider(&mut checkpoint, stage_id, provider_id)?;
                checkpoint.session_id.clone_from(session_id);
                advance_checkpoint(&mut checkpoint)?;
            }
            DomainEventKind::ProviderProgress { provider_id, .. }
            | DomainEventKind::ProviderResumed { provider_id, .. }
            | DomainEventKind::ProviderNeedsUser { provider_id, .. }
            | DomainEventKind::ProviderPaused { provider_id, .. }
            | DomainEventKind::ProviderInterrupted { provider_id, .. }
            | DomainEventKind::ProviderCompleted { provider_id, .. }
            | DomainEventKind::ProviderFailed { provider_id, .. }
            | DomainEventKind::ProviderUsageUpdated { provider_id, .. } => {
                register_provider(&mut checkpoint, stage_id, provider_id)?;
                advance_checkpoint(&mut checkpoint)?;
            }
            _ => {}
        }
    }
    Ok(checkpoint)
}

fn register_provider(
    checkpoint: &mut ProviderCheckpoint,
    stage_id: &StageId,
    provider_id: &ProviderId,
) -> Result<(), EngineError> {
    if let Some(previous) = checkpoint
        .provider_id
        .as_ref()
        .filter(|previous| *previous != provider_id)
    {
        return Err(EngineError::ProviderChanged {
            stage_id: stage_id.clone(),
            previous: previous.to_string(),
            current: provider_id.to_string(),
        });
    }
    checkpoint.provider_id = Some(provider_id.clone());
    Ok(())
}

fn advance_checkpoint(checkpoint: &mut ProviderCheckpoint) -> Result<(), EngineError> {
    checkpoint.signal_index = checkpoint
        .signal_index
        .checked_add(1)
        .ok_or(EngineError::CheckpointOverflow)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use chrono::Duration;
    use rusqlite::params;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        AttentionKind, ConfigSnapshotId, DomainEventKind, WorkflowDefinition, WorkflowKind,
    };
    use crate::engine::{FakeEvent, FakeProvider, FakeScenario, ProviderError, UsageDelta};
    use crate::store::ResolvedConfigSnapshot;
    use crate::workspace::WorkspaceManager;

    struct TestContext {
        next_event: u128,
        next_attention: u128,
    }

    impl TestContext {
        const fn new(base: u128) -> Self {
            Self {
                next_event: base,
                next_attention: base + 50_000,
            }
        }
    }

    impl ExecutionContext for TestContext {
        fn next_event_metadata(&mut self, not_before: DateTime<Utc>) -> EventMetadata {
            let id = EventId::from_u128(self.next_event);
            self.next_event += 1;
            EventMetadata::new(id, not_before + Duration::milliseconds(1))
        }

        fn next_attention_id(&mut self) -> AttentionRequestId {
            let id = AttentionRequestId::from_u128(self.next_attention);
            self.next_attention += 1;
            id
        }
    }

    struct Fixture {
        temp: TempDir,
        database: PathBuf,
        store: SqliteStore,
        run_id: RunId,
    }

    struct AliasedFakeProvider {
        id: ProviderId,
        inner: FakeProvider,
    }

    impl Provider for AliasedFakeProvider {
        fn provider_id_for(&self, _request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
            Ok(self.id.clone())
        }

        fn supports_role(&self, role: crate::domain::Role) -> bool {
            self.inner.supports_role(role)
        }

        fn poll(
            &mut self,
            store: &mut SqliteStore,
            request: &ProviderRequest,
        ) -> Result<ProviderPoll, ProviderError> {
            self.inner.poll(store, request)
        }
    }

    /// Wraps `FakeProvider` with a real `continue_instruction` root, unlike
    /// `FakeProvider` itself, which accepts the trait's no-op default. Tests
    /// that need to observe whether an instruction file was actually staged
    /// or cleaned up use this instead.
    struct InstrumentedProvider {
        root: PathBuf,
        inner: FakeProvider,
    }

    impl Provider for InstrumentedProvider {
        fn provider_id_for(&self, request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
            self.inner.provider_id_for(request)
        }

        fn supports_role(&self, role: crate::domain::Role) -> bool {
            self.inner.supports_role(role)
        }

        fn poll(
            &mut self,
            store: &mut SqliteStore,
            request: &ProviderRequest,
        ) -> Result<ProviderPoll, ProviderError> {
            self.inner.poll(store, request)
        }

        fn stage_continue_instruction(
            &mut self,
            _store: &mut SqliteStore,
            run_id: RunId,
            stage_id: &StageId,
            _role: crate::domain::Role,
            instruction: &str,
        ) -> Result<(), ProviderError> {
            crate::providers::continue_instruction::write_once(
                &self.root,
                run_id,
                stage_id,
                instruction,
            )
            .map_err(|error| ProviderError::new(error.to_string()))
        }

        fn discard_continue_instruction(
            &mut self,
            _store: &mut SqliteStore,
            run_id: RunId,
            stage_id: &StageId,
        ) -> Result<(), ProviderError> {
            crate::providers::continue_instruction::discard(&self.root, run_id, stage_id)
                .map_err(|error| ProviderError::new(error.to_string()))
        }
    }

    /// Simulates a concurrent second caller of `request_continue` racing the
    /// call under test: from inside the very hook the code under test uses
    /// to stage its own instruction, this also stages the identical
    /// instruction (a real concurrent caller issuing the same request would)
    /// and independently commits its own continue cycle before the outer
    /// call's commit runs — reproducing the exact "loser" interleaving
    /// without needing real threads.
    struct RacyContinueProvider {
        root: PathBuf,
        inner: FakeProvider,
    }

    impl Provider for RacyContinueProvider {
        fn provider_id_for(&self, request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
            self.inner.provider_id_for(request)
        }

        fn supports_role(&self, role: crate::domain::Role) -> bool {
            self.inner.supports_role(role)
        }

        fn poll(
            &mut self,
            store: &mut SqliteStore,
            request: &ProviderRequest,
        ) -> Result<ProviderPoll, ProviderError> {
            self.inner.poll(store, request)
        }

        fn stage_continue_instruction(
            &mut self,
            store: &mut SqliteStore,
            run_id: RunId,
            stage_id: &StageId,
            _role: crate::domain::Role,
            instruction: &str,
        ) -> Result<(), ProviderError> {
            crate::providers::continue_instruction::write_once(
                &self.root,
                run_id,
                stage_id,
                instruction,
            )
            .map_err(|error| ProviderError::new(error.to_string()))?;
            let winner = store
                .load_run(run_id)
                .map_err(|error| ProviderError::new(error.to_string()))?;
            let mut winning_run = winner.run;
            // Stamped from the run's own clock, not wall time: the engine's
            // test context advances one millisecond per event, so a fast
            // drive leaves the run's latest event ahead of real `now()` and
            // a wall-clock stamp would be rejected as preceding it.
            let metadata = EventMetadata::new(
                EventId::from_u128(9_000_000_000),
                *winning_run.updated_at() + Duration::milliseconds(1),
            );
            let event = winning_run
                .request_continue(metadata)
                .map_err(|error| ProviderError::new(error.to_string()))?;
            commit_execution(store, &winning_run, winner.revision, &[event])
                .map_err(|error| ProviderError::new(error.to_string()))?;
            Ok(())
        }

        fn discard_continue_instruction(
            &mut self,
            _store: &mut SqliteStore,
            run_id: RunId,
            stage_id: &StageId,
        ) -> Result<(), ProviderError> {
            crate::providers::continue_instruction::discard(&self.root, run_id, stage_id)
                .map_err(|error| ProviderError::new(error.to_string()))
        }
    }

    impl Fixture {
        fn new(kind: WorkflowKind, run_value: u128) -> Self {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source repo");
            init_repository(&source);
            let database = temp.path().join("senate.sqlite3");
            let worktrees = temp.path().join("worktrees");
            let mut store = SqliteStore::open(&database).unwrap();
            let run_id = RunId::from_u128(run_value);
            let created_at: DateTime<Utc> = std::time::SystemTime::now().into();
            let config_id = ConfigSnapshotId::new(format!("config-{run_value}")).unwrap();
            let workflow = WorkflowDefinition::built_in(kind);
            let run = Run::new(run_id, workflow, config_id.clone(), created_at);
            let config =
                ResolvedConfigSnapshot::new(config_id, 1, json!({"provider": "fake"}), created_at)
                    .unwrap();
            let created = run.created_event(EventMetadata::new(
                EventId::from_u128(run_value + 1),
                created_at,
            ));
            store.create_run(&run, &config, &[created]).unwrap();
            WorkspaceManager::new(&worktrees)
                .prepare_run_workspace(&mut store, run_id, &source)
                .unwrap();
            Self {
                temp,
                database,
                store,
                run_id,
            }
        }
    }

    #[test]
    fn deep_run_completes_and_rehydrates_exactly_after_reopen() {
        let mut fixture = Fixture::new(WorkflowKind::Deep, 100_000);
        let scenario = FakeScenario::new()
            .stage("research")
            .events([
                FakeEvent::Started,
                FakeEvent::progress("Inspecting repository"),
                FakeEvent::Completed,
            ])
            .stage("architecture")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("implementation")
            .events([
                FakeEvent::Started,
                FakeEvent::Usage(UsageDelta::stable(120, 45)),
                FakeEvent::Completed,
            ])
            .stage("simplification")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("quality_review")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("spec_review")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("verify")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("decision")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        let provider = FakeProvider::new(scenario).unwrap();
        let mut engine = WorkflowEngine::with_context(
            provider,
            "exercise deterministic workflow".to_owned(),
            TestContext::new(200_000),
        );

        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Finished {
                run_status: RunStatus::Completed
            }
        );
        let original = fixture.store.load_run(fixture.run_id).unwrap().run;
        assert!(
            original
                .stages()
                .iter()
                .all(|stage| stage.status() == StageStatus::Completed)
        );
        let events = fixture.store.load_events(fixture.run_id).unwrap();
        assert!(events.iter().any(|event| matches!(
            event.event.kind(),
            DomainEventKind::ProviderUsageUpdated {
                input_units: 120,
                output_units: 45,
                ..
            }
        )));

        drop(fixture.store);
        let mut reopened = SqliteStore::open(&fixture.database).unwrap();
        assert_eq!(reopened.load_run(fixture.run_id).unwrap().run, original);
    }

    /// Stopping a run signals every active managed process, and one can
    /// belong to a stage the domain still has as Ready — started by the
    /// adapter, not yet transitioned. The interruption must be recorded as
    /// provider evidence without inventing a stage transition the domain
    /// refuses, or the stop fails and leaves the run torn: the process
    /// interrupted while the run still reads Running.
    #[test]
    fn a_stage_interrupted_before_it_starts_never_fails_the_stop() {
        let mut fixture = Fixture::new(WorkflowKind::Fast, 700_000);
        let scenario = FakeScenario::new()
            .stage("implementation")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        let mut engine = WorkflowEngine::with_context(
            FakeProvider::new(scenario).unwrap(),
            "interrupted before start".to_owned(),
            TestContext::new(800_000),
        );

        // Bring the stage to Ready — prepared, with the domain not yet holding
        // it as Running — which is the state a stop can catch it in.
        let stage_id = crate::domain::StageId::new("implementation").unwrap();
        let loaded = fixture.store.load_run(fixture.run_id).unwrap();
        let mut run = loaded.run;
        let created_at: DateTime<Utc> = std::time::SystemTime::now().into();
        let started = run
            .transition(
                RunTransition::Start,
                EventMetadata::new(EventId::from_u128(810_000), created_at),
            )
            .unwrap();
        let ready = run
            .transition_stage(
                &stage_id,
                StageTransition::MarkReady,
                EventMetadata::new(EventId::from_u128(810_001), created_at),
            )
            .unwrap();
        let events = vec![started, ready];
        fixture
            .store
            .commit_run_update(&run, loaded.revision, &events)
            .unwrap();
        assert_eq!(run.stage(&stage_id).unwrap().status(), StageStatus::Ready);

        // The signal a stop produces for a process that was already launched.
        let produced = engine.consume_signal(
            &mut run,
            &stage_id,
            &ProviderId::new("fake").unwrap(),
            None,
            ProviderSignal::Interrupted,
        );
        let produced = produced.expect("an interruption before start must not fail the stop");
        assert!(
            produced.is_empty(),
            "a stage that never ran has nothing to record: {produced:?}"
        );
        assert_eq!(
            run.stage(&stage_id).unwrap().status(),
            StageStatus::Ready,
            "a stage that never started must not be given a stage transition"
        );
    }

    #[test]
    fn needs_user_checkpoint_survives_restart_and_continues_same_stage() {
        let mut fixture = Fixture::new(WorkflowKind::Fast, 300_000);
        let make_scenario = || {
            FakeScenario::new()
                .stage("implementation")
                .events([
                    FakeEvent::Started,
                    FakeEvent::needs_user(AttentionKind::Decision, "Choose API shape"),
                    FakeEvent::progress("Applying decision"),
                    FakeEvent::Completed,
                ])
                .stage("verify")
                .events([FakeEvent::Started, FakeEvent::Completed])
        };
        let mut engine = WorkflowEngine::with_context(
            FakeProvider::new(make_scenario()).unwrap(),
            "exercise deterministic workflow".to_owned(),
            TestContext::new(400_000),
        );
        let blocked = engine.drive(&mut fixture.store, fixture.run_id).unwrap();
        let request_id = match blocked {
            EngineStatus::NeedsUser { requests } => requests[0],
            other => panic!("expected attention, found {other:?}"),
        };
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::NeedsUser
        );

        drop(engine);
        drop(fixture.store);
        fixture.store = SqliteStore::open(&fixture.database).unwrap();
        let mut restarted = WorkflowEngine::with_context(
            FakeProvider::new(make_scenario()).unwrap(),
            "exercise deterministic workflow".to_owned(),
            TestContext::new(500_000),
        );
        restarted
            .resolve_attention(&mut fixture.store, fixture.run_id, request_id)
            .unwrap();
        assert_eq!(
            restarted.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Finished {
                run_status: RunStatus::Completed
            }
        );

        let events = fixture.store.load_events(fixture.run_id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event.kind(), DomainEventKind::NeedsUser { .. }))
                .count(),
            1
        );
        // The resolved implementation started exactly once; the verify
        // stage after it starts on its own.
        let implementation = StageId::new("implementation").unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event.stage_id() == Some(&implementation)
                    && matches!(event.event.kind(), DomainEventKind::ProviderStarted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn same_stage_route_change_is_rejected_before_poll_after_restart() {
        let mut fixture = Fixture::new(WorkflowKind::Fast, 550_000);
        let scenario = || {
            FakeScenario::new().stage("implementation").events([
                FakeEvent::Started,
                FakeEvent::delay("hold"),
                FakeEvent::Completed,
            ])
        };
        let mut original = WorkflowEngine::with_context(
            FakeProvider::new(scenario()).unwrap(),
            "provider continuity".to_owned(),
            TestContext::new(560_000),
        );
        assert!(matches!(
            original.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::WaitingForProvider { .. }
        ));
        drop(original);

        let changed = AliasedFakeProvider {
            id: ProviderId::new("claude").unwrap(),
            inner: FakeProvider::new(scenario()).unwrap(),
        };
        let mut restarted = WorkflowEngine::with_context(
            changed,
            "provider continuity".to_owned(),
            TestContext::new(570_000),
        );
        assert!(matches!(
            restarted.drive(&mut fixture.store, fixture.run_id),
            Err(EngineError::ProviderChanged { previous, current, .. })
                if previous == "fake" && current == "claude"
        ));
    }

    #[test]
    fn review_fan_out_becomes_ready_without_workflow_specific_scheduler_code() {
        let mut fixture = Fixture::new(WorkflowKind::Review, 600_000);
        let scenario = FakeScenario::new()
            .stage("research")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("quality_review")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("spec_review")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("synthesis")
            .events([FakeEvent::Started, FakeEvent::Completed])
            .stage("decision")
            .events([FakeEvent::Started, FakeEvent::Completed]);
        let mut engine = WorkflowEngine::with_context(
            FakeProvider::new(scenario).unwrap(),
            "exercise deterministic workflow".to_owned(),
            TestContext::new(700_000),
        );

        for _ in 0..5 {
            assert!(matches!(
                engine.tick(&mut fixture.store, fixture.run_id).unwrap(),
                EngineStatus::Advanced { .. }
            ));
        }
        let run = fixture.store.load_run(fixture.run_id).unwrap().run;
        assert_eq!(
            run.stage(&StageId::new("quality_review").unwrap())
                .unwrap()
                .status(),
            StageStatus::Ready
        );
        assert_eq!(
            run.stage(&StageId::new("spec_review").unwrap())
                .unwrap()
                .status(),
            StageStatus::Ready
        );
    }

    #[test]
    fn standard_and_deep_reviewers_fan_out_independently_and_decision_joins_both() {
        for (kind, run_value, context_value) in [
            (WorkflowKind::Standard, 710_000, 720_000),
            (WorkflowKind::Deep, 730_000, 740_000),
        ] {
            let mut fixture = Fixture::new(kind, run_value);
            let workflow = WorkflowDefinition::built_in(kind);
            let mut engine = WorkflowEngine::with_context(
                FakeProvider::new(FakeScenario::successful(&workflow)).unwrap(),
                "review fan-out".to_owned(),
                TestContext::new(context_value),
            );
            let quality = StageId::new("quality_review").unwrap();
            let spec = StageId::new("spec_review").unwrap();
            let decision = StageId::new("decision").unwrap();

            loop {
                assert!(matches!(
                    engine.tick(&mut fixture.store, fixture.run_id).unwrap(),
                    EngineStatus::Advanced { .. }
                ));
                let run = fixture.store.load_run(fixture.run_id).unwrap().run;
                if run.stage(&quality).unwrap().status() == StageStatus::Ready
                    && run.stage(&spec).unwrap().status() == StageStatus::Ready
                {
                    assert_eq!(run.stage(&decision).unwrap().status(), StageStatus::Pending);
                    break;
                }
            }

            for _ in 0..4 {
                engine.tick(&mut fixture.store, fixture.run_id).unwrap();
            }
            let run = fixture.store.load_run(fixture.run_id).unwrap().run;
            assert_eq!(
                run.stage(&quality).unwrap().status(),
                StageStatus::Completed
            );
            assert_eq!(run.stage(&spec).unwrap().status(), StageStatus::Ready);
            assert_eq!(run.stage(&decision).unwrap().status(), StageStatus::Pending);

            for _ in 0..4 {
                engine.tick(&mut fixture.store, fixture.run_id).unwrap();
            }
            let run = fixture.store.load_run(fixture.run_id).unwrap().run;
            assert_eq!(run.stage(&spec).unwrap().status(), StageStatus::Completed);
            assert_eq!(run.stage(&decision).unwrap().status(), StageStatus::Pending);

            // Verification became ready beside the reviews and runs after
            // them; the decision waits for it too.
            let verify = StageId::new("verify").unwrap();
            assert_eq!(run.stage(&verify).unwrap().status(), StageStatus::Ready);
            for _ in 0..4 {
                engine.tick(&mut fixture.store, fixture.run_id).unwrap();
            }
            let run = fixture.store.load_run(fixture.run_id).unwrap().run;
            assert_eq!(run.stage(&verify).unwrap().status(), StageStatus::Completed);
            assert_eq!(run.stage(&decision).unwrap().status(), StageStatus::Pending);

            engine.tick(&mut fixture.store, fixture.run_id).unwrap();
            let run = fixture.store.load_run(fixture.run_id).unwrap().run;
            assert_eq!(run.stage(&decision).unwrap().status(), StageStatus::Ready);

            assert_eq!(
                engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
                EngineStatus::Finished {
                    run_status: RunStatus::Completed
                }
            );
            let events = fixture.store.load_events(fixture.run_id).unwrap();
            let completion_sequence = |stage_id: &StageId| {
                events
                    .iter()
                    .find(|event| {
                        event.event.stage_id() == Some(stage_id)
                            && matches!(
                                event.event.kind(),
                                DomainEventKind::ProviderCompleted { .. }
                            )
                    })
                    .unwrap()
                    .sequence
            };
            let implementation = StageId::new("implementation").unwrap();
            let implementation_sequence = completion_sequence(&implementation);
            let quality_sequence = completion_sequence(&quality);
            let spec_sequence = completion_sequence(&spec);
            let decision_sequence = completion_sequence(&decision);
            assert!(implementation_sequence < quality_sequence);
            assert!(implementation_sequence < spec_sequence);
            assert!(decision_sequence > quality_sequence);
            assert!(decision_sequence > spec_sequence);
        }
    }

    #[test]
    fn optional_failure_degrades_join_but_review_run_completes() {
        for (failed_stage, run_value, context_value) in [
            ("quality_review", 800_000, 900_000),
            ("spec_review", 810_000, 910_000),
        ] {
            let mut fixture = Fixture::new(WorkflowKind::Review, run_value);
            let review_events = |stage: &str| {
                if stage == failed_stage {
                    vec![FakeEvent::Started, FakeEvent::failed("review unavailable")]
                } else {
                    vec![FakeEvent::Started, FakeEvent::Completed]
                }
            };
            let scenario = FakeScenario::new()
                .stage("research")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("quality_review")
                .events(review_events("quality_review"))
                .stage("spec_review")
                .events(review_events("spec_review"))
                .stage("synthesis")
                .events([FakeEvent::Started, FakeEvent::Completed])
                .stage("decision")
                .events([FakeEvent::Started, FakeEvent::Completed]);
            let mut engine = WorkflowEngine::with_context(
                FakeProvider::new(scenario).unwrap(),
                "exercise deterministic workflow".to_owned(),
                TestContext::new(context_value),
            );

            assert_eq!(
                engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
                EngineStatus::Finished {
                    run_status: RunStatus::Completed
                }
            );
            assert!(
                fixture
                    .store
                    .load_events(fixture.run_id)
                    .unwrap()
                    .iter()
                    .any(|event| matches!(
                        event.event.kind(),
                        DomainEventKind::StageReady { degraded: true }
                    ))
            );
        }
    }

    #[test]
    fn pause_interruption_and_delay_are_explicitly_controlled() {
        let mut fixture = Fixture::new(WorkflowKind::Fast, 1_000_000);
        let make_scenario = || {
            FakeScenario::new()
                .stage("implementation")
                .events([
                    FakeEvent::Started,
                    FakeEvent::Paused,
                    FakeEvent::progress("resumed"),
                    FakeEvent::Interrupted,
                    FakeEvent::delay("process-ready"),
                    FakeEvent::Completed,
                ])
                .stage("verify")
                .events([FakeEvent::Started, FakeEvent::Completed])
        };
        let mut engine = WorkflowEngine::with_context(
            FakeProvider::new(make_scenario()).unwrap(),
            "exercise deterministic workflow".to_owned(),
            TestContext::new(1_100_000),
        );
        let stage_id = StageId::new("implementation").unwrap();

        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Paused {
                stages: vec![stage_id.clone()]
            }
        );
        engine
            .resume_stage(&mut fixture.store, fixture.run_id, &stage_id)
            .unwrap();
        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Interrupted {
                stages: vec![stage_id.clone()]
            }
        );

        drop(engine);
        drop(fixture.store);
        fixture.store = SqliteStore::open(&fixture.database).unwrap();
        let mut engine = WorkflowEngine::with_context(
            FakeProvider::new(make_scenario()).unwrap(),
            "exercise deterministic workflow".to_owned(),
            TestContext::new(1_150_000),
        );
        engine
            .recover_stage(&mut fixture.store, fixture.run_id, &stage_id)
            .unwrap();
        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::WaitingForProvider {
                stage_id: stage_id.clone(),
                keep_attached: false,
            }
        );
        engine
            .provider_mut()
            .release("implementation", "process-ready")
            .unwrap();
        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Finished {
                run_status: RunStatus::Completed
            }
        );
    }

    #[test]
    fn failed_leaf_fails_run_and_execution_guards_workspace_and_apply() {
        let mut fixture = Fixture::new(WorkflowKind::Fast, 1_200_000);
        let scenario = FakeScenario::new()
            .stage("implementation")
            .events([FakeEvent::Started, FakeEvent::failed("compile failed")]);
        let mut engine = WorkflowEngine::with_context(
            FakeProvider::new(scenario).unwrap(),
            "exercise deterministic workflow".to_owned(),
            TestContext::new(1_300_000),
        );
        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Finished {
                run_status: RunStatus::Failed
            }
        );

        let missing_id = RunId::from_u128(1_400_000);
        let created_at: DateTime<Utc> = std::time::SystemTime::now().into();
        let config_id = ConfigSnapshotId::new("missing-workspace-config").unwrap();
        let run = Run::new(
            missing_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config_id.clone(),
            created_at,
        );
        let config = ResolvedConfigSnapshot::new(config_id, 1, json!({}), created_at).unwrap();
        let created = run.created_event(EventMetadata::new(
            EventId::from_u128(1_400_001),
            created_at,
        ));
        fixture.store.create_run(&run, &config, &[created]).unwrap();
        assert!(matches!(
            engine.tick(&mut fixture.store, missing_id),
            Err(EngineError::MissingWorkspace(id)) if id == missing_id
        ));

        let mut apply_fixture = Fixture::new(WorkflowKind::Fast, 1_500_000);
        let loaded = apply_fixture.store.load_run(apply_fixture.run_id).unwrap();
        let timestamp = loaded.run.updated_at().to_rfc3339();
        apply_fixture
            .store
            .connection
            .execute(
                "INSERT INTO run_apply_operations (
                    run_id, status, patch_hash, run_revision, last_error, revision,
                    created_at, updated_at
                 ) VALUES (?1, 'prepared', ?2, ?3, NULL, 0, ?4, ?4)",
                params![
                    apply_fixture.run_id.to_string(),
                    "0".repeat(64),
                    i64::try_from(loaded.revision.value()).unwrap(),
                    timestamp,
                ],
            )
            .unwrap();
        assert!(matches!(
            engine.tick(&mut apply_fixture.store, apply_fixture.run_id),
            Err(EngineError::ApplyInProgress {
                run_id,
                status: ApplyStatus::Prepared
            }) if run_id == apply_fixture.run_id
        ));
    }

    /// A refused continue must be side-effect-free: the instruction staged
    /// for the follow-up stage before the domain call cannot outlive a
    /// refusal, or a retry that edits the text would durably conflict
    /// against content no stage ever read. `request_continue` is called
    /// while the run is still `Ready` (not `Completed`), which the domain
    /// refuses with `RunFixError::RunNotCompleted` — exactly the "domain
    /// refuses" branch this behavior protects, entirely independent of the
    /// commit-time concurrency-race branch it also protects.
    #[test]
    fn a_refused_continue_leaves_no_instruction_file_and_a_retry_with_different_text_succeeds() {
        let mut fixture = Fixture::new(WorkflowKind::Standard, 1_600_000);
        let root = fixture.temp.path().join("continue-instructions-root");
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let scenario = FakeScenario::successful(&workflow);
        let mut engine = WorkflowEngine::with_context(
            InstrumentedProvider {
                root: root.clone(),
                inner: FakeProvider::new(scenario).unwrap(),
            },
            "exercise continue refusal cleanup".to_owned(),
            TestContext::new(1_700_000),
        );
        let follow_up_stage_id = StageId::new("followup_1").unwrap();

        // The run is freshly prepared (`Ready`), not `Completed`, so the
        // domain refuses this request after the instruction was already
        // staged for the stage it would have created.
        let refused = engine.request_continue(&mut fixture.store, fixture.run_id, "first text");
        assert!(matches!(
            refused,
            Err(EngineError::Fix(
                crate::domain::RunFixError::RunNotCompleted(RunStatus::Ready)
            ))
        ));
        assert_eq!(
            crate::providers::continue_instruction::read(
                &root,
                fixture.run_id,
                &follow_up_stage_id,
            )
            .unwrap(),
            None,
            "a refused continue must leave no staged instruction behind"
        );

        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Finished {
                run_status: RunStatus::Completed
            }
        );

        // Retried with different text than the refused attempt used, which
        // would fail with `ContinueInstructionError::Conflict` if the first
        // attempt's file had survived.
        engine
            .request_continue(&mut fixture.store, fixture.run_id, "second text")
            .unwrap();
        assert_eq!(
            crate::providers::continue_instruction::read(
                &root,
                fixture.run_id,
                &follow_up_stage_id,
            )
            .unwrap()
            .as_deref(),
            Some("second text")
        );
    }

    /// The concurrency-race branch of the same safety property: two callers
    /// compute the identical deterministic follow-up stage identity from the
    /// same completed run and stage the identical instruction, but only one
    /// commit can win. The loser's own commit fails, yet by then the
    /// winner's stage already durably exists — the loser's cleanup must
    /// recognize that and leave the shared instruction file alone rather
    /// than deleting content the winner's stage is about to read.
    #[test]
    fn a_lost_commit_race_never_deletes_a_concurrent_winners_instruction() {
        let mut fixture = Fixture::new(WorkflowKind::Standard, 1_800_000);
        let root = fixture.temp.path().join("continue-instructions-root");
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let scenario = FakeScenario::successful(&workflow);
        let mut engine = WorkflowEngine::with_context(
            RacyContinueProvider {
                root: root.clone(),
                inner: FakeProvider::new(scenario).unwrap(),
            },
            "exercise a lost commit race".to_owned(),
            TestContext::new(1_900_000),
        );
        let follow_up_stage_id = StageId::new("followup_1").unwrap();

        assert_eq!(
            engine.drive(&mut fixture.store, fixture.run_id).unwrap(),
            EngineStatus::Finished {
                run_status: RunStatus::Completed
            }
        );

        let lost =
            engine.request_continue(&mut fixture.store, fixture.run_id, "shared instruction");

        assert!(
            matches!(lost, Err(EngineError::Store(_))),
            "the outer call's own commit must be the one that lost the race"
        );
        // The concurrent "winner" injected by RacyContinueProvider committed
        // the exact same predicted follow-up stage before the outer call's
        // own commit ran.
        let after = fixture.store.load_run(fixture.run_id).unwrap();
        assert!(after.run.stage(&follow_up_stage_id).is_some());
        assert_eq!(
            crate::providers::continue_instruction::read(
                &root,
                fixture.run_id,
                &follow_up_stage_id,
            )
            .unwrap()
            .as_deref(),
            Some("shared instruction"),
            "the loser's cleanup must not delete the winner's staged instruction"
        );
    }

    fn init_repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-b", "main"]);
        git(path, &["config", "user.name", "The Senate Test"]);
        git(path, &["config", "user.email", "senate@example.invalid"]);
        fs::write(path.join("README.md"), "fixture\n").unwrap();
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-m", "fixture"]);
    }

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
