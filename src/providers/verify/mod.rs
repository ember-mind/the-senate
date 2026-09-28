//! The deterministic `verify` provider.
//!
//! Every other provider is a coding agent. This one runs the repository's
//! own verification commands inside the run's worktree, records every
//! command and exit code in a Markdown artifact, and completes the stage
//! only when every exit code is zero. No prompt is built and no model is
//! consulted; the verdict is the exit codes and nothing else.
//!
//! It is synchronous by design. One poll runs the whole sequence and
//! returns the terminal signal, which means a long test suite holds the
//! worker for its duration — the same thread that would otherwise be
//! polling a native CLI. The trade is accepted: a verification that could be
//! interrupted halfway would need its own process supervision, and the
//! commands it runs are the repository's, already written to be run to
//! completion.

mod artifact;
mod config;
pub(crate) mod runner;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::domain::{ProviderId, Role, StageStatus};
use crate::engine::{Provider, ProviderError, ProviderPoll, ProviderRequest, ProviderSignal};
use crate::store::{SqliteStore, StoreError, process_root};

use artifact::Verdict;
use runner::CommandReport;

pub use config::{CONFIG_FILE, DEFAULT_TIMEOUT};

/// Runs a workspace's verification commands and records the outcome.
pub struct VerifyProvider {
    id: ProviderId,
    /// Where artifacts are written: `<root>/<run-id>/artifacts/`, the same
    /// tree the native adapters use, so the control room finds them without
    /// knowing which provider wrote them.
    artifact_root: PathBuf,
}

impl VerifyProvider {
    /// A provider writing artifacts under the configured data directory.
    ///
    /// # Errors
    /// Returns the data-directory resolution failure.
    pub fn from_environment() -> Result<Self, VerifyError> {
        Ok(Self::new(process_root()?))
    }

    /// A provider writing artifacts under an explicit root; evaluations and
    /// tests use this to keep their artifacts out of the user's data.
    ///
    /// # Panics
    /// Only if the static provider identifier were ever invalid, which the
    /// routing tests pin.
    #[must_use]
    pub fn new(artifact_root: PathBuf) -> Self {
        Self {
            id: ProviderId::new(crate::app::VERIFY_PROVIDER_ID)
                .expect("static provider ID must be valid"),
            artifact_root,
        }
    }

    fn now() -> DateTime<Utc> {
        std::time::SystemTime::now().into()
    }

    /// Runs the whole pass for one request and records its artifact.
    fn verify(
        &self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
    ) -> Result<Verdict, VerifyError> {
        // Loaded once, before the commands run: the row carries both the
        // repository the worktree was cut from, which can hold the
        // configuration the worktree does not, and the base commit the
        // artifact is stamped with.
        let workspace = store.load_workspace(request.run_id())?;
        let source_repo = workspace
            .as_ref()
            .map(|workspace| workspace.source_repo_path().to_owned());
        let base_commit = workspace
            .as_ref()
            .map(|workspace| workspace.base_commit().to_owned());
        let (plan, reports, verdict) =
            match resolved_plan(request, source_repo.as_deref(), base_commit.as_deref()) {
                Ok(plan) => {
                    let reports = run_until_first_failure(&plan, request.workspace_path())?;
                    let verdict = artifact::verdict(&plan, &reports);
                    (Some(plan), reports, verdict)
                }
                // A configuration the stage cannot read is a finding about the
                // repository, so it is reported the way a failing command is:
                // in the artifact and as the stage's failure reason.
                Err(VerifyError::Config(message)) => (None, Vec::new(), Verdict::Failed(message)),
                Err(error) => return Err(error),
            };
        let repeated =
            Self::earlier_identical_failure(store, request, base_commit.as_deref(), &verdict)?;
        let content = artifact::render(plan.as_ref(), &reports, &verdict, repeated.as_deref());
        let record = artifact::persist(
            &self.artifact_root,
            request,
            &self.id,
            base_commit.as_deref(),
            &content,
            verdict.artifact_status(),
            Self::now(),
        )?;
        store.insert_artifact(&record)?;
        Ok(verdict)
    }

