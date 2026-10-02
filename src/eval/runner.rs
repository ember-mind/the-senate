use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::app::{
    AppError, ExecutionTarget, ProviderResolver, QuiescentState, ResourcePlan, RoutedProvider,
    RoutingPlan, RunService, UniformProvider,
};
use crate::domain::{ConfigSnapshotId, ModelId, ProviderId, RunId, StageId, WorkflowDefinition};
use crate::store::{ResolvedConfigSnapshot, SequencedEvent};

use super::case::{EvalCase, EvalScorer, ValidationCommand};
use super::result::{
    EVAL_RESULT_SCHEMA_VERSION, EvalProvider, EvalResultError, EvalResultV1, EvalStatus,
    EvalTarget, EvalUsage,
};
use super::scorer::{ScoreInput, ScoredOutcome, ScoringError, score};
use super::suite::{EvalSuite, EvalSuiteError};

const VALIDATION_OUTPUT_LIMIT: usize = 256 * 1024;
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(1800);
/// Upper bound on safe eval permission continuations per case. Each resume
/// grants at least one new exact in-worktree Edit; anything beyond this is a
/// runaway session, not legitimate work.
const MAX_EVAL_AUTO_CONTINUATIONS: u32 = 8;

#[derive(Clone, Debug)]
pub struct EvalRunOptions {
    pub target: EvalTarget,
    /// Requested effort for the candidate role; the Fake support roles have
    /// no dial. Recorded on every result so evidence at different levels is
    /// never mistaken for the same measurement.
    pub effort: crate::domain::EffortSetting,
    pub repeat: u32,
    pub allow_native_usage: bool,
    pub output: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct EvalRunSummary {
    pub output_directory: PathBuf,
    pub results: Vec<EvalResultV1>,
}

pub struct EvalRunner {
    options: EvalRunOptions,
}

impl EvalRunner {
    /// Creates a validated local evaluation runner.
    ///
    /// # Errors
    /// Rejects zero repetitions or invalid target identifiers.
    pub fn new(options: EvalRunOptions) -> Result<Self, EvalRunnerError> {
        if options.repeat == 0 {
            return Err(EvalRunnerError::InvalidRepetitionCount);
        }
        EvalTarget::new(
            options.target.provider,
            options.target.configured_model.clone(),
        )?;
        Ok(Self { options })
    }

    /// Executes every suite case in fresh isolated runtime data.
    ///
    /// # Errors
    /// Rejects missing native consent/provider readiness, unsafe output reuse, corrupt fixtures,
    /// result persistence failures, or infrastructure setup failures.
    pub fn run(
        &self,
        suite: &EvalSuite,
        mut progress: impl FnMut(&EvalCase, u32, u32),
    ) -> Result<EvalRunSummary, EvalRunnerError> {
        if self.options.target.provider.is_native() && !self.options.allow_native_usage {
            return Err(EvalRunnerError::NativeUsageConsentRequired(
                self.options.target.provider,
            ));
        }
        let preflight_cli_version = preflight(self.options.target.provider)?;
        let output_directory = self
            .options
            .output
            .clone()
            .unwrap_or(default_output_directory(&self.options.target)?);
        std::fs::create_dir_all(&output_directory)?;
        let total = u32::try_from(suite.cases().len())
            .map_err(|_| EvalRunnerError::SuiteTooLarge)?
            .saturating_mul(self.options.repeat);
        let mut ordinal = 0_u32;
        let mut results = Vec::new();
        for repetition in 1..=self.options.repeat {
            for case in suite.cases() {
                ordinal = ordinal.saturating_add(1);
                progress(case, ordinal, total);
                let result = self.run_case(
                    suite,
                    case,
                    repetition,
                    &output_directory,
                    preflight_cli_version.as_deref(),
                )?;
                results.push(result);
            }
        }
        Ok(EvalRunSummary {
            output_directory,
            results,
        })
    }

