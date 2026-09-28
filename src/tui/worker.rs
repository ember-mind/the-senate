use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::app::{
    ApplyOutcome, EffortRequest, ExecutionReport, ExecutionSelection, MissionDetails,
    MissionService, ProviderFactory, PurgeReceipt, RetryRoute, RunService, StartProgress,
};
use crate::domain::{AttentionRequestId, MissionId, RunId, StageId, WorkPackageId, WorkflowKind};
use crate::workspace::{PublishReceipt, RebaseReceipt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionKind {
    Start,
    Resume,
    Stop,
    Retry,
    Fix,
    Continue,
    ResolveAttention,
    Apply,
    Rebase,
    Publish,
    Discard,
    Purge,
    /// A mission package's child run being started.
    StartPackage,
    /// A delivered package being recorded as integrated.
    Integrate,
}

impl ActionKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Start => "starting run",
            Self::StartPackage => "starting package",
            Self::Integrate => "integrating package",
            Self::Resume => "resuming run",
            Self::Stop => "stopping run",
            Self::Retry => "retrying stage",
            Self::Fix => "fixing run",
            Self::Continue => "continuing run",
            Self::ResolveAttention => "resolving attention",
            Self::Apply => "applying changes",
            Self::Rebase => "rebasing workspace",
            Self::Publish => "publishing pull request",
            Self::Discard => "discarding run",
            Self::Purge => "deleting run",
        }
    }

    /// Whether this action puts an agent to work.
    ///
    /// The ones that do each hold a managed worktree, a terminal session and a
    /// provider process for as long as they run, so they are the actions worth
    /// counting against a ceiling. Stopping, applying and discarding touch
    /// only the store and the checkout, and cost nothing to have several of.
    pub(crate) const fn drives_a_provider(self) -> bool {
        match self {
            Self::Start
            | Self::StartPackage
            | Self::Resume
            | Self::Retry
            | Self::Fix
            | Self::Continue
            | Self::ResolveAttention => true,
            Self::Stop
            | Self::Apply
            | Self::Rebase
            | Self::Publish
            | Self::Discard
            | Self::Purge
            | Self::Integrate => false,
        }
    }
}

#[derive(Debug)]
pub(crate) enum WorkerCommand {
    StartRun {
        workflow: WorkflowKind,
        task: String,
        repository: PathBuf,
        selection: ExecutionSelection,
        effort: EffortRequest,
    },
    ResumeRun {
        run_id: RunId,
    },
    StopRun {
        run_id: RunId,
    },
    RetryStage {
        run_id: RunId,
        stage_id: StageId,
        /// Where to send the stage instead of its configured provider, when
        /// the operator chose one.
        route: Option<RetryRoute>,
    },
    RequestFix {
        run_id: RunId,
    },
    RequestContinue {
        run_id: RunId,
        instruction: String,
    },
    ResolveAttention {
        run_id: RunId,
        attention_id: AttentionRequestId,
        response: Option<String>,
    },
    ApplyRun {
        run_id: RunId,
    },
    /// Moves a completed run's change onto the checkout's current `HEAD`.
    RebaseRun {
        run_id: RunId,
    },
    PublishRun {
        run_id: RunId,
    },
    DiscardRun {
        run_id: RunId,
    },
    /// Deletes an archived run for good — worktree, files, rows.
    PurgeRun {
        run_id: RunId,
    },
    /// Starts a child run for a ready package; its task is the handoff the
    /// mission renders.
    StartPackage {
        mission_id: MissionId,
        package_id: WorkPackageId,
        selection: ExecutionSelection,
        effort: EffortRequest,
        /// Arm the run to approve grantable permission requests as soon as
        /// it exists, because its mission is armed.
        auto_approve: bool,
    },
    /// Records a delivered package as integrated, on its run's evidence.
    IntegratePackage {
        mission_id: MissionId,
        package_id: WorkPackageId,
    },
}

impl WorkerCommand {
    pub(crate) const fn kind(&self) -> ActionKind {
        match self {
            Self::StartRun { .. } => ActionKind::Start,
            Self::ResumeRun { .. } => ActionKind::Resume,
            Self::StopRun { .. } => ActionKind::Stop,
            Self::RetryStage { .. } => ActionKind::Retry,
            Self::RequestFix { .. } => ActionKind::Fix,
            Self::RequestContinue { .. } => ActionKind::Continue,
            Self::ResolveAttention { .. } => ActionKind::ResolveAttention,
            Self::ApplyRun { .. } => ActionKind::Apply,
            Self::RebaseRun { .. } => ActionKind::Rebase,
            Self::PublishRun { .. } => ActionKind::Publish,
            Self::DiscardRun { .. } => ActionKind::Discard,
            Self::PurgeRun { .. } => ActionKind::Purge,
            Self::StartPackage { .. } => ActionKind::StartPackage,
            Self::IntegratePackage { .. } => ActionKind::Integrate,
        }
    }

