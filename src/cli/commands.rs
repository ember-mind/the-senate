use anyhow::Result;
use clap::CommandFactory;

use crate::app::{
    AppError, ApplyOutcome, BlockedDependencyRef, ExecutionReport, ExecutionSelection,
    MissionDetails, MissionListItem, MissionService, NewWorkPackage, QuiescentState, RetryRoute,
    Rework, RunDetails, RunService, RuntimeProviderFactory, StageDependencyRef,
    StageWaitingSummary, UniformProvider, WorkPackageSummary,
};
use crate::domain::{
    DecisionAuthor, DependencyOutcome, DomainEventKind, ModelId, StageStatus, WorkPackageContract,
    WorkPackageStatus, WorkflowKind,
};
use crate::process::ProcessBackend;

use super::{
    Cli, Command, ContractArgs, EvalCommand, EvalRunArgs, MissionCommand, ReviseArgs, RunArgs,
    UpdateArgs, WorldArgs,
};

#[allow(
    clippy::too_many_lines,
    reason = "one dispatch table, one arm per subcommand"
)]
pub fn execute(command: Option<&Command>) -> Result<()> {
    match command {
        Some(Command::RunProcess { manifest }) => {
            crate::process::run_managed_process(manifest)?;
            Ok(())
        }
        Some(Command::ExecProcess { manifest }) => {
            crate::process::exec_managed_process(manifest)?;
            Ok(())
        }
        Some(Command::ImageTool { socket }) => {
            crate::image::run_stdio_server(socket)?;
            Ok(())
        }
        Some(Command::VerifyReleaseTag { tag }) => verify_release_tag(tag),
        Some(Command::RegisterOfficialInstall { executable, asset }) => {
            register_official_install(executable, asset.as_deref())
        }
        Some(Command::InstallSourceOf { executable }) => install_source_of(executable.as_deref()),
        Some(Command::Tui) => anyhow::bail!("TUI dispatch must be handled before CLI commands"),
        Some(Command::World(args)) => world(args),
        Some(Command::Mission { command }) => mission(command),
        Some(Command::Eval { command }) => eval(command),
        Some(Command::Update(args)) => update(*args),
        Some(Command::Doctor) => doctor(),
        Some(Command::Runs) => runs(),
        Some(Command::Fast(args)) => start(WorkflowKind::Fast, args),
        Some(Command::Standard(args)) => start(WorkflowKind::Standard, args),
        Some(Command::Deep(args)) => start(WorkflowKind::Deep, args),
        Some(Command::Review(args)) => start(WorkflowKind::Review, args),
        Some(Command::Status { run_id }) => {
            print_details(&service()?.inspect_run(*run_id)?);
            Ok(())
        }
        Some(Command::Resume { run_id }) => {
            print_report(&service()?.resume_run(*run_id)?);
            Ok(())
        }
        Some(Command::Stop { run_id }) => {
            let report = service()?.stop_run(*run_id)?;
            println!("Run stopped. Workspace and results are preserved.");
            println!("Resume it with `senate resume {run_id}`.");
            print_report(&report);
            Ok(())
        }
        Some(Command::Retry {
            run_id,
            stage_id,
            provider,
            model,
        }) => {
            let route = retry_route(provider.as_deref(), model.as_deref())?;
            print_report(&service()?.retry_stage(*run_id, stage_id, route)?);
            Ok(())
        }
        Some(Command::Resolve {
            run_id,
            attention_id,
            response,
            skip,
        }) => {
            let service = service()?;
            let report = if *skip {
                service.skip_attention(*run_id, *attention_id)?
            } else {
                service.resolve_attention_with_response(
                    *run_id,
                    *attention_id,
                    response.as_deref(),
                )?
            };
            print_report(&report);
            Ok(())
        }
        Some(Command::Apply { run_id }) => {
            let (outcome, report) = service()?.apply_run(*run_id)?;
            match outcome {
                ApplyOutcome::Applied => println!("Changes applied to source repository."),
                ApplyOutcome::NoChanges => println!("No workspace changes to apply."),
            }
            print_report(&report);
            Ok(())
        }
        Some(Command::Rebase { run_id }) => {
            let (receipt, report) = service()?.rebase_run(*run_id)?;
            println!(
                "Moved the change from {} onto {}, {} commit(s) ahead.",
                receipt.from_base, receipt.to_base, receipt.commits_gained
            );
            println!(
                "Verification now predates the move, so `apply` waits for a fresh check: \
                 `senate fix {run_id}`."
            );
            print_report(&report);
            Ok(())
        }
        Some(Command::Pr { run_id }) => {
            // Progress goes to stderr so a script reading stdout still gets
            // only the receipt; a human at the terminal sees both.
            eprintln!(
                "Publishing run {run_id}: committing, pushing to origin, opening a pull request…"
            );
            let started = std::time::Instant::now();
            let (receipt, report) = service()?.publish_run(*run_id)?;
            println!(
                "Pushed branch {} at {} to origin in {}s.",
                receipt.branch,
                receipt.commit,
                started.elapsed().as_secs()
            );
            match receipt.pull_request {
                crate::workspace::PullRequestStatus::Created(url) => {
                    println!("Pull request created:\n\n  {url}\n");
                }
                crate::workspace::PullRequestStatus::AlreadyExists(url) => {
                    println!("Pull request already open:\n\n  {url}\n");
                }
                crate::workspace::PullRequestStatus::Unavailable(reason) => {
                    println!("Pull request not created: {reason}");
                }
            }
            if let Some(note) = receipt.note {
                println!("{note}");
            }
            print_report(&report);
            Ok(())
        }
        Some(Command::Fix { run_id }) => {
            print_report(&service()?.request_fix(*run_id)?);
            Ok(())
        }
        Some(Command::Discard { run_id }) => {
            print_report(&service()?.discard_run(*run_id)?);
            Ok(())
        }
        Some(Command::Archive { run_id, undo }) => {
            service()?.set_run_archived(*run_id, !*undo)?;
            println!(
                "Run {run_id} {}.",
                if *undo {
                    "returned to the list"
                } else {
                    "archived"
                }
            );
            Ok(())
        }
        Some(Command::AutoApprove { run_id, off }) => {
            service()?.set_run_auto_approve(*run_id, !*off)?;
            if *off {
                println!("Run {run_id} will wait for you on every permission request.");
            } else {
                println!(
                    "Run {run_id} will approve grantable permission requests by itself. Questions still stop it."
                );
                println!("Resume it with `senate resume {run_id}` if it is waiting on one now.");
            }
            Ok(())
        }
        Some(Command::Delete { run_id, yes }) => {
            if !*yes {
                println!(
                    "Run {run_id} was NOT deleted. Deleting a run destroys its worktree, its \
files, and its history, and cannot be undone. Pass --yes to go through with it."
                );
                return Ok(());
            }
            let receipt = service()?.purge_run(*run_id)?;
            println!(
                "Run {} deleted for good{}.",
                receipt.run_id,
                if receipt.files_removed {
                    ""
                } else {
                    " (its files were already gone)"
                }
            );
            Ok(())
        }
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

fn service() -> Result<RunService<RuntimeProviderFactory>> {
    Ok(RunService::from_environment(RuntimeProviderFactory)?)
}

fn start(workflow: WorkflowKind, args: &RunArgs) -> Result<()> {
    let selection = execution_selection(
        args.provider.as_deref(),
        args.profile.as_deref(),
        args.model.as_deref(),
    )?;
    let effort = parse_effort(args.effort.as_deref())?;
    let image = if args.allow_image_generation {
        crate::app::ImageGenerationPlan::implementer_only()
    } else {
        crate::app::ImageGenerationPlan::disabled()
    };
    let report = service()?.start_run(
        workflow,
        args.task.clone(),
        &args.repo,
        selection,
        effort,
        &image,
    )?;
    print_report(&report);
    Ok(())
}

/// Omitting both flags means Recommended, the same default the TUI's new-run
/// composer already starts on. It is a routing profile, not a provider: it
/// resolves to authenticated Claude Code or Codex and refuses to fall back to
/// the development `FakeProvider`, so the default can fail to start but can
/// never quietly run a task against something that only looks like work.
/// Fake stays something you ask for by name.
fn execution_selection(
    provider: Option<&str>,
    profile: Option<&str>,
    model: Option<&str>,
) -> Result<Option<ExecutionSelection>> {
    Ok(Some(match (provider, profile) {
        (Some(provider), None) => {
            let provider = UniformProvider::try_from(provider)?;
            match model {
                Some(model) => ExecutionSelection::UniformWithModel(provider, ModelId::new(model)?),
                None => ExecutionSelection::Uniform(provider),
            }
        }
        (None, Some("recommended") | None) => ExecutionSelection::Recommended,
        (None, Some(other)) => {
            anyhow::bail!("unsupported profile {other:?}; supported profiles: recommended")
        }
        (Some(_), Some(_)) => unreachable!("clap rejects conflicting selection flags"),
    }))
}

/// The `--effort` grammar. Omitted is the routing profile's own policy; one
/// level applies to every role; `role=level[,role=level]` names some roles
/// and leaves the rest to the profile. Anything unknown fails closed.
fn parse_effort(value: Option<&str>) -> Result<crate::app::EffortRequest> {
    use crate::app::EffortRequest;
    let Some(value) = value else {
        return Ok(EffortRequest::ProfileDefault);
    };
    if !value.contains('=') {
        return Ok(EffortRequest::Uniform(parse_effort_level(value)?));
    }
    let mut roles = std::collections::HashMap::new();
    for pair in value.split(',') {
        let (role_word, level) = pair
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("malformed effort {pair:?}; expected role=level"))?;
        let role_word = role_word.trim();
        let role = parse_role(role_word)?;
        if roles
            .insert(role, parse_effort_level(level.trim())?)
            .is_some()
        {
            anyhow::bail!("effort names {role_word:?} twice");
        }
    }
    Ok(EffortRequest::PerRole(roles))
}

/// One effort word. `native` preserves the runtime's own configured default;
/// the levels are the domain's own spellings.
fn parse_effort_level(word: &str) -> Result<crate::domain::EffortSetting> {
    if word == "native" {
        return Ok(crate::domain::EffortSetting::NativeDefault);
    }
    word.parse().map_err(|_| {
        anyhow::anyhow!("unsupported effort {word:?}; supported: native, low, medium, high, xhigh")
    })
}

/// A role by the `snake_case` name `status` prints for it, so the flag and
/// the report agree without a second spelling.
fn parse_role(word: &str) -> Result<crate::domain::Role> {
    serde_json::from_value(serde_json::Value::String(word.to_owned())).map_err(|_| {
        anyhow::anyhow!(
            "unknown role {word:?}; supported: researcher, architect, implementer, simplifier, code_quality_reviewer, spec_reviewer, engineering_lead"
        )
    })
}

/// Bootstrap hook: records an installed executable as officially managed so
/// self-update becomes available for it. Validation lives in the update
/// module, so the installer never reproduces the receipt schema, the data
/// directory rules, or the path semantics in shell.
fn register_official_install(executable: &std::path::Path, asset: Option<&str>) -> Result<()> {
    let asset = asset.map_or_else(
        || crate::update::target_asset_name().unwrap_or("unknown"),
        |asset| asset,
    );
    let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
    // A failed receipt write is returned as an error, so install.sh sees a
    // non-zero exit and tells the user automatic updates were not registered
    // rather than implying they were.
    let receipt = crate::update::register_official_install(executable, asset, now)?;
    println!(
        "registered {} as an official Senate {} installation",
        receipt.executable.display(),
        receipt.version
    );
    Ok(())
}

/// Bootstrap hook: reports how an executable would be classified, without
/// executing it. The installer uses this to decide whether a file already at
/// the destination is a Senate installation it may replace.
fn install_source_of(executable: Option<&std::path::Path>) -> Result<()> {
    let executable = match executable {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe()?,
    };
    println!("{}", crate::update::classify_path(&executable)?.label());
    Ok(())
}

/// Opens the Senate: a local 3D view of the current campaign, served over
/// HTTP to 127.0.0.1 only, and blocks until the user closes it (Ctrl-C).
fn world(args: &WorldArgs) -> Result<()> {
    crate::world::run(args)
}

/// Release-pipeline gate. Read-only and offline: it compares a candidate tag
/// against the version this checkout compiles as, and fails the process when
/// they disagree, so a mismatched tag cannot reach publication.
fn verify_release_tag(tag: &str) -> Result<()> {
    let version = crate::update::verify_release_tag(tag)?;
    println!("release tag {tag} matches package version {version}");
    Ok(())
}

/// Reports update status and, when explicitly confirmed, installs an
/// official release over this executable.
///
/// `--check` never mutates anything. Without it the command still refuses to
/// install unless the installation is one Senate recognizes as its own and
/// the user confirms — `--yes` is the only way to skip the prompt, and a
/// non-interactive invocation without it reports instead of guessing.
fn update(args: UpdateArgs) -> Result<()> {
    let service = crate::update::UpdateService::from_environment()?;
    let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
    // Both forms are explicitly typed by the user, so both get a real check.
    // Answering `--check` from a day-old cache made Senate report "up to
    // date" minutes after a release was published.
    let status = service.check_now(now);
    println!("Current version: {}", crate::update::CURRENT_VERSION);
    let crate::update::UpdateStatus::Available(info) = &status else {
        match status {
            crate::update::UpdateStatus::Current => println!("The Senate is up to date."),
            _ if crate::update::checks_disabled() => println!(
                "Update checks are disabled ({}).",
                crate::update::DISABLE_ENVIRONMENT_VARIABLE
            ),
            _ => println!("Update status is unavailable right now."),
        }
        return Ok(());
    };
    println!(
        "Update available: {} \u{2192} {}",
        info.current_version, info.available_version
    );
    if !info.release_url.is_empty() {
        println!("Release: {}", info.release_url);
    }
    let source = crate::update::detect_install_source()?;
    println!("Install source: {}", source.label());
    let strategy = source.strategy();
    if !strategy.is_automatic() {
        println!("{}", strategy.guidance());
        return Ok(());
    }
    if manual_update(args.check) == ManualUpdate::Report {
        println!("Automatic installation is supported. Run `senate update` to install.");
        return Ok(());
    }
    if !args.yes && !confirm_install(&info.available_version.to_string())? {
        println!("Not installing.");
        return Ok(());
    }
    let installed = install_update(info, now)?;
    println!("{}", installed.restart_notice());
    if let Some(warning) = installed.registration_warning() {
        println!("{warning}");
    }
    Ok(())
}

/// What an explicitly typed update command may do to the executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManualUpdate {
    /// Report only. `--check` can never reach the installer.
    Report,
    /// Offer installation, still subject to confirmation and every
    /// install-source, checksum, and version rule.
    Offer,
}