    /// An earlier verification of this run that failed the same way at the
    /// same base commit, if there is one.
    ///
    /// The question a failed verification cannot answer on its own is the
    /// only one that matters to the lead: *did this change break it?* A fix
    /// cycle re-verifies, so when `verify` and `verify_1` end on the same
    /// sentence at the same base commit, two different trees produced one
    /// failure — which is far more often the repository's state than
    /// anything either attempt did. Saying so costs a read of artifacts
    /// already on disk; proving it would cost running the suite again on an
    /// untouched checkout.
    ///
    /// This is a hint, and the artifact words it as one. Two attempts can
    /// genuinely fail the same way for the same reason the change caused.
    fn earlier_identical_failure(
        store: &mut SqliteStore,
        request: &ProviderRequest,
        base_commit: Option<&str>,
        verdict: &Verdict,
    ) -> Result<Option<String>, VerifyError> {
        let Verdict::Failed(_) = verdict else {
            return Ok(None);
        };
        let bottom_line = verdict.bottom_line();
        for existing in store.list_artifacts(request.run_id())? {
            let metadata = existing.metadata();
            if metadata.kind() != crate::domain::ArtifactKind::Verify
                || metadata.stage_id() == request.stage_id()
                || metadata.base_commit() != base_commit
            {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(existing.path()) else {
                continue;
            };
            if artifact::bottom_line_of(&content).is_some_and(|earlier| earlier == bottom_line) {
                return Ok(Some(metadata.stage_id().as_str().to_owned()));
            }
        }
        Ok(None)
    }

    /// The verdict already recorded for this exact attempt, if a previous
    /// poll wrote its artifact but the process died before the terminal
    /// signal was committed. Reporting it again is correct; running the
    /// commands again would write a second artifact the store refuses.
    ///
    /// Two places to look, because the write is not atomic with the row:
    /// the store row, and — when the crash landed between the file and
    /// the row — the deterministic artifact path itself. In the second
    /// case the row is inserted here for the existing file, so the stage
    /// cannot wedge on an `ArtifactConflict` between two runs of the same
    /// commands that produced different bytes.
    fn recorded_verdict(
        &self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
    ) -> Result<Option<String>, VerifyError> {
        let existing = store
            .list_artifacts(request.run_id())?
            .into_iter()
            .find(|artifact| {
                artifact.metadata().stage_id() == request.stage_id()
                    && artifact.attempt() == request.attempt()
            });
        if let Some(existing) = existing {
            let content = std::fs::read_to_string(existing.path())?;
            return Ok(artifact::bottom_line_of(&content));
        }
        let path = artifact::artifact_path(&self.artifact_root, request);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        let Some(bottom_line) = artifact::bottom_line_of(&String::from_utf8_lossy(&bytes)) else {
            // Not an artifact this module wrote; leave it to `write_once`
            // to refuse and surface the conflict.
            return Ok(None);
        };
        let base_commit = store
            .load_workspace(request.run_id())?
            .map(|workspace| workspace.base_commit().to_owned());
        let record = artifact::describe(
            path,
            &bytes,
            request,
            &self.id,
            base_commit.as_deref(),
            artifact::status_of_bottom_line(&bottom_line),
            Self::now(),
        )?;
        store.insert_artifact(&record)?;
        Ok(Some(bottom_line))
    }
}

/// The plan as it will actually run: read from the repository, then with the
/// base commit filled into any command that asked for it.
///
/// Resolution happens here rather than in the reader so the artifact records
/// the command that ran, with a real commit ID a reader can paste into their
/// own shell, rather than the template it came from.
fn resolved_plan(
    request: &ProviderRequest,
    source_repo: Option<&Path>,
    base_commit: Option<&str>,
) -> Result<config::VerifyPlan, VerifyError> {
    let mut plan = config::plan_for(request.workspace_path(), source_repo)?;
    plan.commands = config::resolve_placeholders(&plan.commands, base_commit)?;
    Ok(plan)
}

/// Runs the plan's commands in order and stops at the first that does not
/// exit zero. Later commands are not run: their result would say nothing
/// about the change that is not already said by the failure, and a
/// formatting failure should not cost a full test suite.
fn run_until_first_failure(
    plan: &config::VerifyPlan,
    worktree: &Path,
) -> Result<Vec<CommandReport>, VerifyError> {
    let mut reports = Vec::with_capacity(plan.commands.len());
    for command in &plan.commands {
        let report = runner::run(command, worktree, plan.timeout)?;
        let failed = !report.exit.succeeded();
        reports.push(report);
        if failed {
            break;
        }
    }
    Ok(reports)
}

impl Provider for VerifyProvider {
    fn provider_id_for(&self, _request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
        Ok(self.id.clone())
    }

