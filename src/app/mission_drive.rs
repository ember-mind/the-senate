//! Restartable, bounded dispatch for mission work packages.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::{MissionId, MissionStatus, RunId, RunStatus, WorkPackageId, WorkPackageStatus};
use crate::git::{Git, GitRepository, source_is_clean};
use crate::store::{MissionDriveRecord, SqliteStore, StoreError};

use super::mission_query::MissionDetails;
use super::mission_service::{MissionService, now};
use super::provider_factory::ProviderFactory;
use super::{AppError, EffortRequest, ExecutionReport, ExecutionSelection, RunService};

const DRIVE_POLICY_SCHEMA_VERSION: u32 = 1;
const DEFAULT_MISSION_DRIVE_LIMIT: usize = 4;
const MAX_MISSION_DRIVE_LIMIT: usize = 16;
const DRIVE_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct MissionDriveLock(File);

impl Drop for MissionDriveLock {
    fn drop(&mut self) {
        // A concurrent fork can temporarily retain this open file before
        // exec closes it. Release the lock explicitly, even in that window.
        #[cfg(unix)]
        let _ = rustix::fs::flock(&self.0, rustix::fs::FlockOperation::Unlock);
    }
}

/// Per-invocation choices for the mission driver. Omitted policy fields reuse
/// the last persisted choice; `once` limits this invocation to one worker wave.
#[derive(Clone, Debug, Default)]
pub struct MissionDriveOptions {
    pub max_parallel: Option<usize>,
    pub selection: Option<ExecutionSelection>,
    pub effort: Option<EffortRequest>,
    pub once: bool,
}

/// Durable choices for future package runs. Existing Runs retain their own
/// immutable execution configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDrivePolicy {
    #[serde(default = "drive_policy_schema_version")]
    pub schema_version: u32,
    pub max_parallel: usize,
    pub selection: ExecutionSelection,
    pub effort: EffortRequest,
}

const fn drive_policy_schema_version() -> u32 {
    DRIVE_POLICY_SCHEMA_VERSION
}

/// The latest report per package plus the point where scheduling paused or
/// finished. Reports are bounded by the number of packages, even when the
/// driver polls provider-backed Runs for a long time.
#[derive(Clone, Debug)]
pub struct MissionDriveReport {
    pub details: MissionDetails,
    pub policy: MissionDrivePolicy,
    pub reports: Vec<ExecutionReport>,
    pub reason: String,
}