    fn run_case(
        &self,
        suite: &EvalSuite,
        case: &EvalCase,
        repetition: u32,
        output_root: &Path,
        preflight_cli_version: Option<&str>,
    ) -> Result<EvalResultV1, EvalRunnerError> {
        let repetition_root = output_root
            .join(case.id)
            .join(format!("rep-{repetition:03}"));
        if repetition_root.join("result.json").exists() {
            return Err(EvalRunnerError::ResultAlreadyExists(repetition_root));
        }
        std::fs::create_dir_all(&repetition_root)?;
        let started_at = now();
        let fixture_hash = EvalSuite::fixture_fingerprint(case);
        let mut evidence = CaseEvidence::default();
        let execution = self.execute_case(case, &repetition_root, &mut evidence);
        let finished_at = now();
        let (status, metrics, detail) = match execution {
            Ok(scored) => (
                if scored.passed {
                    EvalStatus::Passed
                } else {
                    EvalStatus::Failed
                },
                Some(scored.metrics),
                scored.detail,
            ),
            Err(CaseFailure::Benchmark(error)) => {
                (EvalStatus::Failed, None, Some(error.to_string()))
            }
            Err(CaseFailure::Infrastructure(error)) => {
                (EvalStatus::InfrastructureFailure, None, Some(error))
            }
        };
        write_evidence(&repetition_root, &evidence)?;
        let result = EvalResultV1 {
            requested_effort: Some(self.options.effort),
            schema_version: EVAL_RESULT_SCHEMA_VERSION,
            suite: suite.name().to_owned(),
            suite_version: suite.version().to_owned(),
            suite_fingerprint: suite.fingerprint().to_owned(),
            case_id: case.id.to_owned(),
            repetition,
            target: self.options.target.clone(),
            confirmed_model: evidence.confirmed_model,
            provider_cli_version: evidence
                .provider_cli_version
                .or_else(|| preflight_cli_version.map(ToOwned::to_owned)),
            role: case.target_role,
            status,
            metrics,
            usage: evidence.usage,
            injected_prompt_bytes: evidence.injected_prompt_bytes,
            latency_ms: evidence
                .latency_ms
                .unwrap_or_else(|| duration_ms(started_at, finished_at)),
            artifact_hash: evidence.artifact.as_deref().map(sha256),
            diff_hash: sha256(evidence.diff.as_bytes()),
            fixture_hash,
            synthetic: self.options.target.provider == EvalProvider::Fake,
            started_at,
            finished_at,
            detail,
        };
        result.write(&repetition_root.join("result.json"))?;
        Ok(result)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one transaction-like orchestration path keeps evidence capture ordered around apply"
    )]
    fn execute_case(
        &self,
        case: &EvalCase,
        repetition_root: &Path,
        evidence: &mut CaseEvidence,
    ) -> Result<ScoredOutcome, CaseFailure> {
        let source = repetition_root.join("source");
        materialize_fixture(case, &source).map_err(infrastructure)?;
        initialize_repository(&source).map_err(infrastructure)?;
        let runtime = repetition_root.join("runtime");
        // Verification is not part of what an evaluation measures; see
        // `WorkflowDefinition::without_verification`.
        let workflow = WorkflowDefinition::built_in(case.workflow)
            .without_verification()
            .map_err(|error| infrastructure(error.to_string()))?;
        let target_stage = target_stage(&workflow, case).map_err(infrastructure)?;
        let provider_id = ProviderId::new(self.options.target.provider.as_str())
            .map_err(|error| infrastructure(error.to_string()))?;
        let model_id = self
            .options
            .target
            .configured_model
            .clone()
            .map(ModelId::new)
            .transpose()
            .map_err(|error| infrastructure(error.to_string()))?;
        let target = ExecutionTarget::new(provider_id, model_id);
        let config = crate::app::resolve_eval_config(
            case.target_role,
            &target,
            self.options.effort,
            &workflow,
            ConfigSnapshotId::new(format!("eval-config-{}", ulid::Ulid::new()))
                .map_err(|error| infrastructure(error.to_string()))?,
            now(),
        )
        .map_err(|error| infrastructure(error.to_string()))?;
        let resolver = EvalProviderResolver {
            process_root: runtime.join("runs"),
            runner_executable: std::env::current_exe().map_err(infrastructure)?,
        };
        let service = RunService::new(
            runtime.join("senate.db"),
            runtime.join("worktrees"),
            resolver,
        );
        let mut report = service
            .start_run_with_config(workflow.clone(), case.task, &source, &config)
            .map_err(|error| infrastructure(error.to_string()))?;
        let run_id = report.details.id;
        if self.options.target.provider == EvalProvider::Claude {
            let mut continuations = 0;
            while report.outcome == QuiescentState::NeedsUser {
                if continuations >= MAX_EVAL_AUTO_CONTINUATIONS {
                    return Err(CaseFailure::Infrastructure(format!(
                        "eval auto-continuation exceeded {MAX_EVAL_AUTO_CONTINUATIONS} resumes"
                    )));
                }
                continuations += 1;
                let Some(attention) = report.details.attention.first() else {
                    break;
                };
                let Some(next) = service
                    .auto_resolve_attention(run_id, attention.id)
                    .map_err(|error| infrastructure(error.to_string()))?
                else {
                    break;
                };
                report = next;
            }
        }
        if !matches!(report.outcome, QuiescentState::Completed) {
            return Err(CaseFailure::Infrastructure(format!(
                "run reached unexpected quiescent state {:?}",
                report.outcome
            )));
        }
        let stage_evidence = service
            .stage_execution_evidence(run_id, &target_stage)
            .map_err(|error| infrastructure(error.to_string()))?;
        if stage_evidence.configured_provider != self.options.target.provider.as_str()
            || stage_evidence.configured_model != self.options.target.configured_model
            || stage_evidence.actual_provider.as_deref()
                != Some(self.options.target.provider.as_str())
        {
            return Err(CaseFailure::Infrastructure(
                "candidate stage target identity mismatch".to_owned(),
            ));
        }
        evidence.confirmed_model = stage_evidence.confirmed_model;
        evidence.provider_cli_version = stage_evidence.provider_cli_version;
        evidence.usage = EvalUsage {
            input_units: stage_evidence.usage.input_units,
            output_units: stage_evidence.usage.output_units,
            cache_read_units: stage_evidence.usage.cache_read_units,
            cache_write_units: stage_evidence.usage.cache_write_units,
            reasoning_output_units: stage_evidence.usage.reasoning_output_units,
        };
        evidence.injected_prompt_bytes = stage_evidence.injected_prompt_bytes;
        // Provider execution latency (first ProviderStarted to terminal
        // provider event), not the whole-case wall clock fallback below.
        evidence.latency_ms = stage_evidence.latency_ms;
        let diff = service
            .preview_run_diff(run_id)
            .map_err(|error| infrastructure(error.to_string()))?;
        evidence.diff = diff.text;
        evidence.artifact = match service.read_artifact(run_id, &target_stage) {
            Ok(artifact) => Some(artifact.text),
            Err(AppError::ArtifactNotFound { .. })
                if self.options.target.provider == EvalProvider::Fake =>
            {
                None
            }
            Err(error) => return Err(infrastructure(error.to_string())),
        };
        if !matches!(case.scorer, EvalScorer::Implementer { .. })
            && !evidence.diff.trim().is_empty()
        {
            return Err(CaseFailure::Infrastructure(
                "read-only reviewer modified repository".to_owned(),
            ));
        }
        let validation_pass = match case.scorer {
            EvalScorer::Implementer {
                validation,
                require_plan_mismatch: false,
                ..
            } => {
                service
                    .apply_run(run_id)
                    .map_err(|error| infrastructure(error.to_string()))?;
                let validation = run_validation(&source, validation).map_err(infrastructure)?;
                evidence.validation = validation.output;
                Some(validation.passed)
            }
            _ => None,
        };
        score(
            case,
            ScoreInput {
                artifact: evidence.artifact.as_deref(),
                diff: &evidence.diff,
                validation_pass,
            },
        )
        .map_err(CaseFailure::Benchmark)
    }
}