const fn manual_update(check: bool) -> ManualUpdate {
    if check {
        ManualUpdate::Report
    } else {
        ManualUpdate::Offer
    }
}

/// Explicit confirmation before an irreversible action, matching how apply
/// and discard behave. A non-interactive stdin is never treated as consent.
fn confirm_install(version: &str) -> Result<bool> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() {
        println!("Re-run with `--yes` to install {version} without a prompt.");
        return Ok(false);
    }
    print!("Install {version} now? It applies when Senate restarts. [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Re-reads the release so the download and its checksums come from one
/// consistent listing rather than from cached metadata.
fn install_update(
    info: &crate::update::UpdateInfo,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<crate::update::Installed> {
    use crate::update::ReleaseSource as _;
    let source = crate::update::GitHubReleases::new(
        crate::update::OFFICIAL_REPOSITORY,
        std::time::Duration::from_secs(10),
    );
    let release = source
        .latest_stable()?
        .filter(|release| release.version == info.available_version)
        .ok_or_else(|| anyhow::anyhow!("release {} is no longer published", info.tag))?;
    let executable = std::env::current_exe()?;
    let downloader = crate::update::HttpDownloader::new(std::time::Duration::from_secs(120));
    Ok(crate::update::install(
        &release,
        &executable,
        &downloader,
        now,
    )?)
}

/// Version and distribution diagnostics. Never touches the network: how
/// How Senate was installed is knowable offline, and internet reachability is
/// not a doctor failure.
fn print_distribution() {
    println!("  version: {}", crate::update::CURRENT_VERSION);
    match crate::update::detect_install_source() {
        Ok(source) => {
            println!("  install source: {}", source.label());
            println!(
                "  automatic update: {}",
                if source.strategy().is_automatic() {
                    "supported"
                } else {
                    "unavailable for this build"
                }
            );
        }
        Err(error) => println!("  install source: undetermined ({error})"),
    }
    println!(
        "  update checks: {}",
        if crate::update::checks_disabled() {
            "disabled"
        } else {
            "enabled (public GitHub release metadata, at most once per day)"
        }
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "one sequential report, one paragraph per provider/capability"
)]
fn doctor() -> Result<()> {
    let config_file = crate::config::config_file()?;
    let database_file = crate::store::database_file()?;

    println!("The Senate doctor");
    print_distribution();
    println!("  config: {}", config_file.display());
    println!("  database: {}", database_file.display());
    if database_file.exists() {
        let store = crate::store::SqliteStore::open(&database_file)?;
        println!("  database schema: {}", store.schema_version()?);
    } else {
        println!("  database schema: not initialized");
    }
    match crate::providers::claude::ClaudeInstallation::discover() {
        Ok(installation) => {
            println!("  Claude Code: available ({})", installation.version());
            println!(
                "  Claude auth: {}{}",
                if installation.authenticated() {
                    "ready"
                } else {
                    "not authenticated"
                },
                installation
                    .auth_method()
                    .map_or(String::new(), |method| format!(" ({method})"))
            );
        }
        Err(crate::providers::claude::ClaudeProviderError::NotFound) => {
            println!("  Claude Code: not found on PATH");
            println!(
                "  guidance: install/configure Claude Code, verify `claude` works, then rerun `senate doctor`"
            );
        }
        Err(error) => println!("  Claude Code: error ({error})"),
    }
    let suspicious = crate::providers::claude::suspicious_secret_environment();
    if suspicious.is_empty() {
        println!("  secret environment: no known provider credential overrides detected");
    } else {
        println!(
            "  secret environment: set variables: {}",
            suspicious.join(", ")
        );
    }
    match crate::providers::codex::CodexInstallation::discover() {
        Ok(installation) => {
            println!("  Codex CLI: available ({})", installation.version());
            println!(
                "  Codex auth: {}{}",
                if installation.authenticated() {
                    "ready"
                } else {
                    "not authenticated"
                },
                installation
                    .auth_method()
                    .map_or(String::new(), |method| format!(" ({method})"))
            );
        }
        Err(crate::providers::codex::CodexProviderError::NotFound) => {
            println!("  Codex CLI: not found on PATH");
            println!(
                "  guidance: install/configure Codex CLI, authenticate with native `codex login`, then rerun `senate doctor`"
            );
        }
        Err(error) => println!("  Codex CLI: error ({error})"),
    }
    let codex_environment = crate::providers::codex::suspicious_codex_environment();
    if !codex_environment.is_empty() {
        println!(
            "  Codex environment overrides: {}",
            codex_environment.join(", ")
        );
    }
    match crate::providers::opencode::OpencodeInstallation::discover() {
        Ok(installation) => {
            println!("  opencode CLI: available ({})", installation.version());
            println!(
                "  opencode auth: {}",
                if installation.authenticated() {
                    "ready"
                } else {
                    "no configured credentials"
                }
            );
        }
        Err(crate::providers::opencode::OpencodeProviderError::NotFound) => {
            println!("  opencode CLI: not found on PATH");
            println!(
                "  guidance: install opencode, authenticate with native `opencode auth login`, then rerun `senate doctor`. opencode is explicit-only: `--provider opencode --model <provider/model>`, never chosen by --profile recommended"
            );
        }
        Err(error) => println!("  opencode CLI: error ({error})"),
    }
    let opencode_environment = crate::providers::opencode::suspicious_opencode_environment();
    if !opencode_environment.is_empty() {
        println!(
            "  opencode environment overrides: {}",
            opencode_environment.join(", ")
        );
    }
    println!("  fake provider: available (deterministic development/testing)");
    // Image generation is opt-in per run; without the flag nothing here is
    // needed, so an unavailable backend is information, not a warning.
    match crate::image::backend_available() {
        Ok(installation) => println!(
            "  image generation: available (backend Codex CLI {} built-in image_gen, native auth{}; opt in per run with --allow-image-generation)",
            installation.version(),
            installation
                .auth_method()
                .map_or(String::new(), |method| format!(" via {method}"))
        ),
        Err(error) => println!(
            "  image generation: unavailable ({error}; only needed with --allow-image-generation)"
        ),
    }
    // Git is as fundamental as tmux here: every run needs a repository and a
    // managed worktree.
    if let Some(version) = crate::git::git_version(&crate::git::Git::default()) {
        println!("  Git: available ({version})");
    } else {
        println!("  Git: not found on PATH");
        println!("  guidance: install Git, verify `git --version` works, then rerun");
    }
    let tmux = crate::process::TmuxBackend::new(std::env::current_exe()?);
    match tmux.availability() {
        Ok(availability) => println!("  tmux: available ({})", availability.version),
        Err(crate::process::ProcessError::TmuxNotFound) => println!("  tmux: unavailable"),
        Err(error) => println!("  tmux: error ({error})"),
    }
    Ok(())
}

fn eval(command: &EvalCommand) -> Result<()> {
    match command {
        EvalCommand::List => {
            for version in [
                crate::eval::ROLE_CORE_SUITE_VERSION,
                crate::eval::ROLE_CORE_SUITE_VERSION_V2,
                crate::eval::ROLE_CORE_SUITE_VERSION_V3,
            ] {
                let suite = crate::eval::EvalSuite::load(version)?;
                println!("{} · {}", suite.version(), suite.fingerprint());
                for case in suite.cases() {
                    println!(
                        "  {}  role={}  workflow={}",
                        case.id,
                        enum_text(case.target_role),
                        enum_text(case.workflow)
                    );
                }
            }
            println!(
                "Architect, Researcher, and EngineeringLead cases are deferred until deterministic high-signal oracles exist."
            );
            Ok(())
        }
        EvalCommand::Run(args) => run_eval(args),
        EvalCommand::Report { paths } => {
            let results = crate::eval::load_results(paths)?;
            print!("{}", crate::eval::render_report(&results)?);
            Ok(())
        }
    }
}

fn run_eval(args: &EvalRunArgs) -> Result<()> {
    let provider = crate::eval::EvalProvider::try_from(args.provider.as_str())?;
    let target = crate::eval::EvalTarget::new(provider, args.model.clone())?;
    let suite = crate::eval::EvalSuite::load(&args.suite)?;
    let runner = crate::eval::EvalRunner::new(crate::eval::EvalRunOptions {
        target,
        effort: parse_effort_level(args.effort.as_deref().unwrap_or("native"))?,
        repeat: args.repeat,
        allow_native_usage: args.allow_native_usage,
        output: args.out.clone(),
    })?;
    let summary = runner.run(&suite, |case, ordinal, total| {
        println!(
            "{} · {} / {} · {ordinal}/{total} · {}",
            suite.version(),
            args.provider,
            args.model.as_deref().unwrap_or("native_default"),
            case.id
        );
    })?;
    for result in &summary.results {
        let mark = match result.status {
            crate::eval::EvalStatus::Passed => "✓",
            crate::eval::EvalStatus::Failed => "✗",
            crate::eval::EvalStatus::InfrastructureFailure => "!",
        };
        println!(
            "{mark} {} · repetition {} · {:?}",
            result.case_id, result.repetition, result.status
        );
    }
    println!("Evidence: {}", summary.output_directory.display());
    print!("{}", crate::eval::render_report(&summary.results)?);
    Ok(())
}

fn runs() -> Result<()> {
    let items = service()?.list_runs()?;
    if items.is_empty() {
        println!("No runs.");
        return Ok(());
    }
    for run in items {
        println!(
            "{}  {}  {}  {}  {}  {}",
            run.id,
            enum_text(run.workflow),
            enum_text(run.status),
            run.task_summary,
            run.repository
                .as_deref()
                .map_or("-".to_owned(), |path| path.display().to_string()),
            run.updated_at.to_rfc3339()
        );
    }
    Ok(())
}

fn missions() -> Result<MissionService> {
    Ok(MissionService::from_environment()?)
}

#[allow(
    clippy::too_many_lines,
    reason = "one dispatch table, one arm per subcommand"
)]
fn mission(command: &MissionCommand) -> Result<()> {
    let missions = missions()?;
    match command {
        MissionCommand::New { title, goal, repo } => {
            let details = missions.create_mission(title.clone(), goal.clone(), repo)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::List => {
            print_mission_list(&missions.list_missions()?);
            Ok(())
        }
        MissionCommand::Show { mission_id } => {
            print_mission(&missions.inspect_mission(*mission_id)?);
            Ok(())
        }
        MissionCommand::Add {
            mission_id,
            package_id,
            contract,
            depends_on,
        } => {
            let package = NewWorkPackage {
                id: package_id.clone(),
                contract: build_contract(contract)?,
                dependencies: depends_on.clone(),
            };
            let details = missions.add_package(*mission_id, package)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Revise {
            mission_id,
            package_id,
            contract,
        } => {
            let current = missions.inspect_mission(*mission_id)?;
            let summary = current.package(package_id).ok_or_else(|| {
                anyhow::anyhow!("mission {mission_id} has no package {package_id}")
            })?;
            let revised = revise_contract(summary, contract)?;
            let details = missions.revise_package(*mission_id, package_id, revised)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Depends {
            mission_id,
            package_id,
            on,
        } => {
            let details = missions.set_dependencies(*mission_id, package_id, on.clone())?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Start {
            mission_id,
            package_id,
            provider,
            profile,
            effort,
        } => {
            let selection = execution_selection(provider.as_deref(), profile.as_deref(), None)?;
            let effort = parse_effort(effort.as_deref())?;
            let (report, details) =
                missions.start_package(&service()?, *mission_id, package_id, selection, effort)?;
            print_report(&report);
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Attach {
            mission_id,
            package_id,
            run_id,
        } => {
            let details = missions.attach_run(*mission_id, package_id, *run_id)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Integrate {
            mission_id,
            package_id,
        } => {
            let details = missions.integrate_package(&service()?, *mission_id, package_id)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Fix {
            mission_id,
            package_id,
        } => {
            let (report, details) =
                missions.rework_package(&service()?, *mission_id, package_id, Rework::Fix)?;
            print_report(&report);
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Continue {
            mission_id,
            package_id,
            instruction,
        } => {
            let (report, details) = missions.rework_package(
                &service()?,
                *mission_id,
                package_id,
                Rework::Continue(instruction.clone()),
            )?;
            print_report(&report);
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Ask {
            mission_id,
            message,
            provider,
            profile,
            effort,
            apply,
        } => {
            let routed = provider.is_some() || profile.is_some() || effort.is_some();
            let selection = execution_selection(provider.as_deref(), profile.as_deref(), None)?;
            let effort = parse_effort(effort.as_deref())?;
            let continuing = missions.inspect_mission(*mission_id)?.lead.is_some();
            if continuing && routed {
                println!(
                    "Note: the lead session already runs on its own configuration; --provider, --profile and --effort apply to the first message only."
                );
            }
            let (turn, details) =
                missions.ask_lead(&service()?, *mission_id, message, selection, effort)?;
            print_lead_turn(&turn, *mission_id);
            if *apply
                && turn.answer.as_ref().is_some_and(
                    |answer| matches!(&answer.proposals, Ok(changes) if !changes.is_empty()),
                )
            {
                let details = missions.apply_lead_proposals(*mission_id)?;
                println!();
                println!("Applied.");
                print_mission(&details);
            } else {
                println!();
                print_mission(&details);
            }
            Ok(())
        }
        MissionCommand::Apply { mission_id } => {
            let answer = missions
                .latest_lead_answer(*mission_id)?
                .ok_or(crate::app::AppError::NoLeadAnswer(*mission_id))?;
            match &answer.proposals {
                Ok(changes) if changes.is_empty() => {
                    println!("The lead proposed no plan changes; nothing to apply.");
                    Ok(())
                }
                Ok(changes) => {
                    for change in changes {
                        println!("  - {change}");
                    }
                    let details = missions.apply_lead_proposals(*mission_id)?;
                    println!("Applied.");
                    print_mission(&details);
                    Ok(())
                }
                Err(source) => Err(crate::app::AppError::LeadProposalUnreadable {
                    run_id: answer.run_id,
                    stage_id: answer.stage_id.clone(),
                    source: source.clone(),
                }
                .into()),
            }
        }
        MissionCommand::Resume { mission_id } => {
            let (reports, details) = missions.resume_mission(&service()?, *mission_id)?;
            for report in &reports {
                print_report(report);
            }
            println!("Resumed {} run(s).", reports.len());
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Retry {
            mission_id,
            package_id,
        } => {
            let details = missions.retry_package(*mission_id, package_id)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::CancelPackage {
            mission_id,
            package_id,
            reason,
        } => {
            let details = missions.cancel_package(*mission_id, package_id, reason.clone())?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Decide {
            mission_id,
            title,
            why,
            by,
        } => {
            let author = parse_decision_author(by)?;
            let details =
                missions.record_decision(*mission_id, title.clone(), why.clone(), author)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Complete { mission_id } => {
            let details = missions.complete_mission(*mission_id)?;
            print_mission(&details);
            Ok(())
        }
        MissionCommand::Cancel { mission_id, reason } => {
            let details = missions.cancel_mission(*mission_id, reason.clone())?;
            print_mission(&details);
            Ok(())
        }
    }
}

/// Builds a new package's contract from its CLI arguments.
fn build_contract(args: &ContractArgs) -> Result<WorkPackageContract> {
    Ok(WorkPackageContract {
        title: args.title.clone(),
        goal: args.goal.clone(),
        rationale: args.why.clone().unwrap_or_default(),
        scope: args.scope.clone().unwrap_or_default(),
        acceptance_criteria: args.accept.clone(),
        verification: args.verify.clone().unwrap_or_default(),
        workflow: parse_workflow(&args.workflow)?,
    })
}

/// Overwrites only the fields `--revise` was given, keeping the rest of the
/// current contract.
fn revise_contract(current: &WorkPackageSummary, args: &ReviseArgs) -> Result<WorkPackageContract> {
    Ok(WorkPackageContract {
        title: args.title.clone().unwrap_or_else(|| current.title.clone()),
        goal: args.goal.clone().unwrap_or_else(|| current.goal.clone()),
        rationale: args
            .why
            .clone()
            .unwrap_or_else(|| current.rationale.clone()),
        scope: args.scope.clone().unwrap_or_else(|| current.scope.clone()),
        acceptance_criteria: if args.accept.is_empty() {
            current.acceptance_criteria.clone()
        } else {
            args.accept.clone()
        },
        verification: args
            .verify
            .clone()
            .unwrap_or_else(|| current.verification.clone()),
        workflow: match args.workflow.as_deref() {
            Some(word) => parse_workflow(word)?,
            None => current.workflow,
        },
    })
}

fn parse_workflow(word: &str) -> Result<WorkflowKind> {
    match word {
        "fast" => Ok(WorkflowKind::Fast),
        "standard" => Ok(WorkflowKind::Standard),
        "deep" => Ok(WorkflowKind::Deep),
        "review" => Ok(WorkflowKind::Review),
        other => {
            anyhow::bail!("unsupported workflow {other:?}; supported: fast, standard, deep, review")
        }
    }
}

fn parse_decision_author(word: &str) -> Result<DecisionAuthor> {
    match word {
        "user" => Ok(DecisionAuthor::User),
        "lead" => Ok(DecisionAuthor::Lead),
        other => anyhow::bail!("unsupported decision author {other:?}; supported: user, lead"),
    }
}

/// The delivered package's evidence: counts and committed statuses, then
/// the stages' own bottom lines quoted verbatim.
fn print_result(result: &crate::domain::WorkPackageResult) {
    let files = if result.changes_complete {
        format!("{} file(s) changed", result.changed_files.len())
    } else {
        format!("{}+ file(s) changed (list cut)", result.changed_files.len())
    };
    let verification = result
        .verification
        .as_ref()
        .map_or_else(|| "no verify stage".to_owned(), |v| enum_text(v.status));
    let reviews = if result.reviews.is_empty() {
        "no reviews".to_owned()
    } else {
        result
            .reviews
            .iter()
            .map(|review| format!("{} {}", review.stage_id, enum_text(review.status)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("      delivered: {files}; verify {verification}; {reviews}");
    if let Some(line) = &result.bottom_line {
        println!("      said: {}", first_line(line));
    }
    if let Some(decision) = result
        .decision
        .as_ref()
        .and_then(|d| d.bottom_line.as_deref())
    {
        println!("      decision: {}", first_line(decision));
    }
    if result.open_questions.is_some() {
        println!(
            "      open: the decision left follow-ups; `senate mission continue` carries them"
        );
    }
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

fn print_mission_list(items: &[MissionListItem]) {
    if items.is_empty() {
        println!("No missions yet.");
        return;
    }
    for item in items {
        println!(
            "{}  {}  {}/{} integrated  {} active  {} need you  {}",
            item.id,
            enum_text(item.status),
            item.integrated,
            item.packages,
            item.active,
            item.attention,
            item.title
        );
    }
}

/// The lead's answer as the user reads it: prose first, then the plan
/// changes as a list with the command that applies them.
fn print_lead_turn(turn: &crate::app::LeadTurn, mission_id: crate::domain::MissionId) {
    let run = &turn.report.details;
    match turn.answer.as_ref() {
        None => {
            println!(
                "The lead (run {}) is {}; no answer yet. `senate status {}` shows where it is.",
                run.id,
                enum_text(run.status),
                run.id
            );
        }
        Some(answer) => {
            println!("Lead (run {}, turn {}):", answer.run_id, answer.turn);
            println!();
            println!("{}", answer.prose());
            println!();
            match &answer.proposals {
                Ok(changes) if changes.is_empty() => println!("No plan changes proposed."),
                Ok(changes) => {
                    println!("Proposed plan changes:");
                    for change in changes {
                        println!("  - {change}");
                    }
                    println!("Apply them with `senate mission apply {mission_id}`.");
                }
                Err(crate::domain::PlanChangeParseError::MissingSection) => {
                    println!("No plan changes proposed.");
                }
                Err(error) => println!("The answer's plan changes could not be read: {error}"),
            }
        }
    }
}

fn print_mission(details: &MissionDetails) {
    println!("Mission {}: {}", details.id, details.title);
    println!(
        "Status: {}   Repository: {}   Base: {}",
        enum_text(details.status),
        details.repository.display(),
        &details.base_commit[..details.base_commit.len().min(12)]
    );
    println!("Goal: {}", details.goal);
    if let Some(lead) = details.lead.as_ref() {
        println!(
            "Lead: run {} ({}), {} turn(s)",
            lead.run_id,
            enum_text(lead.run_status),
            lead.turns
        );
    }
    println!();

    let integrated = details
        .packages
        .iter()
        .filter(|package| package.status == WorkPackageStatus::Integrated)
        .count();
    let active = details
        .packages
        .iter()
        .filter(|package| package.status.has_active_run())
        .count();
    println!(
        "Packages ({integrated}/{} integrated, {active} active)",
        details.packages.len()
    );
    for package in &details.packages {
        println!(
            "  {:<10} {}  {}",
            enum_text(package.status),
            package.id,
            package.title
        );
        println!("      goal: {}", package.goal);
        if !package.dependencies.is_empty() {
            println!(
                "      depends on: {}",
                package
                    .dependencies
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(run_id) = package.current_run {
            println!(
                "      run: {run_id} ({})",
                package
                    .run_status
                    .map_or_else(|| "unknown".to_owned(), enum_text)
            );
        }
        if let Some(reason) = package.reason.as_deref() {
            println!("      reason: {reason}");
        }
        if let Some(result) = &package.result {
            print_result(result);
        }
    }

    if !details.decisions.is_empty() {
        println!();
        println!("Decisions");
        for decision in &details.decisions {
            println!(
                "  - {} ({}): {}",
                decision.title,
                enum_text(decision.author),
                decision.rationale
            );
        }
    }

    println!();
    if details.attention.is_empty() {
        println!("Nothing needs you.");
    } else {
        println!("Needs you");
        for (package_id, reason) in &details.attention.blocked {
            println!("  - {package_id} is blocked: {reason}");
        }
        for (package_id, reason) in &details.attention.failed {
            println!("  - {package_id} failed: {reason}");
        }
        for package_id in &details.attention.awaiting_integration {
            let run_id = details
                .package(package_id)
                .and_then(|package| package.current_run)
                .map_or_else(|| "unknown".to_owned(), |run_id| run_id.to_string());
            println!(
                "  - {package_id} is done (run {run_id}); `senate mission integrate {} {package_id}` brings it in",
                details.id
            );
        }
    }
}

fn print_report(report: &ExecutionReport) {
    for event in &report.committed_events {
        print_event(
            event.sequence,
            event.stage_id.as_ref().map(ToString::to_string),
            &event.kind,
        );
    }
    println!();
    print_details(&report.details);
    match &report.outcome {
        QuiescentState::NeedsUser => {
            println!("Resolve each attention request with `senate resolve`.");
        }
        QuiescentState::Failed => println!("Retry a failed stage with `senate retry`."),
        QuiescentState::Paused | QuiescentState::Interrupted => {
            println!(
                "Continue recovery with `senate resume {}`.",
                report.details.id
            );
        }
        QuiescentState::WaitingForProvider { stage_id } => {
            println!("Stage {stage_id} is waiting for provider progress; resume later.");
        }
        _ => {}
    }
}

fn print_event(sequence: u64, stage: Option<String>, kind: &DomainEventKind) {
    let scope = stage.map_or_else(|| "run".to_owned(), |stage| format!("stage {stage}"));
    match kind {
        DomainEventKind::ProviderProgress { message, .. } => {
            println!("[{sequence}] {scope}: {message}");
        }
        DomainEventKind::ProviderUsageUpdated {
            input_units,
            output_units,
            ..
        } => println!(
            "[{sequence}] {scope}: usage +{input_units} input units, +{output_units} output units"
        ),
        DomainEventKind::ProviderFailed { reason, .. } => println!(
            "[{sequence}] {scope}: provider failed{}",
            reason
                .as_deref()
                .map_or(String::new(), |reason| format!(": {reason}"))
        ),
        _ => println!("[{sequence}] {scope}: {}", event_name(kind)),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one status projection keeps configured and actual columns aligned in source order"
)]
fn print_details(details: &RunDetails) {
    println!("Run        {}", details.id);
    println!("Workflow   {}", enum_text(details.workflow));
    println!("Status     {}", enum_text(details.status));
    println!(
        "Profile    {} ({})",
        details.profile, details.profile_version
    );
    println!(
        "Repository {}",
        details
            .repository
            .as_deref()
            .map_or("unavailable".to_owned(), |path| path.display().to_string())
    );
    println!(
        "Workspace  {}",
        details
            .workspace_status
            .map_or("unavailable".to_owned(), |status| format!("{status:?}")
                .to_lowercase())
    );
    println!(
        "Base       {}",
        details.base_commit.as_deref().unwrap_or("unavailable")
    );
    println!("Revision   {}", details.revision.value());
    println!("Created    {}", details.created_at.to_rfc3339());
    println!("Updated    {}", details.updated_at.to_rfc3339());
    println!();
    println!("Task");
    println!(
        "{}",
        details
            .task
            .as_deref()
            .unwrap_or("<legacy input unavailable>")
    );
    println!();
    println!("Routing");
    for route in &details.routes {
        println!(
            "{}  {}  {}  {}  effort={}",
            enum_text(route.role),
            route.configured_provider,
            route
                .configured_model
                .as_deref()
                .unwrap_or("native default"),
            route.reason,
            route
                .requested_effort
                .map_or("unstated", crate::domain::EffortSetting::label)
        );
    }
    println!();
    println!("Stages");
    for stage in &details.stages {
        println!(
            "{} {} ({}) · role={} · configured={}/{}{} · effort={} · actual={}/{} · session={} · native={} · conversation={} · process={}",
            stage_mark(stage.status),
            stage.id,
            enum_text(stage.status),
            enum_text(stage.role),
            stage.configured_provider,
            stage
                .configured_model
                .as_deref()
                .unwrap_or("native default"),
            if stage.route_overridden {
                " (operator override)"
            } else {
                ""
            },
            stage.observed_effort.as_ref().map_or_else(
                || format!("{} requested", stage.requested_effort.label()),
                |observed| {
                    format!(
                        "{} requested → {observed} observed",
                        stage.requested_effort.label()
                    )
                }
            ),
            stage.actual_provider.as_deref().unwrap_or("not started"),
            stage.actual_model.as_deref().unwrap_or("unconfirmed"),
            stage
                .provider_session_record
                .as_deref()
                .unwrap_or("unavailable"),
            stage.native_session.as_deref().unwrap_or("unavailable"),
            stage
                .provider_session_status
                .as_deref()
                .unwrap_or("unavailable"),
            stage.process_status.as_deref().unwrap_or("unavailable")
        );
        if let Some(line) = waiting_line(stage.waiting.as_ref()) {
            println!("    {line}");
        }
        if let Some(reason) = stage.failure_reason.as_deref() {
            println!("    reason: {reason}");
        }
        if let Some(model) = stage.model_fallback() {
            println!(
                "    try: senate retry {} {} --provider codex --model {model}",
                details.id, stage.id
            );
        }
    }
    println!();
    println!("Attention");
    if details.attention.is_empty() {
        println!("none");
    } else {
        for request in &details.attention {
            println!(
                "{} · {} · {} · {}",
                request.id,
                request.stage_id,
                enum_text(request.kind),
                request.summary
            );
        }
    }
    if !details.image_generations.is_empty() {
        println!();
        println!("Image generations (worktree files; not visually reviewed)");
        for image in &details.image_generations {
            println!(
                "{} · {} (attempt {}) · {}/{} · {} · {} bytes · sha256 {} · {}",
                image.ordinal,
                image.stage_id,
                image.attempt,
                image.backend,
                image.model,
                image.output_path,
                image.output_size,
                image.output_sha256,
                image.completed_at.to_rfc3339()
            );
        }
    }
    println!();
    for line in usage_lines(&details.usage) {
        println!("{line}");
    }
}

/// Why a Pending/Ready stage isn't running, as one extra indented CLI line.
/// `None` when the stage isn't Pending/Ready, or its dependencies are all
/// satisfied and the scheduler just hasn't marked it Ready yet. A blocked
/// required dependency outranks the rest: this stage is about to be skipped
/// in turn.
fn waiting_line(waiting: Option<&StageWaitingSummary>) -> Option<String> {
    use std::fmt::Write as _;

    let waiting = waiting?;
    if !waiting.blocked_by.is_empty() {
        return Some(format!("blocked by: {}", blocked_ids(&waiting.blocked_by)));
    }
    if waiting.waiting_on.is_empty() {
        return None;
    }
    let mut line = format!("waiting on: {}", dependency_ids(&waiting.waiting_on));
    if !waiting.degraded.is_empty() {
        let _ = write!(line, " (degraded: {})", dependency_ids(&waiting.degraded));
    }
    Some(line)
}

/// Raw dependency stage ids, comma-joined — same technical register as the
/// stage rows above them.
fn dependency_ids(dependencies: &[StageDependencyRef]) -> String {
    dependencies
        .iter()
        .map(|dependency| dependency.id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Raw dependency stage ids with their outcome, comma-joined — a skipped
/// dependency is never reported as having failed.
fn blocked_ids(dependencies: &[BlockedDependencyRef]) -> String {
    dependencies
        .iter()
        .map(|dependency| format!("{} ({})", dependency.id, outcome_word(dependency.outcome)))
        .collect::<Vec<_>>()
        .join(", ")
}

const fn outcome_word(outcome: DependencyOutcome) -> &'static str {
    match outcome {
        DependencyOutcome::Failed => "failed",
        DependencyOutcome::Skipped => "skipped",
    }
}

/// Provider-native units, one line per runtime that reported any.
///
/// Totals are never summed across runtimes: their input figures do not
/// measure the same thing. A runtime whose input total already contains its
/// cache reads says so in place instead of listing the cache read again as a
/// further quantity.
fn usage_lines(usage: &crate::app::RunUsage) -> Vec<String> {
    use std::fmt::Write as _;
    if usage.is_empty() {
        return vec!["Usage      not reported".to_owned()];
    }
    usage
        .providers()
        .map(|entry| {
            let mut line = format!(
                "Usage      {:<8} {} input units",
                entry.provider, entry.usage.input_units
            );
            let folded_cache_read = entry
                .input_contains_cache_reads()
                .then_some(entry.usage.cache_read_units)
                .flatten();
            if let Some(cached) = folded_cache_read {
                let _ = write!(line, " ({cached} of them cached)");
            }
            let _ = write!(line, " · {} output units", entry.usage.output_units);
            for (label, value) in [
                (
                    "cache read",
                    if folded_cache_read.is_some() {
                        None
                    } else {
                        entry.usage.cache_read_units
                    },
                ),
                ("cache write", entry.usage.cache_write_units),
                ("reasoning output", entry.usage.reasoning_output_units),
            ] {
                if let Some(value) = value {
                    let _ = write!(line, " · {value} {label} units");
                }
            }
            line
        })
        .collect()
}

fn stage_mark(status: StageStatus) -> &'static str {
    match status {
        StageStatus::Completed => "✓",
        StageStatus::Failed
        | StageStatus::NeedsUser
        | StageStatus::Paused
        | StageStatus::Interrupted => "!",
        StageStatus::Running | StageStatus::Ready => ">",
        StageStatus::Pending | StageStatus::Skipped => "·",
    }
}

fn enum_text(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// `--provider` alone sends the stage to that provider's native default
/// model; `--model` without `--provider` is refused by clap before this.
fn retry_route(provider: Option<&str>, model: Option<&str>) -> Result<Option<RetryRoute>> {
    provider
        .map(|provider| {
            Ok::<_, AppError>(RetryRoute::new(
                UniformProvider::try_from(provider)?,
                model.map(ModelId::new).transpose()?,
            ))
        })
        .transpose()
        .map_err(Into::into)
}

fn event_name(kind: &DomainEventKind) -> &'static str {
    match kind {
        DomainEventKind::RunCreated { .. } => "run created",
        DomainEventKind::RunPreparationStarted => "workspace preparation started",
        DomainEventKind::RunPrepared => "workspace ready",
        DomainEventKind::RunStarted => "run started",
        DomainEventKind::RunPaused => "run paused",
        DomainEventKind::RunInterrupted => "run interrupted",
        DomainEventKind::RunResumed => "run resumed",
        DomainEventKind::RunRecovered => "run recovered",
        DomainEventKind::RunCompleted => "run completed",
        DomainEventKind::RunFailed => "run failed",
        DomainEventKind::RunApplied => "run applied",
        DomainEventKind::RunDiscarded => "run discarded",
        DomainEventKind::WorkspaceRebased { .. } => "workspace rebased",
        DomainEventKind::RunPublished { .. } => "published",
        DomainEventKind::RunFixRequested { .. } => "fix requested",
        DomainEventKind::RunContinueRequested { .. } => "continue requested",
        DomainEventKind::StageReady { .. } => "ready",
        DomainEventKind::StageStarted => "started",
        DomainEventKind::StagePaused => "paused",
        DomainEventKind::StageInterrupted => "interrupted",
        DomainEventKind::StageResumed => "resumed",
        DomainEventKind::StageRecovered => "recovered",
        DomainEventKind::StageCompleted => "completed",
        DomainEventKind::StageSkipped => "skipped",
        DomainEventKind::StageFailed => "failed",
        DomainEventKind::StageRetryScheduled => "retry scheduled",
        DomainEventKind::StageRouteOverridden { .. } => "route overridden",
        DomainEventKind::NeedsUser { .. } => "attention requested",
        DomainEventKind::AttentionResolved { .. } => "attention resolved",
        DomainEventKind::AttentionCancelled { .. } => "attention cancelled",
        DomainEventKind::ProviderStarted { .. } => "provider started",
        DomainEventKind::ProviderRuntimeObserved { .. } => "provider runtime observed",
        DomainEventKind::ProviderNeedsUser { .. } => "provider awaiting user",
        DomainEventKind::ProviderPaused { .. } => "provider paused",
        DomainEventKind::ProviderInterrupted { .. } => "provider interrupted",
        DomainEventKind::ProviderCompleted { .. } => "provider completed",
        DomainEventKind::ProviderFailed { .. } => "provider failed",
        DomainEventKind::UsageUpdated => "usage updated",
        DomainEventKind::ProviderProgress { .. } | DomainEventKind::ProviderUsageUpdated { .. } => {
            unreachable!("formatted separately")
        }
        DomainEventKind::ProviderResumed { .. } => "provider resumed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{StageId, StageKind};

    /// A skipped required dependency must never print as "failed": each
    /// blocked dependency states its own outcome.
    #[test]
    fn waiting_line_states_each_blocked_dependencys_own_outcome() {
        let waiting = StageWaitingSummary {
            waiting_on: Vec::new(),
            blocked_by: vec![
                BlockedDependencyRef {
                    id: StageId::new("quality_review").unwrap(),
                    kind: StageKind::CodeQualityReview,
                    outcome: DependencyOutcome::Failed,
                },
                BlockedDependencyRef {
                    id: StageId::new("spec_review").unwrap(),
                    kind: StageKind::SpecReview,
                    outcome: DependencyOutcome::Skipped,
                },
            ],
            degraded: Vec::new(),
        };

        assert_eq!(
            waiting_line(Some(&waiting)),
            Some("blocked by: quality_review (failed), spec_review (skipped)".to_owned())
        );
    }

    /// The `--check` form is report-only by construction: there is one path to
    /// the installer, and this decision closes it.
    #[test]
    fn an_explicit_check_can_never_reach_the_installer() {
        assert_eq!(manual_update(true), ManualUpdate::Report);
        assert_eq!(manual_update(false), ManualUpdate::Offer);
    }

    /// Both typed forms must perform a real check. The cache-aware entry point
    /// exists for background detection only, so its name must not appear here
    /// — answering a typed `--check` from a day-old cache is the bug this
    /// module was fixed for.
    #[test]
    fn typed_update_commands_never_use_the_cache_aware_entry_point() {
        // Only the non-test half of this file is the call graph under test.
        let source = include_str!("commands.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        assert!(
            !code.contains("cached_status"),
            "CLI update commands must call check_now, not cached_status"
        );
        assert!(code.contains("service.check_now(now)"));
    }
}

#[cfg(test)]
mod effort_flag_tests {
    use std::collections::HashMap;

    use super::{parse_effort, parse_effort_level, parse_role};
    use crate::app::EffortRequest;
    use crate::domain::{EffortSetting, Role};

    #[test]
    fn omitted_is_the_profiles_policy_and_a_word_is_every_role() {
        assert_eq!(parse_effort(None).unwrap(), EffortRequest::ProfileDefault);
        assert_eq!(
            parse_effort(Some("native")).unwrap(),
            EffortRequest::Uniform(EffortSetting::NativeDefault)
        );
        for (word, setting) in [
            ("low", EffortSetting::LOW),
            ("medium", EffortSetting::MEDIUM),
            ("high", EffortSetting::HIGH),
            ("xhigh", EffortSetting::XHIGH),
        ] {
            assert_eq!(
                parse_effort(Some(word)).unwrap(),
                EffortRequest::Uniform(setting),
                "{word}"
            );
        }
    }

    #[test]
    fn role_pairs_name_some_roles_in_status_spelling() {
        let request =
            parse_effort(Some("architect=xhigh, implementer=low,simplifier=native")).unwrap();
        assert_eq!(
            request,
            EffortRequest::PerRole(HashMap::from([
                (Role::Architect, EffortSetting::XHIGH),
                (Role::Implementer, EffortSetting::LOW),
                (Role::Simplifier, EffortSetting::NativeDefault),
            ]))
        );
        assert_eq!(
            parse_role("code_quality_reviewer").unwrap(),
            Role::CodeQualityReviewer
        );
        assert_eq!(
            parse_role("engineering_lead").unwrap(),
            Role::EngineeringLead
        );
    }

    /// Unknown words fail closed, in both positions, and a role named twice
    /// is a contradiction rather than a last-one-wins.
    #[test]
    fn unknown_levels_roles_and_repeats_are_refused() {
        assert!(parse_effort_level("max").is_err());
        assert!(parse_effort_level("Medium").is_err());
        assert!(parse_effort(Some("turbo")).is_err());
        assert!(parse_effort(Some("Architect=high")).is_err());
        assert!(parse_effort(Some("planner=high")).is_err());
        assert!(parse_effort(Some("architect=turbo")).is_err());
        assert!(parse_effort(Some("architect=high,architect=low")).is_err());
        assert!(
            parse_effort(Some("architect")).is_err(),
            "a bare word must be a level"
        );
        assert!(parse_effort(Some("architect=")).is_err());
        assert!(parse_effort(Some("=high")).is_err());
    }
}