    fn supports_role(&self, role: Role) -> bool {
        role == Role::Verifier
    }

    /// Two polls per attempt, driven by the durable signal cursor like the
    /// Fake provider: the first starts the stage, the second runs every
    /// command and ends it. Nothing is held between the two, so a process
    /// that dies in between simply runs the pass on the next poll.
    fn poll(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
    ) -> Result<ProviderPoll, ProviderError> {
        if request.observe_only() {
            // Nothing runs across polls, so there is never anything to
            // observe; and observation must not start the commands.
            return Ok(ProviderPoll::Pending);
        }
        match (request.signal_index(), request.stage_status()) {
            (0, StageStatus::Ready) => Ok(ProviderPoll::Signal(ProviderSignal::Started {
                model_id: None,
                session_id: None,
            })),
            (1, StageStatus::Running) => {
                if let Some(bottom_line) = self.recorded_verdict(store, request)? {
                    return Ok(ProviderPoll::Signal(signal_for(&bottom_line)));
                }
                let verdict = self.verify(store, request)?;
                Ok(ProviderPoll::Signal(match verdict {
                    Verdict::Passed { .. } | Verdict::NothingChecked => ProviderSignal::Completed,
                    Verdict::Failed(_) => ProviderSignal::Failed(verdict.bottom_line()),
                }))
            }
            (index, status) => Err(ProviderError::new(format!(
                "verify stage {} has no signal at cursor {index} while {status:?}",
                request.stage_id()
            ))),
        }
    }
}

/// The terminal signal a recorded bottom line stands for.
fn signal_for(bottom_line: &str) -> ProviderSignal {
    if bottom_line.starts_with("passed") || bottom_line.starts_with("nothing checked") {
        ProviderSignal::Completed
    } else {
        ProviderSignal::Failed(bottom_line.to_owned())
    }
}

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("verification filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The repository's own configuration could not be used; reported as a
    /// failed stage, never as an infrastructure error.
    #[error("{0}")]
    Config(String),
    #[error("verification artifact exceeds {0} bytes")]
    ArtifactTooLarge(usize),
    #[error("verification artifact already exists with different content: {0}")]
    ArtifactConflict(PathBuf),
    #[error("verification artifact record is invalid: {0}")]
    Artifact(String),
}

impl From<VerifyError> for ProviderError {
    fn from(error: VerifyError) -> Self {
        Self::new(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, EventId, EventMetadata, Run, RunId, RunTransition, StageId, StageKind,
        WorkflowDefinition, WorkflowKind,
    };
    use crate::store::ResolvedConfigSnapshot;
    use crate::workspace::{RunWorkspace, WorkspaceMode};

    fn run_id() -> RunId {
        RunId::from_u128(7)
    }

    struct Harness {
        temp: tempfile::TempDir,
        worktree: PathBuf,
        provider: VerifyProvider,
        store: SqliteStore,
    }

    impl Harness {
        /// A store holding one run with a verify stage — artifacts belong
        /// to a run — but no workspace, so the provider is exercised on the
        /// path where no base commit is known.
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let worktree = temp.path().join("worktree");
            std::fs::create_dir(&worktree).unwrap();
            let created_at: DateTime<Utc> = std::time::SystemTime::now().into();
            let config_id = ConfigSnapshotId::new("verify-test").unwrap();
            let run = Run::new(
                run_id(),
                WorkflowDefinition::built_in(WorkflowKind::Fast),
                config_id.clone(),
                created_at,
            );
            let config = ResolvedConfigSnapshot::new(
                config_id,
                2,
                json!({"schema_version":2,"profile":"uniform","profile_version":"uniform_v1","routes":{},"providers":{}}),
                created_at,
            )
            .unwrap();
            let created = run.created_event(EventMetadata::new(EventId::new(), created_at));
            let mut store = SqliteStore::open_in_memory().unwrap();
            store.create_run(&run, &config, &[created]).unwrap();
            Self {
                provider: VerifyProvider::new(temp.path().join("runs")),
                worktree,
                store,
                temp,
            }
        }

        fn config(&self, text: &str) {
            std::fs::write(self.worktree.join(CONFIG_FILE), text).unwrap();
        }

        /// Registers a workspace row whose source repository holds `text`,
        /// so what the test exercises is the provider's own lookup — the
        /// stored row reaching the config reader — and not just the reader.
        fn source_repo_config(&mut self, text: &str) {
            let source = self.temp.path().join("source-repo");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(source.join(CONFIG_FILE), text).unwrap();
            let now: DateTime<Utc> = std::time::SystemTime::now().into();
            let workspace = RunWorkspace::preparing(
                run_id(),
                source.clone(),
                source.join(".git"),
                "0".repeat(40),
                self.worktree.clone(),
                None,
                WorkspaceMode::Detached,
                now,
            )
            .unwrap();
            let loaded = self.store.load_run(run_id()).unwrap();
            let mut run = loaded.run;
            let event = run
                .transition(
                    RunTransition::BeginPreparation,
                    EventMetadata::new(EventId::new(), now),
                )
                .unwrap();
            self.store
                .begin_workspace_preparation(&workspace, &run, loaded.revision, &event)
                .unwrap();
        }

        fn request(&self, signal_index: usize, status: StageStatus) -> ProviderRequest {
            self.request_for("verify", signal_index, status)
        }

        fn request_for(
            &self,
            stage_id: &str,
            signal_index: usize,
            status: StageStatus,
        ) -> ProviderRequest {
            ProviderRequest::new(
                run_id(),
                StageId::new(stage_id).unwrap(),
                StageKind::Verify,
                status,
                Role::Verifier,
                "task".to_owned(),
                self.worktree.clone(),
                1,
                signal_index,
                None,
                Vec::new(),
            )
        }

        /// Drives both polls and returns the terminal signal.
        fn run(&mut self) -> ProviderSignal {
            self.run_stage("verify")
        }

        /// The same, under a named stage, so one harness can hold the
        /// `verify` / `verify_1` pair a fix cycle produces.
        fn run_stage(&mut self, stage_id: &str) -> ProviderSignal {
            let request = self.request_for(stage_id, 0, StageStatus::Ready);
            let started = self.provider.poll(&mut self.store, &request).unwrap();
            assert!(matches!(
                started,
                ProviderPoll::Signal(ProviderSignal::Started {
                    model_id: None,
                    session_id: None
                })
            ));
            let request = self.request_for(stage_id, 1, StageStatus::Running);
            match self.provider.poll(&mut self.store, &request).unwrap() {
                ProviderPoll::Signal(signal) => signal,
                other => panic!("expected a terminal signal, got {other:?}"),
            }
        }

        fn artifact(&self) -> String {
            let artifacts = self.store.list_artifacts(run_id()).unwrap();
            assert_eq!(artifacts.len(), 1, "exactly one artifact per attempt");
            let artifact = &artifacts[0];
            assert_eq!(
                artifact.metadata().kind(),
                crate::domain::ArtifactKind::Verify
            );
            assert_eq!(artifact.metadata().role(), Role::Verifier);
            assert_eq!(
                artifact.metadata().provider_id().map(ProviderId::as_str),
                Some("verify")
            );
            std::fs::read_to_string(artifact.path()).unwrap()
        }
    }