struct EvalProviderResolver {
    process_root: PathBuf,
    runner_executable: PathBuf,
}

impl ProviderResolver for EvalProviderResolver {
    type Provider = RoutedProvider;

    fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
        Err(AppError::UnsupportedProvider(format!(
            "{}: evaluation runs never reroute a stage",
            provider.as_str()
        )))
    }

    fn resolve_for_run(
        &self,
        run_id: RunId,
        config: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
        _events: &[SequencedEvent],
    ) -> Result<Self::Provider, AppError> {
        let plan = RoutingPlan::from_snapshot(config, workflow).map_err(|error| {
            if config.schema_version() == 1 {
                AppError::LegacyExecutionConfig(run_id)
            } else {
                error.into()
            }
        })?;
        let resource_plan = ResourcePlan::from_snapshot(config, workflow)?;
        Ok(RoutedProvider::isolated(
            plan,
            resource_plan,
            workflow.clone(),
            self.process_root.clone(),
            self.runner_executable.clone(),
        ))
    }
}

#[derive(Default)]
struct CaseEvidence {
    artifact: Option<String>,
    diff: String,
    validation: String,
    usage: EvalUsage,
    injected_prompt_bytes: Option<u64>,
    latency_ms: Option<u64>,
    confirmed_model: Option<String>,
    provider_cli_version: Option<String>,
}