    pub(crate) const fn run_id(&self) -> Option<RunId> {
        match self {
            Self::StartRun { .. } | Self::StartPackage { .. } | Self::IntegratePackage { .. } => {
                None
            }
            Self::ResumeRun { run_id }
            | Self::StopRun { run_id }
            | Self::RetryStage { run_id, .. }
            | Self::RequestFix { run_id }
            | Self::RequestContinue { run_id, .. }
            | Self::ResolveAttention { run_id, .. }
            | Self::ApplyRun { run_id }
            | Self::RebaseRun { run_id }
            | Self::PublishRun { run_id }
            | Self::DiscardRun { run_id }
            | Self::PurgeRun { run_id } => Some(*run_id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkerSuccess {
    Execution(ExecutionReport),
    Applied(ApplyOutcome, ExecutionReport),
    Published(PublishReceipt, ExecutionReport),
    Rebased(RebaseReceipt, ExecutionReport),
    /// Apply was refused because the checkout moved past the run's base; a
    /// rebase is the answer, so the interface offers it instead of an error.
    ApplyNeedsRebase {
        run_id: RunId,
        reason: String,
    },
    /// A purge leaves no run to report on: the rows it would describe are
    /// the ones it deleted.
    Purged(PurgeReceipt),
    /// A package's run started; the mission it belongs to came back current.
    PackageStarted(ExecutionReport, Box<MissionDetails>),
    /// A package was recorded as integrated.
    Integrated(Box<MissionDetails>),
}

impl WorkerSuccess {
    /// The run this action left behind, when it left one.
    pub(crate) const fn report(&self) -> Option<&ExecutionReport> {
        match self {
            Self::Execution(report)
            | Self::Applied(_, report)
            | Self::Published(_, report)
            | Self::Rebased(_, report)
            | Self::PackageStarted(report, _) => Some(report),
            Self::Purged(_) | Self::Integrated(_) | Self::ApplyNeedsRebase { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerResult {
    /// The ticket [`Worker::send`] handed out for the command this answers.
    pub ticket: u64,
    pub action: ActionKind,
    pub run_id: Option<RunId>,
    pub result: Result<WorkerSuccess, String>,
}

/// Where one start has got to, tagged with the ticket of the command it
/// belongs to. A start has no run id until its run exists, so the ticket is
/// the only name the interface has for it until then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartUpdate {
    pub ticket: u64,
    pub progress: StartProgress,
}

/// Starts one command, given its ticket and somewhere to report its progress
/// and outcome.
type Start = Box<dyn Fn(u64, WorkerCommand, Sender<WorkerResult>, Sender<StartUpdate>)>;

pub(crate) struct Worker {
    /// Starts one command on a thread of its own. Boxed so `Worker` carries
    /// none of the service's provider-factory type parameter.
    start: Start,
    /// Held for the life of the interface so the result channel can never
    /// disconnect: every action reports into a receiver that still exists.
    results: Sender<WorkerResult>,
    receiver: Receiver<WorkerResult>,
    /// Same lifetime argument as `results`.
    progress: Sender<StartUpdate>,
    progress_receiver: Receiver<StartUpdate>,
    next_ticket: std::cell::Cell<u64>,
}

/// Guarantees the interface hears back about every command it started.
///
/// A thread that panicked would otherwise never report, and the action it was
/// running would stay in flight for the rest of the session — holding its run
/// against every later action and lighting the header for work nobody is
/// doing. Reporting from `Drop` closes that gap: the outcome is sent exactly
/// once, whether the action returned or unwound.
struct Report {
    ticket: u64,
    action: ActionKind,
    run_id: Option<RunId>,
    results: Sender<WorkerResult>,
    reported: bool,
}

impl Report {
    const fn new(
        ticket: u64,
        action: ActionKind,
        run_id: Option<RunId>,
        results: Sender<WorkerResult>,
    ) -> Self {
        Self {
            ticket,
            action,
            run_id,
            results,
            reported: false,
        }
    }

    fn settle(&mut self, result: Result<WorkerSuccess, String>) {
        self.reported = true;
        self.send(result);
    }

    fn send(&self, result: Result<WorkerSuccess, String>) {
        // A closed receiver means the interface is already gone, and there is
        // nobody left to tell.
        let _ = self.results.send(WorkerResult {
            ticket: self.ticket,
            action: self.action,
            run_id: self.run_id,
            result,
        });
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if !self.reported {
            self.send(Err("the action ended unexpectedly".to_owned()));
        }
    }
}

impl Worker {
    pub(crate) fn spawn<F>(service: RunService<F>, missions: MissionService) -> Self
    where
        F: ProviderFactory + Send + Sync + 'static,
        F::Provider: Send + 'static,
    {
        // Shared rather than owned by one thread: every action gets the same
        // service, and they no longer take turns holding it.
        let service = Arc::new(service);
        let missions = Arc::new(missions);
        let (results, receiver) = mpsc::channel::<WorkerResult>();
        let (progress, progress_receiver) = mpsc::channel::<StartUpdate>();
        let start: Start = Box::new(move |ticket, command, results, progress| {
            let service = Arc::clone(&service);
            let missions = Arc::clone(&missions);
            std::thread::spawn(move || {
                let mut report = Report::new(ticket, command.kind(), command.run_id(), results);
                let observe = |update| {
                    // A closed receiver means nobody is left to show it to.
                    let _ = progress.send(StartUpdate {
                        ticket,
                        progress: update,
                    });
                };
                let outcome = execute(&service, &missions, command, &observe)
                    .map_err(|error| error.to_string());
                report.settle(outcome);
            });
        });
        Self {
            start,
            results,
            receiver,
            progress,
            progress_receiver,
            next_ticket: std::cell::Cell::new(0),
        }
    }

    /// Sets one action going on its own thread.
    ///
    /// Actions run concurrently by construction. Which of them may overlap is
    /// the caller's judgement, not this one's: the worker starts what it is
    /// handed and nothing here queues behind anything else.
    ///
    /// Returns the ticket its progress and outcome will carry.
    pub(crate) fn send(&self, command: WorkerCommand) -> u64 {
        let ticket = self.next_ticket.get();
        self.next_ticket.set(ticket + 1);
        (self.start)(ticket, command, self.results.clone(), self.progress.clone());
        ticket
    }

    /// Takes the next action that has finished, if one has reported back.
    ///
    /// Never fails: the sender lives in this struct, so the channel cannot
    /// disconnect while the worker is alive, and the only other outcome is an
    /// empty queue.
    pub(crate) fn try_recv(&self) -> Option<WorkerResult> {
        self.receiver.try_recv().ok()
    }

    /// Takes the next progress report from a start, if one has arrived.
    pub(crate) fn try_recv_progress(&self) -> Option<StartUpdate> {
        self.progress_receiver.try_recv().ok()
    }
}

fn execute<F>(
    service: &RunService<F>,
    missions: &MissionService,
    command: WorkerCommand,
    observe: &dyn Fn(StartProgress),
) -> Result<WorkerSuccess, crate::app::AppError>
where
    F: ProviderFactory,
{
    match command {
        WorkerCommand::StartRun {
            workflow,
            task,
            repository,
            selection,
            effort,
        } => service
            .start_run_observed(
                workflow,
                task,
                repository,
                Some(selection),
                effort,
                &crate::app::ImageGenerationPlan::disabled(),
                observe,
            )
            .map(WorkerSuccess::Execution),
        WorkerCommand::ResumeRun { run_id } => {
            service.resume_run(run_id).map(WorkerSuccess::Execution)
        }
        WorkerCommand::StopRun { run_id } => service.stop_run(run_id).map(WorkerSuccess::Execution),
        WorkerCommand::RetryStage {
            run_id,
            stage_id,
            route,
        } => service
            .retry_stage(run_id, &stage_id, route)
            .map(WorkerSuccess::Execution),
        WorkerCommand::RequestFix { run_id } => {
            service.request_fix(run_id).map(WorkerSuccess::Execution)
        }
        WorkerCommand::RequestContinue {
            run_id,
            instruction,
        } => service
            .request_continue(run_id, instruction)
            .map(WorkerSuccess::Execution),
        WorkerCommand::ResolveAttention {
            run_id,
            attention_id,
            response,
        } => service
            .resolve_attention_with_response(run_id, attention_id, response.as_deref())
            .map(WorkerSuccess::Execution),
        WorkerCommand::ApplyRun { run_id } => match service.apply_run(run_id) {
            Ok((outcome, report)) => Ok(WorkerSuccess::Applied(outcome, report)),
            Err(crate::app::AppError::Workspace(
                crate::workspace::WorkspaceError::PatchCheckFailed {
                    reason,
                    rebasable: true,
                },
            )) => Ok(WorkerSuccess::ApplyNeedsRebase { run_id, reason }),
            Err(error) => Err(error),
        },
        WorkerCommand::RebaseRun { run_id } => service
            .rebase_run(run_id)
            .map(|(receipt, report)| WorkerSuccess::Rebased(receipt, report)),
        WorkerCommand::PublishRun { run_id } => service
            .publish_run(run_id)
            .map(|(receipt, report)| WorkerSuccess::Published(receipt, report)),
        WorkerCommand::DiscardRun { run_id } => {
            service.discard_run(run_id).map(WorkerSuccess::Execution)
        }
        WorkerCommand::PurgeRun { run_id } => service.purge_run(run_id).map(WorkerSuccess::Purged),
        WorkerCommand::StartPackage {
            mission_id,
            package_id,
            selection,
            effort,
            auto_approve,
        } => missions
            .start_package_with(
                service,
                mission_id,
                &package_id,
                Some(selection),
                effort,
                auto_approve,
                observe,
            )
            .map(|(report, details)| WorkerSuccess::PackageStarted(report, Box::new(details))),
        WorkerCommand::IntegratePackage {
            mission_id,
            package_id,
        } => missions
            .integrate_package(service, mission_id, &package_id)
            .map(|details| WorkerSuccess::Integrated(Box::new(details))),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::*;
    use crate::app::{DevelopmentFakeProviderFactory, UniformProvider};
    use crate::domain::RunStatus;

    #[test]
    fn worker_executes_fake_start_off_caller_and_returns_committed_report() {
        let fixture = TempDir::new().unwrap();
        let repo = fixture.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("README.md"), "baseline\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-qm", "initial"]);
        let worker = Worker::spawn(
            RunService::new(
                fixture.path().join("senate.db"),
                fixture.path().join("worktrees"),
                DevelopmentFakeProviderFactory::new(fixture.path().join("runs")),
            ),
            MissionService::new(
                fixture.path().join("senate.db"),
                fixture.path().join("worktrees"),
            ),
        );
        worker.send(WorkerCommand::StartRun {
            workflow: WorkflowKind::Standard,
            task: "worker integration".to_owned(),
            repository: repo,
            selection: ExecutionSelection::Uniform(UniformProvider::Fake),
            effort: EffortRequest::ProfileDefault,
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Some(result) = worker.try_recv() {
                break result;
            }
            assert!(Instant::now() < deadline, "worker result timed out");
            std::thread::sleep(Duration::from_millis(10));
        };
        let success = result.result.unwrap();
        assert_eq!(result.action, ActionKind::Start);
        assert_eq!(
            success.report().unwrap().details.status,
            RunStatus::Completed
        );
    }

    #[test]
    fn worker_propagates_owned_action_error() {
        let fixture = TempDir::new().unwrap();
        let worker = Worker::spawn(
            RunService::new(
                fixture.path().join("senate.db"),
                fixture.path().join("worktrees"),
                DevelopmentFakeProviderFactory::new(fixture.path().join("runs")),
            ),
            MissionService::new(
                fixture.path().join("senate.db"),
                fixture.path().join("worktrees"),
            ),
        );
        worker.send(WorkerCommand::StartRun {
            workflow: WorkflowKind::Fast,
            task: "invalid repository".to_owned(),
            repository: fixture.path().join("missing"),
            selection: ExecutionSelection::Uniform(UniformProvider::Fake),
            effort: EffortRequest::ProfileDefault,
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = worker.try_recv() {
                assert!(
                    result
                        .result
                        .unwrap_err()
                        .contains("repository path is unavailable")
                );
                break;
            }
            assert!(Instant::now() < deadline, "worker error timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// An action whose thread dies without reporting would otherwise hold its
    /// run against every later action, with nothing left running to release
    /// it. The guard reports the failure on the dying thread's behalf.
    #[test]
    fn an_action_that_never_reports_still_settles() {
        let (results, receiver) = mpsc::channel();
        let run_id = Some(RunId::from_u128(3));
        drop(Report::new(0, ActionKind::Apply, run_id, results));

        let result = receiver.try_recv().expect("the guard reported for it");
        assert_eq!(result.action, ActionKind::Apply);
        assert_eq!(result.run_id, run_id, "settling the run it was holding");
        assert!(result.result.is_err(), "and says it did not succeed");
    }

    /// Two actions dispatched together both report: nothing queues behind
    /// anything else, and no result is lost to the other one running.
    #[test]
    fn two_actions_started_together_both_report_back() {
        let fixture = TempDir::new().unwrap();
        let worker = Worker::spawn(
            RunService::new(
                fixture.path().join("senate.db"),
                fixture.path().join("worktrees"),
                DevelopmentFakeProviderFactory::new(fixture.path().join("runs")),
            ),
            MissionService::new(
                fixture.path().join("senate.db"),
                fixture.path().join("worktrees"),
            ),
        );
        for task in ["first", "second"] {
            worker.send(WorkerCommand::StartRun {
                workflow: WorkflowKind::Fast,
                task: task.to_owned(),
                repository: fixture.path().join("missing"),
                selection: ExecutionSelection::Uniform(UniformProvider::Fake),
                effort: EffortRequest::ProfileDefault,
            });
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reported = 0;
        while reported < 2 {
            if worker.try_recv().is_some() {
                reported += 1;
                continue;
            }
            assert!(Instant::now() < deadline, "only {reported} of 2 reported");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn git(path: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