    #[test]
    fn passing_commands_complete_the_stage_with_a_passed_bottom_line() {
        let mut harness = Harness::new();
        harness.config("[verify]\ncommands = [\"true\", \"echo done\"]\n");

        assert_eq!(harness.run(), ProviderSignal::Completed);

        let artifact = harness.artifact();
        assert!(artifact.contains("## Bottom line\npassed — 2 commands\n"));
        assert!(artifact.contains("### $ true\nexit: 0\n"));
        assert!(artifact.contains("### $ echo done\nexit: 0\nstdout:\n```text\ndone\n```\n"));
        assert_eq!(
            harness.store.list_artifacts(run_id()).unwrap()[0]
                .metadata()
                .status(),
            crate::domain::ArtifactStatus::Complete
        );
    }

    #[test]
    fn the_first_failing_command_fails_the_stage_and_skips_the_rest() {
        let mut harness = Harness::new();
        harness.config("[verify]\ncommands = [\"true\", \"false\", \"echo never\"]\n");

        assert_eq!(
            harness.run(),
            ProviderSignal::Failed("failed — false exited 1".to_owned())
        );

        let artifact = harness.artifact();
        assert!(artifact.contains("## Bottom line\nfailed — false exited 1\n"));
        assert!(artifact.contains("### $ false\nexit: 1\n"));
        assert!(artifact.contains("### $ echo never\nskipped: not run after the first failure\n"));
        assert_eq!(
            harness.store.list_artifacts(run_id()).unwrap()[0]
                .metadata()
                .status(),
            crate::domain::ArtifactStatus::Failed
        );
    }