fn materialize_fixture(case: &EvalCase, destination: &Path) -> Result<(), EvalRunnerError> {
    if destination.exists() {
        return Err(EvalRunnerError::FixtureDestinationExists(
            destination.to_path_buf(),
        ));
    }
    std::fs::create_dir_all(destination)?;
    for file in case.fixture {
        let path = destination.join(file.path);
        let parent = path.parent().ok_or_else(|| {
            EvalRunnerError::FixtureCorrupt(format!("fixture path has no parent: {}", file.path))
        })?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(path, file.contents.as_bytes())?;
    }
    Ok(())
}

fn initialize_repository(path: &Path) -> Result<(), EvalRunnerError> {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "eval@senate.invalid"][..],
        &["config", "user.name", "The Senate Eval"][..],
        &["add", "-A"][..],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "eval baseline",
        ][..],
    ] {
        let argv = args
            .iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>();
        let output = crate::git::Git::default()
            .output(path, &argv, &[])
            .map_err(|error| EvalRunnerError::Git {
                args: args.join(" "),
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(EvalRunnerError::Git {
                args: args.join(" "),
                message: bounded_text(&output.stderr),
            });
        }
    }
    Ok(())
}

fn target_stage(
    workflow: &WorkflowDefinition,
    case: &EvalCase,
) -> Result<StageId, EvalRunnerError> {
    let stages = workflow
        .stages()
        .iter()
        .filter(|stage| stage.role() == case.target_role)
        .collect::<Vec<_>>();
    if stages.len() != 1 {
        return Err(EvalRunnerError::FixtureCorrupt(format!(
            "case {} target role occurs {} times",
            case.id,
            stages.len()
        )));
    }
    Ok(stages[0].id().clone())
}

struct ValidationOutcome {
    passed: bool,
    output: String,
}

fn run_validation(
    repository: &Path,
    commands: &[ValidationCommand],
) -> Result<ValidationOutcome, EvalRunnerError> {
    run_validation_with_timeout(repository, commands, VALIDATION_TIMEOUT)
}