impl MissionService {
    /// Resumes eligible bound Runs and starts Ready packages up to the stored
    /// concurrency limit. A local advisory lock guarantees that one process
    /// owns dispatch for this mission at a time; dropping it on process exit
    /// makes an interrupted drive immediately recoverable after restart.
    ///
    /// # Errors
    /// Returns lock contention, invalid policy, repository, provider, store,
    /// workspace, or execution errors.
    #[allow(
        clippy::too_many_lines,
        reason = "one ordered dispatch loop with durable pause guards"
    )]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "owned command options match the application API"
    )]
    pub fn drive_mission<F: ProviderFactory + Sync>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        options: MissionDriveOptions,
    ) -> Result<MissionDriveReport, AppError> {
        let _lock = self.acquire_drive_lock(mission_id)?;
        self.inspect_mission(mission_id)?;
        let policy = self.load_or_update_policy(mission_id, &options)?;
        let mut reports = Vec::new();
        let mut active_cursor = 0usize;

        loop {
            let details = self.inspect_mission(mission_id)?;
            if matches!(
                details.status,
                MissionStatus::Completed | MissionStatus::Cancelled
            ) {
                self.clear_pause(mission_id)?;
                return Ok(report(details, policy, reports, "mission is closed"));
            }

            if let Some(reason) = attention_reason(&details) {
                self.persist_pause(mission_id, &reason)?;
                return Ok(report(details, policy, reports, reason));
            }

            let repository = GitRepository::discover(&details.repository)?;
            if !source_is_clean(&Git::default(), &repository)? {
                let reason = format!(
                    "source checkout {} has uncommitted changes; commit integrated changes or stash unrelated work before resuming mission drive",
                    details.repository.display()
                );
                self.persist_pause(mission_id, &reason)?;
                return Ok(report(details, policy, reports, reason));
            }

            let active = active_packages(&details);
            let capacity = policy.max_parallel.saturating_sub(active.len());
            let ready: Vec<WorkPackageId> = details
                .packages
                .iter()
                .filter(|package| package.status == WorkPackageStatus::Ready)
                .take(capacity)
                .map(|package| package.id.clone())
                .collect();

            // RunService constructs each provider on the worker thread, so
            // the factory itself only needs shared-reference safety. Active
            // Runs count against capacity even when a native provider has
            // detached and the previous command exited.
            let resume_limit = active.len().min(policy.max_parallel);
            let mut resumable: Vec<(WorkPackageId, RunId)> = active
                .into_iter()
                .filter_map(|(package_id, run_id, status)| {
                    matches!(
                        status,
                        Some(
                            RunStatus::Created
                                | RunStatus::Preparing
                                | RunStatus::Ready
                                | RunStatus::Running
                                | RunStatus::Interrupted
                        )
                    )
                    .then_some((package_id, run_id))
                })
                .collect();
            if !resumable.is_empty() {
                let rotate = active_cursor % resumable.len();
                resumable.rotate_left(rotate);
                active_cursor = (active_cursor + resume_limit) % resumable.len();
            }
            resumable.truncate(resume_limit);

            if resumable.is_empty() && ready.is_empty() {
                self.clear_pause(mission_id)?;
                let reason = if has_active_packages(&details) {
                    "active Runs have no automatically resumable state"
                } else {
                    "no Ready packages; waiting for mission changes"
                };
                return Ok(report(details, policy, reports, reason));
            }

            // Existing active Runs occupy their slots; dispatch only into
            // capacity not already consumed by persisted package bindings.
            let jobs = build_jobs(resumable, ready, policy.max_parallel);
            let (wave_reports, dirty, capacity_race, fatal) =
                self.run_wave(runs, mission_id, &policy, jobs);
            for (package_id, execution) in wave_reports {
                record_report(&mut reports, package_id, execution);
            }
            if let Some(error) = fatal {
                return Err(error);
            }

            let latest = self.inspect_mission(mission_id)?;
            if dirty
                || !source_is_clean(
                    &Git::default(),
                    &GitRepository::discover(&latest.repository)?,
                )?
            {
                let reason = format!(
                    "source checkout {} has uncommitted changes; commit integrated changes or stash unrelated work before resuming mission drive",
                    latest.repository.display()
                );
                self.persist_pause(mission_id, &reason)?;
                return Ok(report(latest, policy, reports, reason));
            }
            if let Some(reason) = attention_reason(&latest) {
                self.persist_pause(mission_id, &reason)?;
                return Ok(report(latest, policy, reports, reason));
            }
            self.clear_pause(mission_id)?;

            if options.once {
                let reason = if capacity_race {
                    "a concurrent package start filled the available capacity"
                } else {
                    "one drive wave finished"
                };
                return Ok(report(latest, policy, reports, reason));
            }
            if capacity_race || has_active_packages(&latest) {
                thread::sleep(DRIVE_POLL_INTERVAL);
            }
        }
    }

    fn load_or_update_policy(
        &self,
        mission_id: MissionId,
        options: &MissionDriveOptions,
    ) -> Result<MissionDrivePolicy, AppError> {
        if let Some(limit) = options.max_parallel {
            validate_limit(limit)?;
        }

        let mut store = SqliteStore::open(&self.database)?;
        let persisted = store.mission_drive(mission_id)?;
        let mut policy = match &persisted {
            Some(record) => decode_policy(record)?,
            None => MissionDrivePolicy {
                schema_version: DRIVE_POLICY_SCHEMA_VERSION,
                max_parallel: DEFAULT_MISSION_DRIVE_LIMIT,
                selection: ExecutionSelection::Recommended,
                effort: EffortRequest::ProfileDefault,
            },
        };

        if let Some(limit) = options.max_parallel {
            policy.max_parallel = limit;
        }
        if let Some(selection) = &options.selection {
            policy.selection = selection.clone();
        }
        if let Some(effort) = &options.effort {
            policy.effort = effort.clone();
        }
        validate_limit(policy.max_parallel)?;

        let has_overrides = options.max_parallel.is_some()
            || options.selection.is_some()
            || options.effort.is_some();
        if persisted.is_none() || has_overrides {
            let json = serde_json::to_string(&policy).map_err(StoreError::from)?;
            store.save_mission_drive(mission_id, policy.max_parallel, &json, &now())?;
        }
        Ok(policy)
    }

    fn persist_pause(&self, mission_id: MissionId, reason: &str) -> Result<(), AppError> {
        SqliteStore::open(&self.database)?.set_mission_drive_pause(
            mission_id,
            Some(reason),
            &now(),
        )?;
        Ok(())
    }

    fn clear_pause(&self, mission_id: MissionId) -> Result<(), AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        if store
            .mission_drive(mission_id)?
            .is_some_and(|record| record.pause_reason.is_some())
        {
            store.set_mission_drive_pause(mission_id, None, &now())?;
        }
        Ok(())
    }

    fn acquire_drive_lock(&self, mission_id: MissionId) -> Result<MissionDriveLock, AppError> {
        let parent = self
            .database
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|error| AppError::MissionDriveUnavailable(error.to_string()))?;
        let path = parent.join(format!("mission-drive-{mission_id}.lock"));
        acquire_lock_file(&path, mission_id).map(MissionDriveLock)
    }

    fn run_wave<F: ProviderFactory + Sync>(
        &self,
        runs: &RunService<F>,
        mission_id: MissionId,
        policy: &MissionDrivePolicy,
        jobs: Vec<DriveJob>,
    ) -> (
        Vec<(WorkPackageId, ExecutionReport)>,
        bool,
        bool,
        Option<AppError>,
    ) {
        let results = thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .into_iter()
                .map(|job| {
                    scope.spawn(move || match job {
                        DriveJob::Resume(package_id, run_id) => {
                            (package_id, runs.resume_run(run_id))
                        }
                        DriveJob::Start(package_id) => (
                            package_id.clone(),
                            self.start_package(
                                runs,
                                mission_id,
                                &package_id,
                                Some(policy.selection.clone()),
                                policy.effort.clone(),
                            )
                            .map(|(execution, _)| execution),
                        ),
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(thread::ScopedJoinHandle::join)
                .collect::<Vec<_>>()
        });

        let mut reports = Vec::new();
        let mut dirty = false;
        let mut capacity_race = false;
        let mut fatal = None;
        for joined in results {
            let Ok((package_id, result)) = joined else {
                fatal.get_or_insert_with(|| {
                    AppError::MissionDriveUnavailable("a mission drive worker panicked".to_owned())
                });
                continue;
            };
            match result {
                Ok(execution) => {
                    reports.push((package_id, execution));
                }
                Err(
                    AppError::MissionIntegrationNeedsCommit { .. }
                    | AppError::DirtySourceRepository,
                ) => {
                    dirty = true;
                }
                Err(AppError::Store(StoreError::MissionDriveLimit { .. })) => {
                    capacity_race = true;
                }
                Err(error) => {
                    fatal.get_or_insert(error);
                }
            }
        }
        (reports, dirty, capacity_race, fatal)
    }
}

#[derive(Clone)]
enum DriveJob {
    Resume(WorkPackageId, RunId),
    Start(WorkPackageId),
}

fn build_jobs(
    resumable: Vec<(WorkPackageId, RunId)>,
    ready: Vec<WorkPackageId>,
    max_parallel: usize,
) -> Vec<DriveJob> {
    let mut jobs: Vec<_> = resumable
        .into_iter()
        .map(|(package_id, run_id)| DriveJob::Resume(package_id, run_id))
        .collect();
    let remaining = max_parallel.saturating_sub(jobs.len());
    jobs.extend(ready.into_iter().take(remaining).map(DriveJob::Start));
    jobs
}

fn active_packages(details: &MissionDetails) -> Vec<(WorkPackageId, RunId, Option<RunStatus>)> {
    details
        .packages
        .iter()
        .filter(|package| package.status.has_active_run())
        .filter_map(|package| Some((package.id.clone(), package.current_run?, package.run_status)))
        .collect()
}

fn has_active_packages(details: &MissionDetails) -> bool {
    details
        .packages
        .iter()
        .any(|package| package.status.has_active_run())
}

fn attention_reason(details: &MissionDetails) -> Option<String> {
    if !details.attention.failed.is_empty() {
        return Some(format!(
            "mission has failed package(s) awaiting operator action: {}",
            details
                .attention
                .failed
                .iter()
                .map(|(id, _)| id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !details.attention.awaiting_integration.is_empty() {
        return Some(format!(
            "mission has delivered package(s) awaiting explicit integration: {}",
            details
                .attention
                .awaiting_integration
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    details
        .attention
        .blocked
        .iter()
        .find(|(package_id, _)| {
            details
                .package(package_id)
                .is_none_or(|package| package.run_status != Some(RunStatus::Interrupted))
        })
        .map(|(package_id, reason)| {
            format!("mission package {package_id} needs operator attention: {reason}")
        })
}

fn validate_limit(max_parallel: usize) -> Result<(), AppError> {
    if !(1..=MAX_MISSION_DRIVE_LIMIT).contains(&max_parallel) {
        return Err(AppError::InvalidMissionDriveLimit(max_parallel));
    }
    Ok(())
}

fn decode_policy(record: &MissionDriveRecord) -> Result<MissionDrivePolicy, AppError> {
    let policy: MissionDrivePolicy =
        serde_json::from_str(&record.policy_json).map_err(StoreError::from)?;
    if policy.schema_version != DRIVE_POLICY_SCHEMA_VERSION {
        return Err(AppError::MissionDriveUnavailable(format!(
            "stored policy schema version {} is unsupported",
            policy.schema_version
        )));
    }
    if policy.max_parallel != record.max_parallel {
        return Err(StoreError::SnapshotProjectionMismatch(
            "mission drive max_parallel differs from policy",
        )
        .into());
    }
    validate_limit(policy.max_parallel)?;
    Ok(policy)
}

fn record_report(
    reports: &mut Vec<(WorkPackageId, ExecutionReport)>,
    package_id: WorkPackageId,
    execution: ExecutionReport,
) {
    if let Some((_, previous)) = reports
        .iter_mut()
        .find(|(previous_package, _)| *previous_package == package_id)
    {
        *previous = execution;
    } else {
        reports.push((package_id, execution));
    }
}

fn report(
    details: MissionDetails,
    policy: MissionDrivePolicy,
    reports: Vec<(WorkPackageId, ExecutionReport)>,
    reason: impl Into<String>,
) -> MissionDriveReport {
    MissionDriveReport {
        details,
        policy,
        reports: reports.into_iter().map(|(_, report)| report).collect(),
        reason: reason.into(),
    }
}

#[cfg(unix)]
fn acquire_lock_file(path: &PathBuf, mission_id: MissionId) -> Result<File, AppError> {
    use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};

    use std::os::fd::OwnedFd;
    use std::os::unix::fs::PermissionsExt as _;

    let flags =
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let mode = Mode::RUSR | Mode::WUSR;
    let fd: OwnedFd = open(path, flags, mode)
        .map_err(|error| AppError::MissionDriveUnavailable(error.to_string()))?;
    let file = File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|error| AppError::MissionDriveUnavailable(error.to_string()))?;
    if !metadata.is_file() {
        return Err(AppError::MissionDriveUnavailable(
            "mission driver lock path is not a regular file".to_owned(),
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::MissionDriveUnavailable(error.to_string()))?;
    if let Err(error) = flock(&file, FlockOperation::NonBlockingLockExclusive) {
        let error = std::io::Error::from(error);
        return if error.kind() == std::io::ErrorKind::WouldBlock {
            Err(AppError::MissionDriveBusy(mission_id))
        } else {
            Err(AppError::MissionDriveUnavailable(error.to_string()))
        };
    }
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_lock_file(_path: &PathBuf, _mission_id: MissionId) -> Result<File, AppError> {
    Err(AppError::MissionDriveUnavailable(
        "mission drive requires local Unix advisory file locks".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{DateTime, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::app::{
        DevelopmentFakeProviderFactory, NewWorkPackage, ProviderFactory, UniformProvider,
    };
    use crate::domain::{ConfigSnapshotId, WorkflowDefinition, WorkflowKind};
    use crate::store::ResolvedConfigSnapshot;

    struct Fixture {
        temp: TempDir,
        repo: PathBuf,
        database: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let repo = temp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            git(&repo, &["init", "-q"]);
            git(&repo, &["config", "user.email", "test@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            fs::write(repo.join("README.md"), "baseline\n").unwrap();
            git(&repo, &["add", "README.md"]);
            git(&repo, &["commit", "-qm", "initial"]);
            Self {
                database: temp.path().join("data/senate.db"),
                repo,
                temp,
            }
        }

        fn missions(&self) -> MissionService {
            MissionService::new(
                self.database.clone(),
                self.temp.path().join("data/worktrees"),
            )
        }

        fn runs<F: ProviderFactory>(&self, provider_factory: F) -> RunService<F> {
            RunService::new(
                self.database.clone(),
                self.temp.path().join("data/worktrees"),
                provider_factory,
            )
        }

        fn add_mission(&self, packages: Vec<NewWorkPackage>) -> MissionDetails {
            self.missions()
                .create_mission_with_packages(
                    "Drive test",
                    "exercise the scheduler",
                    &self.repo,
                    packages,
                )
                .unwrap()
        }
    }

    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn package(id: &str) -> NewWorkPackage {
        NewWorkPackage {
            id: WorkPackageId::new(id).unwrap(),
            contract: crate::domain::WorkPackageContract {
                title: id.to_owned(),
                goal: format!("deliver {id}"),
                rationale: String::new(),
                scope: String::new(),
                acceptance_criteria: vec![],
                verification: String::new(),
                workflow: WorkflowKind::Fast,
            },
            dependencies: vec![],
        }
    }

    #[derive(Clone)]
    struct DelayedFakeFactory {
        inner: DevelopmentFakeProviderFactory,
        active_configurations: Arc<AtomicUsize>,
        max_configurations: Arc<AtomicUsize>,
    }

    impl DelayedFakeFactory {
        fn note_configuration(&self) {
            let active = self.active_configurations.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_configurations.fetch_max(active, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            self.active_configurations.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl ProviderFactory for DelayedFakeFactory {
        type Provider = crate::app::RoutedProvider;

        fn config_for_new_run(
            &self,
            selection: ExecutionSelection,
            effort: EffortRequest,
            workflow: &WorkflowDefinition,
            id: ConfigSnapshotId,
            created_at: DateTime<Utc>,
        ) -> Result<ResolvedConfigSnapshot, AppError> {
            self.note_configuration();
            self.inner
                .config_for_new_run(selection, effort, workflow, id, created_at)
        }

        fn config_for_new_run_with_image(
            &self,
            selection: ExecutionSelection,
            effort: EffortRequest,
            image: &crate::app::ImageGenerationPlan,
            workflow: &WorkflowDefinition,
            id: ConfigSnapshotId,
            created_at: DateTime<Utc>,
        ) -> Result<ResolvedConfigSnapshot, AppError> {
            self.note_configuration();
            self.inner
                .config_for_new_run_with_image(selection, effort, image, workflow, id, created_at)
        }

        fn for_run(
            &self,
            run_id: RunId,
            config: &ResolvedConfigSnapshot,
            workflow: &WorkflowDefinition,
            events: &[crate::store::SequencedEvent],
        ) -> Result<Self::Provider, AppError> {
            self.inner.for_run(run_id, config, workflow, events)
        }

        fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
            self.inner.require_provider(provider)
        }
    }

    fn delayed_factory(fixture: &Fixture) -> (DelayedFakeFactory, Arc<AtomicUsize>) {
        let maximum = Arc::new(AtomicUsize::new(0));
        (
            DelayedFakeFactory {
                inner: DevelopmentFakeProviderFactory::new(fixture.temp.path().join("runs")),
                active_configurations: Arc::new(AtomicUsize::new(0)),
                max_configurations: Arc::clone(&maximum),
            },
            maximum,
        )
    }

    fn fake_options(max_parallel: usize) -> MissionDriveOptions {
        MissionDriveOptions {
            max_parallel: Some(max_parallel),
            selection: Some(ExecutionSelection::Uniform(UniformProvider::Fake)),
            once: true,
            ..MissionDriveOptions::default()
        }
    }

    #[test]
    fn drive_runs_a_bounded_parallel_wave_and_reuses_policy_after_reopen() {
        let fixture = Fixture::new();
        let mission = fixture.add_mission(vec![package("a"), package("b")]);
        let (factory, maximum) = delayed_factory(&fixture);
        let first = fixture
            .missions()
            .drive_mission(&fixture.runs(factory), mission.id, fake_options(2))
            .unwrap();

        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        assert_eq!(first.reports.len(), 2);
        assert_eq!(first.policy.max_parallel, 2);
        assert_eq!(first.details.attention.awaiting_integration.len(), 2);

        // A fresh service/run factory models a new CLI process. The durable
        // policy is reused; delivered packages pause dispatch instead of
        // being integrated or started again.
        let restarted = fixture
            .missions()
            .drive_mission(
                &fixture.runs(DevelopmentFakeProviderFactory::new(
                    fixture.temp.path().join("runs"),
                )),
                mission.id,
                MissionDriveOptions {
                    once: true,
                    ..MissionDriveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(restarted.policy, first.policy);
        assert_eq!(restarted.reports.len(), 0);
        assert_eq!(restarted.details.attention.awaiting_integration.len(), 2);
        assert!(restarted.reason.contains("awaiting explicit integration"));
    }

    #[test]
    fn dirty_checkout_pauses_without_starting_and_commit_allows_restart() {
        let fixture = Fixture::new();
        let mission = fixture.add_mission(vec![package("a")]);
        fs::write(fixture.repo.join("operator.txt"), "operator change\n").unwrap();
        let first = fixture
            .missions()
            .drive_mission(
                &fixture.runs(DevelopmentFakeProviderFactory::new(
                    fixture.temp.path().join("runs"),
                )),
                mission.id,
                fake_options(2),
            )
            .unwrap();
        assert!(first.reason.contains("uncommitted changes"));
        assert_eq!(first.reports.len(), 0);
        assert_eq!(first.details.packages[0].status, WorkPackageStatus::Ready);
        assert!(
            SqliteStore::open(&fixture.database)
                .unwrap()
                .list_runs()
                .unwrap()
                .is_empty()
        );
        assert!(
            SqliteStore::open(&fixture.database)
                .unwrap()
                .mission_drive(mission.id)
                .unwrap()
                .unwrap()
                .pause_reason
                .is_some()
        );

        git(&fixture.repo, &["add", "operator.txt"]);
        git(&fixture.repo, &["commit", "-qm", "operator change"]);
        let restarted = fixture
            .missions()
            .drive_mission(
                &fixture.runs(DevelopmentFakeProviderFactory::new(
                    fixture.temp.path().join("runs"),
                )),
                mission.id,
                MissionDriveOptions {
                    once: true,
                    ..MissionDriveOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            restarted.details.packages[0].status,
            WorkPackageStatus::Delivered
        );
        assert_eq!(restarted.policy.max_parallel, 2);
        assert_eq!(restarted.reports.len(), 1);
    }

    #[test]
    fn one_driver_lock_excludes_a_second_call_and_releases_on_drop() {
        let fixture = Fixture::new();
        let mission = fixture.add_mission(vec![package("a")]);
        let missions = fixture.missions();
        let lock = missions.acquire_drive_lock(mission.id).unwrap();
        let error = missions
            .drive_mission(
                &fixture.runs(DevelopmentFakeProviderFactory::new(
                    fixture.temp.path().join("runs"),
                )),
                mission.id,
                fake_options(1),
            )
            .unwrap_err();
        assert!(matches!(error, AppError::MissionDriveBusy(id) if id == mission.id));
        drop(lock);

        let resumed = missions
            .drive_mission(
                &fixture.runs(DevelopmentFakeProviderFactory::new(
                    fixture.temp.path().join("runs"),
                )),
                mission.id,
                fake_options(1),
            )
            .unwrap();
        assert_eq!(resumed.reports.len(), 1);
    }

    #[test]
    fn invalid_concurrency_limit_is_refused_before_policy_persistence() {
        let fixture = Fixture::new();
        let mission = fixture.add_mission(vec![package("a")]);
        let result = fixture.missions().drive_mission(
            &fixture.runs(DevelopmentFakeProviderFactory::new(
                fixture.temp.path().join("runs"),
            )),
            mission.id,
            fake_options(17),
        );
        assert!(matches!(
            result,
            Err(AppError::InvalidMissionDriveLimit(17))
        ));
        assert!(
            SqliteStore::open(&fixture.database)
                .unwrap()
                .mission_drive(mission.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn interrupted_packages_may_recover_but_paused_and_needs_user_are_attention() {
        let fixture = Fixture::new();
        let mission = fixture.add_mission(vec![package("a")]);
        let missions = fixture.missions();
        let mut details = missions.inspect_mission(mission.id).unwrap();
        let package_id = details.packages[0].id.clone();
        details.packages[0].status = WorkPackageStatus::Blocked;
        details.attention.blocked = vec![(package_id.clone(), "stopped".to_owned())];

        details.packages[0].run_status = Some(RunStatus::Interrupted);
        assert_eq!(attention_reason(&details), None);
        details.packages[0].run_status = Some(RunStatus::Paused);
        assert!(attention_reason(&details).is_some());
        details.packages[0].run_status = Some(RunStatus::NeedsUser);
        assert!(attention_reason(&details).is_some());
    }
}