    #[test]
    fn a_command_past_the_configured_timeout_fails_the_stage_as_timed_out() {
        let mut harness = Harness::new();
        harness.config("[verify]\ncommands = [\"sleep 5\"]\ntimeout_seconds = 1\n");

        assert_eq!(
            harness.run(),
            ProviderSignal::Failed("failed — sleep 5 timed out after 1 s".to_owned())
        );
        assert!(harness.artifact().contains("exit: timed out after 1 s\n"));
    }

    #[test]
    fn a_source_repository_config_verifies_a_worktree_that_carries_none() {
        let mut harness = Harness::new();
        // Detection would answer `npm test` here, which for a monorepo is
        // the wrong suite and can be red for reasons no change caused.
        std::fs::write(harness.worktree.join("package.json"), "{}\n").unwrap();
        harness.source_repo_config("[verify]\ncommands = [\"echo scoped\"]\n");

        assert_eq!(harness.run(), ProviderSignal::Completed);

        let artifact = harness.artifact();
        assert!(artifact.contains("## Bottom line\npassed — 1 command\n"));
        assert!(artifact.contains("### $ echo scoped\nexit: 0\n"));
        // The artifact names the checkout, so a green stage stays readable
        // back to the file that configured it.
        assert!(
            artifact.contains("## Source\n`.senate.toml` `[verify]` table (source repository)\n")
        );
    }

    #[test]
    fn the_worktrees_own_config_still_wins_over_the_source_repository() {
        let mut harness = Harness::new();
        harness.source_repo_config("[verify]\ncommands = [\"echo from the source repo\"]\n");
        harness.config("[verify]\ncommands = [\"echo from the worktree\"]\n");

        assert_eq!(harness.run(), ProviderSignal::Completed);

        let artifact = harness.artifact();
        assert!(artifact.contains("### $ echo from the worktree\n"));
        assert!(!artifact.contains("from the source repo"));
        assert!(artifact.contains("## Source\n`.senate.toml` `[verify]` table (worktree)\n"));
    }