fn run_validation_with_timeout(
    repository: &Path,
    commands: &[ValidationCommand],
    timeout: Duration,
) -> Result<ValidationOutcome, EvalRunnerError> {
    use crate::providers::verify::runner::{CommandExit, run_argv};
    let mut passed = true;
    let mut log = String::new();
    for command in commands {
        let description = format!("{} {}", command.program, command.args.join(" "));
        let report = run_argv(
            &description,
            command.program,
            command.args,
            repository,
            timeout,
        );
        if let CommandExit::CouldNotStart(error) | CommandExit::StatusUnavailable(error) =
            &report.exit
        {
            return Err(std::io::Error::other(error.clone()).into());
        }
        writeln!(
            log,
            "$ {} {}\nexit: {:?}\nstdout ({} earlier bytes omitted):\n{}\nstderr ({} earlier bytes omitted):\n{}",
            command.program,
            command.args.join(" "),
            report.exit,
            report.stdout.dropped,
            bounded_text(&report.stdout.bytes),
            report.stderr.dropped,
            bounded_text(&report.stderr.bytes)
        )
        .expect("String write cannot fail");
        passed &= report.exit.succeeded();
    }
    Ok(ValidationOutcome {
        passed,
        output: log,
    })
}

fn write_evidence(root: &Path, evidence: &CaseEvidence) -> Result<(), EvalRunnerError> {
    std::fs::write(
        root.join("artifact.md"),
        evidence
            .artifact
            .as_deref()
            .unwrap_or("<!-- candidate produced no artifact -->\n"),
    )?;
    std::fs::write(root.join("diff.patch"), &evidence.diff)?;
    std::fs::write(root.join("validation.txt"), &evidence.validation)?;
    Ok(())
}

fn preflight(provider: EvalProvider) -> Result<Option<String>, EvalRunnerError> {
    match provider {
        EvalProvider::Fake => Ok(Some("deterministic-fake-v1".to_owned())),
        EvalProvider::Claude => {
            let installation = crate::providers::claude::ClaudeInstallation::discover()
                .map_err(|error| EvalRunnerError::ProviderUnavailable(error.to_string()))?;
            if !installation.authenticated() {
                return Err(EvalRunnerError::ProviderUnavailable(
                    "Claude Code is not authenticated".to_owned(),
                ));
            }
            Ok(Some(installation.version().to_owned()))
        }
        EvalProvider::Codex => {
            let installation = crate::providers::codex::CodexInstallation::discover()
                .map_err(|error| EvalRunnerError::ProviderUnavailable(error.to_string()))?;
            if !installation.authenticated() {
                return Err(EvalRunnerError::ProviderUnavailable(
                    "Codex CLI is not authenticated".to_owned(),
                ));
            }
            Ok(Some(installation.version().to_owned()))
        }
    }
}

fn default_output_directory(target: &EvalTarget) -> Result<PathBuf, EvalRunnerError> {
    let base = if let Some(data) =
        std::env::var_os("SENATE_DATA_DIR").filter(|value| !value.is_empty())
    {
        PathBuf::from(data)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or(EvalRunnerError::OutputDirectoryUnavailable)?)
            .join(".senate")
    };
    let model = target
        .configured_model
        .as_deref()
        .map_or_else(|| "native-default".to_owned(), sanitize_component);
    Ok(base.join("evals").join(format!(
        "{}-{}-{}-{}",
        now().timestamp_millis(),
        target.provider,
        model,
        ulid::Ulid::new()
    )))
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn bounded_text(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(VALIDATION_OUTPUT_LIMIT);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn sha256(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    let mut hash = String::with_capacity(64);
    for byte in digest {
        write!(hash, "{byte:02x}").expect("String write cannot fail");
    }
    hash
}

fn duration_ms(start: DateTime<Utc>, finish: DateTime<Utc>) -> u64 {
    u64::try_from(
        finish
            .signed_duration_since(start)
            .num_milliseconds()
            .max(0),
    )
    .unwrap_or(u64::MAX)
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err adapters receive owned heterogeneous error values"
)]
fn infrastructure(error: impl ToString) -> CaseFailure {
    CaseFailure::Infrastructure(error.to_string())
}

enum CaseFailure {
    Benchmark(ScoringError),
    Infrastructure(String),
}

#[derive(Debug, Error)]
pub enum EvalRunnerError {
    #[error("repeat must be at least 1")]
    InvalidRepetitionCount,
    #[error("eval suite has too many cases")]
    SuiteTooLarge,
    #[error("native eval provider {0} may consume provider usage; rerun with --allow-native-usage")]
    NativeUsageConsentRequired(EvalProvider),
    #[error("eval provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("eval output directory cannot be resolved; set HOME or --out")]
    OutputDirectoryUnavailable,
    #[error("eval result already exists under {0}; choose a new --out path")]
    ResultAlreadyExists(PathBuf),
    #[error("fixture destination already exists: {0}")]
    FixtureDestinationExists(PathBuf),
    #[error("fixture is corrupt: {0}")]
    FixtureCorrupt(String),
    #[error("fixture git command `git {args}` failed: {message}")]
    Git { args: String, message: String },
    #[error(transparent)]
    Suite(#[from] EvalSuiteError),
    #[error(transparent)]
    Result(#[from] EvalResultError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::eval::case::ROLE_CORE_CASES_V3;
    use crate::eval::result::EvalMetrics;
    use crate::eval::scorer::{ScoreInput, ScoringError, score};

    #[cfg(unix)]
    #[test]
    fn validation_preserves_exact_argv_and_times_out_inherited_output_pipes() {
        let directory = tempdir().unwrap();
        let exact = run_validation_with_timeout(
            directory.path(),
            &[ValidationCommand {
                program: "sh",
                args: &["-c", "printf 'two words'"],
            }],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(exact.passed);
        assert!(exact.output.contains("two words"));
        let started = std::time::Instant::now();
        let stalled = run_validation_with_timeout(
            directory.path(),
            &[ValidationCommand {
                program: "sh",
                args: &["-c", "sleep 6 & exit 0"],
            }],
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!stalled.passed);
        assert!(stalled.output.contains("TimedOut"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    fn prepare(case: &EvalCase) -> (TempDir, PathBuf) {
        let directory = tempdir().unwrap();
        let repository = directory.path().join("fixture");
        materialize_fixture(case, &repository).unwrap();
        initialize_repository(&repository).unwrap();
        (directory, repository)
    }

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn v3_implementer_fixtures_stay_clean_through_validation_and_score_only_src_changes() {
        let cases = [
            (&ROLE_CORE_CASES_V3[0], "value + 2", "value * 2"),
            (
                &ROLE_CORE_CASES_V3[1],
                "input.to_owned()",
                "input.trim().to_owned()",
            ),
        ];
        let cargo_test = &[ValidationCommand {
            program: "cargo",
            args: &["test", "--quiet", "--offline"],
        }];
        for (case, before, after) in cases {
            let (_directory, repository) = prepare(case);
            let _baseline = run_validation(&repository, cargo_test).unwrap();
            let baseline_status = git(&repository, &["status", "--porcelain"]);
            assert!(
                baseline_status.is_empty(),
                "{} baseline diff: {baseline_status:?}",
                case.id
            );

            let source_path = repository.join("src/lib.rs");
            let source = std::fs::read_to_string(&source_path).unwrap();
            assert!(source.contains(before));
            std::fs::write(&source_path, source.replace(before, after)).unwrap();
            let candidate = run_validation(&repository, cargo_test).unwrap();
            assert!(
                candidate.passed,
                "candidate validation failed: {}",
                candidate.output
            );
            assert_eq!(git(&repository, &["diff", "--name-only"]), "src/lib.rs\n");

            let diff = git(&repository, &["diff"]);
            let scored = score(
                case,
                ScoreInput {
                    artifact: None,
                    diff: &diff,
                    validation_pass: Some(candidate.passed),
                },
            )
            .unwrap();
            assert!(
                scored.passed,
                "implementer score failed: {:?}",
                scored.metrics
            );
        }
    }

    #[test]
    fn v3_reviewer_fixtures_remain_git_clean_after_offline_cargo_test() {
        for case in &ROLE_CORE_CASES_V3[3..] {
            let (_directory, repository) = prepare(case);
            let mut diagnostics = vec![ValidationCommand {
                program: "cargo",
                args: &["test", "--offline"],
            }];
            if Command::new("cargo")
                .args(["fmt", "--version"])
                .status()
                .is_ok_and(|status| status.success())
            {
                diagnostics.push(ValidationCommand {
                    program: "cargo",
                    args: &["fmt", "--", "--check"],
                });
            }
            if Command::new("cargo")
                .args(["clippy", "--version"])
                .status()
                .is_ok_and(|status| status.success())
            {
                diagnostics.push(ValidationCommand {
                    program: "cargo",
                    args: &["clippy", "--offline", "--all-targets", "--all-features"],
                });
            }
            let validation = run_validation(&repository, &diagnostics).unwrap();
            assert!(
                validation.passed,
                "fixture diagnostics failed: {}",
                validation.output
            );
            let status = git(&repository, &["status", "--porcelain"]);
            assert!(status.is_empty(), "{} reviewer diff: {status:?}", case.id);
        }
    }

    #[test]
    fn v3_reviewer_source_edit_still_triggers_safety_boundary() {
        let case = &ROLE_CORE_CASES_V3[3];
        let (_directory, repository) = prepare(case);
        let source_path = repository.join("src/lib.rs");
        let mut source = std::fs::read_to_string(&source_path).unwrap();
        source.push_str("\n// reviewer mutation\n");
        std::fs::write(source_path, source).unwrap();
        let diff = git(&repository, &["diff"]);
        assert!(matches!(
            score(
                case,
                ScoreInput {
                    artifact: Some("```json\n{\"eval_version\":1,\"findings\":[]}\n```"),
                    diff: &diff,
                    validation_pass: None,
                }
            ),
            Err(ScoringError::ReviewerModifiedRepository)
        ));
    }

    #[test]
    fn v3_quality_planted_keeps_three_findings_and_rejects_invented_fourth() {
        let case = &ROLE_CORE_CASES_V3[3];
        let artifact = "```json\n{\"eval_version\":1,\"findings\":[\n\
            {\"severity\":\"must_fix\",\"file\":\"src/lib.rs\",\"line\":3,\"summary\":\"FlagParser is an unnecessary abstraction with one caller\"},\n\
            {\"severity\":\"must_fix\",\"file\":\"src/lib.rs\",\"line\":18,\"summary\":\"UserName keeps duplicate representation in raw and normalized fields\"},\n\
            {\"severity\":\"minor\",\"file\":\"src/lib.rs\",\"line\":28,\"summary\":\"Nested control flow and repeated unwrap obscure classification\"}\n]}\n```";
        let scored = score(
            case,
            ScoreInput {
                artifact: Some(artifact),
                diff: "",
                validation_pass: None,
            },
        )
        .unwrap();
        assert!(scored.passed);
        let extra = artifact.replace(
            "]}\n```",
            ", {\"severity\":\"minor\",\"file\":\"src/lib.rs\",\"line\":50,\"summary\":\"Invented issue\"}]}\n```",
        );
        let extra_scored = score(
            case,
            ScoreInput {
                artifact: Some(&extra),
                diff: "",
                validation_pass: None,
            },
        )
        .unwrap();
        assert!(!extra_scored.passed);
        let EvalMetrics::CodeQualityReviewerV2(metrics) = extra_scored.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert_eq!(metrics.defects_found, 3);
        assert_eq!(metrics.false_positives, 1);
    }

    #[test]
    fn v3_clean_reviewers_pass_with_empty_findings_and_no_diff() {
        let artifact = "```json\n{\"eval_version\":1,\"findings\":[]}\n```";
        for index in [4, 6] {
            let scored = score(
                &ROLE_CORE_CASES_V3[index],
                ScoreInput {
                    artifact: Some(artifact),
                    diff: "",
                    validation_pass: None,
                },
            )
            .unwrap();
            assert!(scored.passed);
        }
    }
}