    #[test]
    fn a_cargo_project_without_configuration_verifies_with_cargo_test() {
        let harness = Harness::new();
        std::fs::write(harness.worktree.join("Cargo.toml"), "[package]\n").unwrap();

        let plan = config::plan_for(&harness.worktree, None).unwrap();

        assert_eq!(plan.commands, ["cargo test"]);
        assert_eq!(plan.source, config::CommandSource::Detected("Cargo.toml"));
        assert_eq!(plan.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn a_command_can_ask_for_the_base_commit_the_worktree_was_cut_from() {
        let mut harness = Harness::new();
        // `echo` is the only argv-only way to prove what the command received.
        harness.source_repo_config("[verify]\ncommands = [\"echo {base_commit}\"]\n");

        assert_eq!(harness.run(), ProviderSignal::Completed);

        let artifact = harness.artifact();
        let base_commit = "0".repeat(40);
        // Both the heading and the output carry the real commit, so the
        // reader can paste the line into their own shell.
        assert!(
            artifact.contains(&format!("### $ echo {base_commit}\n")),
            "{artifact}"
        );
        assert!(
            artifact.contains(&format!("```text\n{base_commit}\n```")),
            "{artifact}"
        );
        assert!(!artifact.contains("{base_commit}"), "{artifact}");
    }

    #[test]
    fn a_second_failure_identical_to_the_first_is_flagged_as_not_new() {
        let mut harness = Harness::new();
        harness.source_repo_config("[verify]\ncommands = [\"false\"]\n");

        assert_eq!(
            harness.run_stage("verify"),
            ProviderSignal::Failed("failed — false exited 1".to_owned())
        );
        assert_eq!(
            harness.run_stage("verify_1"),
            ProviderSignal::Failed("failed — false exited 1".to_owned())
        );

        let artifacts = harness.store.list_artifacts(run_id()).unwrap();
        let mut by_stage = artifacts.iter().map(|artifact| {
            (
                artifact.metadata().stage_id().as_str().to_owned(),
                std::fs::read_to_string(artifact.path()).unwrap(),
            )
        });
        let first = by_stage
            .clone()
            .find(|(stage, _)| stage == "verify")
            .expect("first verification")
            .1;
        let second = by_stage
            .find(|(stage, _)| stage == "verify_1")
            .expect("second verification")
            .1;

        // The first failure has nothing to compare against; the second says
        // the fix cycle did not introduce it.
        assert!(!first.contains("## Not the first time"), "{first}");
        assert!(second.contains("## Not the first time"), "{second}");
        assert!(
            second.contains("Stage `verify` ended on this same line"),
            "{second}"
        );
    }

    #[test]
    fn a_failure_unlike_the_earlier_one_is_left_to_speak_for_itself() {
        let mut harness = Harness::new();
        harness.source_repo_config("[verify]\ncommands = [\"false\"]\n");
        assert!(matches!(
            harness.run_stage("verify"),
            ProviderSignal::Failed(_)
        ));

        // A different command, so a different bottom line: nothing says this
        // one is old news, because it is not.
        harness.config("[verify]\ncommands = [\"ls /no-such-senate-path\"]\n");
        assert!(matches!(
            harness.run_stage("verify_1"),
            ProviderSignal::Failed(_)
        ));

        let second = harness
            .store
            .list_artifacts(run_id())
            .unwrap()
            .into_iter()
            .find(|artifact| artifact.metadata().stage_id().as_str() == "verify_1")
            .map(|artifact| std::fs::read_to_string(artifact.path()).unwrap())
            .expect("second verification");

        assert!(!second.contains("## Not the first time"), "{second}");
    }

    #[test]
    fn a_passing_stage_is_never_told_it_failed_before() {
        let mut harness = Harness::new();
        harness.source_repo_config("[verify]\ncommands = [\"false\"]\n");
        assert!(matches!(
            harness.run_stage("verify"),
            ProviderSignal::Failed(_)
        ));

        harness.config("[verify]\ncommands = [\"true\"]\n");
        assert_eq!(harness.run_stage("verify_1"), ProviderSignal::Completed);

        let second = harness
            .store
            .list_artifacts(run_id())
            .unwrap()
            .into_iter()
            .find(|artifact| artifact.metadata().stage_id().as_str() == "verify_1")
            .map(|artifact| std::fs::read_to_string(artifact.path()).unwrap())
            .expect("second verification");

        assert!(!second.contains("## Not the first time"), "{second}");
    }

    #[test]
    fn an_unconfigured_monorepo_completes_the_stage_instead_of_running_the_whole_suite() {
        let mut harness = Harness::new();
        std::fs::write(
            harness.worktree.join("package.json"),
            r#"{"workspaces":["packages/*"],"scripts":{"test":"run-s test-client test-server"}}"#,
        )
        .unwrap();

        // Completed, not Failed: an unconfigured repository must not be left
        // unable to apply. The artifact is where the reader learns why.
        assert_eq!(harness.run(), ProviderSignal::Completed);

        let artifact = harness.artifact();
        assert!(
            artifact
                .contains("## Bottom line\nnothing checked — no commands configured or detected\n")
        );
        assert!(artifact.contains("workspaces root"), "{artifact}");
        assert!(artifact.contains("Add a `[verify]` table"), "{artifact}");
        assert!(!artifact.contains("### $ npm test"), "{artifact}");
        assert_eq!(
            harness.store.list_artifacts(run_id()).unwrap()[0]
                .metadata()
                .status(),
            crate::domain::ArtifactStatus::Complete
        );
    }

    #[test]
    fn malformed_configuration_fails_the_stage_with_the_parse_error() {
        let mut harness = Harness::new();
        harness.config("[verify]\ncommands = \"not a list\"\n");

        let signal = harness.run();

        let ProviderSignal::Failed(reason) = signal else {
            panic!("expected failure, got {signal:?}");
        };
        assert!(reason.starts_with("failed — .senate.toml: "), "{reason}");
        let artifact = harness.artifact();
        assert!(artifact.contains(&reason));
        assert!(artifact.contains("## Source\n`.senate.toml` (could not be read)\n"));
    }

    #[test]
    fn an_empty_directory_completes_having_checked_nothing() {
        let mut harness = Harness::new();

        assert_eq!(harness.run(), ProviderSignal::Completed);

        let artifact = harness.artifact();
        assert!(
            artifact
                .contains("## Bottom line\nnothing checked — no commands configured or detected\n")
        );
        assert!(!artifact.contains("### $"));
    }

    #[test]
    fn a_repeated_terminal_poll_reports_the_recorded_verdict_without_rerunning() {
        let mut harness = Harness::new();
        let counter = harness.worktree.join("count");
        harness.config(&format!(
            "[verify]\ncommands = [\"touch {}\", \"false\"]\n",
            counter.display()
        ));
        assert!(matches!(harness.run(), ProviderSignal::Failed(_)));
        std::fs::remove_file(&counter).unwrap();

        let request = harness.request(1, StageStatus::Running);
        let again = harness.provider.poll(&mut harness.store, &request).unwrap();

        assert_eq!(
            again,
            ProviderPoll::Signal(ProviderSignal::Failed("failed — false exited 1".to_owned()))
        );
        assert!(!counter.exists(), "the commands did not run a second time");
    }

    /// The write is not atomic with the row: a crash between the two must
    /// leave a poll that reports what the file says, records it, and runs
    /// nothing — not one that re-runs the commands into a conflicting file.
    #[test]
    fn an_artifact_file_without_a_row_is_adopted_instead_of_rerun() {
        let mut harness = Harness::new();
        let marker = harness.worktree.join("ran");
        harness.config(&format!(
            "[verify]\ncommands = [\"touch {}\"]\n",
            marker.display()
        ));
        let request = harness.request(1, StageStatus::Running);
        let path = artifact::artifact_path(&harness.provider.artifact_root, &request);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# Verification\n\n## Bottom line\nfailed — cargo test exited 101\n\n## Source\n`.senate.toml` `[verify]` table\n",
        )
        .unwrap();
        assert!(harness.store.list_artifacts(run_id()).unwrap().is_empty());

        let poll = harness.provider.poll(&mut harness.store, &request).unwrap();

        assert_eq!(
            poll,
            ProviderPoll::Signal(ProviderSignal::Failed(
                "failed — cargo test exited 101".to_owned()
            ))
        );
        assert!(!marker.exists(), "the commands did not run");
        let artifacts = harness.store.list_artifacts(run_id()).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].path(), path);
        assert_eq!(
            artifacts[0].metadata().status(),
            crate::domain::ArtifactStatus::Failed
        );
    }

    #[test]
    fn an_observing_poll_never_runs_the_commands() {
        let mut harness = Harness::new();
        let marker = harness.worktree.join("ran");
        harness.config(&format!(
            "[verify]\ncommands = [\"touch {}\"]\n",
            marker.display()
        ));

        let request = harness.request(1, StageStatus::Running).observing();
        let poll = harness.provider.poll(&mut harness.store, &request).unwrap();

        assert_eq!(poll, ProviderPoll::Pending);
        assert!(!marker.exists());
    }

    #[test]
    fn the_provider_serves_only_the_verifier_role() {
        let harness = Harness::new();
        assert!(harness.provider.supports_role(Role::Verifier));
        assert!(!harness.provider.supports_role(Role::Implementer));
        assert_eq!(
            harness
                .provider
                .provider_id_for(&harness.request(0, StageStatus::Ready))
                .unwrap()
                .as_str(),
            "verify"
        );
    }
}
