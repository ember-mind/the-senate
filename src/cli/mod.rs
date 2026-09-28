pub mod commands;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::domain::{AttentionRequestId, MissionId, RunId, StageId, WorkPackageId};

/// Orchestrate native coding agents as a specialized engineering team.
#[derive(Debug, Parser)]
#[command(
    name = "senate",
    version,
    about = "The Senate orchestrates native coding agents. Legacy command name: `senate`."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Internal managed-process runner.
    #[command(name = "__run-process", hide = true)]
    RunProcess { manifest: PathBuf },
    /// Internal signal-normalizing exec bridge.
    #[command(name = "__exec-process", hide = true)]
    ExecProcess { manifest: PathBuf },
    /// Internal run-scoped MCP server for the image-generation tool.
    #[command(name = "__image-tool", hide = true)]
    ImageTool {
        /// Unix socket of the Senate process hosting the tool.
        #[arg(long)]
        socket: PathBuf,
    },
    /// Internal release-pipeline gate: canonical tag matching this build.
    #[command(name = "__verify-release-tag", hide = true)]
    VerifyReleaseTag { tag: String },
    /// Internal bootstrap hook: record an installed executable as official.
    #[command(name = "__register-official-install", hide = true)]
    RegisterOfficialInstall {
        executable: PathBuf,
        /// Release asset the executable was installed from.
        #[arg(long)]
        asset: Option<String>,
    },
    /// Internal bootstrap hook: classify an executable without running it.
    #[command(name = "__install-source", hide = true)]
    InstallSourceOf { executable: Option<PathBuf> },
    /// Open interactive local control room.
    Tui,
    /// Open the Senate: a local 3D view of the current campaign in your
    /// browser.
    World(WorldArgs),
    /// Plan and direct a multi-package mission above individual runs.
    Mission {
        #[command(subcommand)]
        command: MissionCommand,
    },
    /// Experimental role-specific provider/model evaluation tools.
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    /// Run implementation-only workflow.
    Fast(RunArgs),
    /// Run architecture, implementation, review, and decision workflow.
    Standard(RunArgs),
    /// Run full research-to-decision workflow.
    Deep(RunArgs),
    /// Run read-only parallel review workflow.
    Review(RunArgs),
    /// List known runs.
    Runs,
    /// Inspect one run.
    Status { run_id: RunId },
    /// Continue one prepared or suspended run.
    Resume { run_id: RunId },
    /// Stop execution, keeping the run, workspace, and results resumable.
    Stop { run_id: RunId },
    /// Retry one failed stage explicitly.
    Retry {
        run_id: RunId,
        stage_id: StageId,
        /// Send this stage to another provider (claude|codex|fake) instead of
        /// the one its role was configured with. Only this stage moves.
        #[arg(long)]
        provider: Option<String>,
        /// Model for the provider named by --provider; omit for its native
        /// default.
        #[arg(long, requires = "provider")]
        model: Option<String>,
    },
    /// Resolve one pending attention request and continue.
    Resolve {
        run_id: RunId,
        attention_id: AttentionRequestId,
        /// Answer for a provider question. For a permission request, omit to
        /// approve it, or give an instruction to continue without granting it.
        #[arg(long, conflicts_with = "skip")]
        response: Option<String>,
        /// Decline the permission request and continue the task without it.
        #[arg(long)]
        skip: bool,
    },
    /// Send a completed run back to fix its own result, then decide again.
    Fix { run_id: RunId },
    /// Apply completed workspace changes to source repository.
    Apply { run_id: RunId },
    /// Move completed workspace changes onto the source checkout's current HEAD.
    Rebase { run_id: RunId },
    /// Publish completed workspace changes as a remote branch and pull request.
    Pr { run_id: RunId },
    /// Discard run and remove owned workspace resources.
    Discard { run_id: RunId },
    /// Archive a run out of the default list, or bring it back with --undo.
    Archive {
        run_id: RunId,
        /// Return the run to the default list instead of archiving it.
        #[arg(long)]
        undo: bool,
    },
    /// Approve this run's permission requests without asking each time.
    ///
    /// Questions, and requests that cannot be expressed as an exact
    /// permission rule, still stop the run and wait for you.
    AutoApprove {
        run_id: RunId,
        /// Go back to being asked about every permission request.
        #[arg(long)]
        off: bool,
    },
    /// Delete an archived run for good: worktree, files, and rows. No undo.
    Delete {
        run_id: RunId,
        /// Required: deleting a run is irreversible, so it is never implied.
        #[arg(long)]
        yes: bool,
    },
    /// Check for a newer official release of The Senate.
    Update(UpdateArgs),
    /// Check The Senate's local environment.
    Doctor,
}

/// `senate world` flags.
#[derive(Clone, Debug, Default, PartialEq, Eq, Args)]
pub struct WorldArgs {
    /// Mission whose campaign the Senate shows; omitted shows none bound.
    #[arg(long)]
    pub mission: Option<MissionId>,
    /// Port to bind; 0 (the default) lets the OS assign one.
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    /// Print the URL but do not launch a browser.
    #[arg(long)]
    pub no_open: bool,
    /// Named scenario the browser loads instead of the live campaign,
    /// carried as `?demo=<scenario>` in the URL.
    #[arg(long)]
    pub demo: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Args)]
pub struct UpdateArgs {
    /// Report update status without installing anything.
    #[arg(long)]
    pub check: bool,
    /// Install without the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Subcommand)]
pub enum MissionCommand {
    /// Create a mission over a Git checkout.
    New {
        title: String,
        #[arg(long)]
        goal: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// List missions.
    List,
    /// Show one mission: packages in dependency order, decisions, attention.
    Show { mission_id: MissionId },
    /// Add a work package to the plan.
    Add {
        mission_id: MissionId,
        package_id: WorkPackageId,
        #[command(flatten)]
        contract: ContractArgs,
        /// Package ids this one depends on (repeatable).
        #[arg(long = "depends-on")]
        depends_on: Vec<WorkPackageId>,
    },
    /// Replace the contract of a package nothing has run for. Only the given
    /// fields change.
    Revise {
        mission_id: MissionId,
        package_id: WorkPackageId,
        #[command(flatten)]
        contract: ReviseArgs,
    },
    /// Replace the dependency list of a package nothing has run for.
    Depends {
        mission_id: MissionId,
        package_id: WorkPackageId,
        #[arg(long = "on")]
        on: Vec<WorkPackageId>,
    },
    /// Start a child run for a ready package; its task is the package
    /// handoff.
    Start {
        mission_id: MissionId,
        package_id: WorkPackageId,
        #[arg(long, conflicts_with = "profile")]
        provider: Option<String>,
        #[arg(long, conflicts_with = "provider")]
        profile: Option<String>,
        #[arg(long)]
        effort: Option<String>,
    },
    /// Bind an existing run to a ready package.
    Attach {
        mission_id: MissionId,
        package_id: WorkPackageId,
        run_id: RunId,
    },
    /// Record that a delivered package's run was applied to the checkout.
    Integrate {
        mission_id: MissionId,
        package_id: WorkPackageId,
    },
    /// Return a failed package to the plan so a new run can serve it.
    Retry {
        mission_id: MissionId,
        package_id: WorkPackageId,
    },
    /// Send a delivered package's run back to fix its own review findings.
    Fix {
        mission_id: MissionId,
        package_id: WorkPackageId,
    },
    /// Send a delivered package's run back with an instruction.
    Continue {
        mission_id: MissionId,
        package_id: WorkPackageId,
        instruction: String,
    },
    /// Resume every package run that is waiting to be driven, then observe.
    Resume { mission_id: MissionId },
    /// Ask the mission's lead something. The first message opens the lead
    /// session; later ones continue it. The answer's plan changes are
    /// listed, and applied only with `--apply` or `mission apply`.
    Ask {
        mission_id: MissionId,
        message: String,
        /// Provider for the lead session; read on the first message only,
        /// later turns run on the session's own configuration.
        #[arg(long, conflicts_with = "profile")]
        provider: Option<String>,
        /// Routing profile for the lead session (first message only).
        #[arg(long, conflicts_with = "provider")]
        profile: Option<String>,
        /// Effort for the lead session (first message only).
        #[arg(long)]
        effort: Option<String>,
        /// Apply the answer's plan changes straight away.
        #[arg(long)]
        apply: bool,
    },
    /// Apply the plan changes the lead proposed in its latest answer.
    Apply { mission_id: MissionId },
    /// Cancel a package nothing depends on and no run is serving.
    CancelPackage {
        mission_id: MissionId,
        package_id: WorkPackageId,
        #[arg(long, default_value = "cancelled by the operator")]
        reason: String,
    },
    /// Record a design decision the plan rests on.
    Decide {
        mission_id: MissionId,
        title: String,
        #[arg(long)]
        why: String,
        /// Who made the decision (`user`|`lead`).
        #[arg(long, default_value = "user")]
        by: String,
    },
    /// Complete a mission whose packages are all integrated or cancelled.
    Complete { mission_id: MissionId },
    /// Cancel a mission and every open package (stop running packages
    /// first).
    Cancel {
        mission_id: MissionId,
        #[arg(long, default_value = "cancelled by the operator")]
        reason: String,
    },
}

/// The engineering contract for a new work package.
#[derive(Clone, Debug, PartialEq, Eq, Args)]
pub struct ContractArgs {
    #[arg(long)]
    pub title: String,
    #[arg(long)]
    pub goal: String,
    /// Why the package exists in the plan.
    #[arg(long)]
    pub why: Option<String>,
    /// Expected or permitted scope; omitted means the goal bounds it.
    #[arg(long)]
    pub scope: Option<String>,
    /// Acceptance criteria (repeatable).
    #[arg(long = "accept")]
    pub accept: Vec<String>,
    /// Verification the lead expects beyond the repository's own checks.
    #[arg(long)]
    pub verify: Option<String>,
    /// Built-in workflow that delivers the package (fast|standard|deep|review).
    #[arg(long, default_value = "fast")]
    pub workflow: String,
}

/// The engineering contract fields to overwrite on an existing package. Every
/// field is optional: only what is given changes.
#[derive(Clone, Debug, PartialEq, Eq, Args)]
pub struct ReviseArgs {
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub goal: Option<String>,
    #[arg(long)]
    pub why: Option<String>,
    #[arg(long)]
    pub scope: Option<String>,
    /// Acceptance criteria (repeatable); omitted keeps the current criteria.
    #[arg(long = "accept")]
    pub accept: Vec<String>,
    #[arg(long)]
    pub verify: Option<String>,
    #[arg(long)]
    pub workflow: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Subcommand)]
pub enum EvalCommand {
    /// List source-controlled evaluation suites and cases.
    List,
    /// Execute one candidate target against an isolated suite.
    Run(EvalRunArgs),
    /// Aggregate one or more result files/directories without selecting a winner.
    Report {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Args)]
pub struct EvalRunArgs {
    /// Source-controlled suite version.
    #[arg(long, default_value = "role_core_v1")]
    pub suite: String,
    /// Candidate provider (`claude`, `codex`, or synthetic `fake`).
    #[arg(long)]
    pub provider: String,
    /// Explicit provider model; omission means native configured/default model.
    #[arg(long)]
    pub model: Option<String>,
    /// Fresh repetitions per case.
    #[arg(long, default_value_t = 1)]
    pub repeat: u32,
    /// Explicit acknowledgement that native evaluation may consume provider usage.
    #[arg(long)]
    pub allow_native_usage: bool,
    /// Requested native-runtime effort for the candidate role
    /// (`native|low|medium|high|xhigh`); omitted means native.
    #[arg(long)]
    pub effort: Option<String>,
    /// Evidence output directory. Defaults under ~/.senate/evals.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Args)]
pub struct RunArgs {
    /// Task sent unchanged to each provider stage after outer trim normalization.
    pub task: String,
    /// Git repository; defaults to current directory.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Native provider (`claude` or `codex`) or deterministic development
    /// provider (`fake`). Overrides the default routing profile.
    #[arg(long, conflicts_with = "profile")]
    pub provider: Option<String>,
    /// Versioned routing profile (`recommended`). Used by default when neither
    /// selection flag is given.
    #[arg(long, conflicts_with = "provider")]
    pub profile: Option<String>,
    /// Requested native-runtime effort. One level for every role
    /// (`native|low|medium|high|xhigh`), or `role=level[,role=level]` to
    /// name some roles and leave the rest to the routing profile. Omitted
    /// means the profile's own per-role policy under Recommended, and native
    /// under `--provider`; `native` opts every role out.
    #[arg(long)]
    pub effort: Option<String>,
    /// Let the Implementer generate PNG images into the worktree through the
    /// local Codex CLI's built-in image tool (needs an authenticated `codex`;
    /// at most four generations per run). Off by default.
    #[arg(long)]
    pub allow_image_generation: bool,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;

    #[test]
    fn parses_workflow_and_control_commands() {
        let cli = Cli::try_parse_from([
            "senate",
            "deep",
            "inspect repository",
            "--repo",
            "/tmp/repo",
            "--provider",
            "fake",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Deep(RunArgs { provider: Some(ref provider), .. })) if provider == "fake"
        ));

        let run = RunId::from_u128(1);
        let cli = Cli::try_parse_from(["senate", "status", &run.to_string()]).unwrap();
        assert_eq!(cli.command, Some(Command::Status { run_id: run }));

        let cli = Cli::try_parse_from(["senate", "__run-process", "/tmp/spec.json"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::RunProcess {
                manifest: PathBuf::from("/tmp/spec.json")
            })
        );

        let cli = Cli::try_parse_from(["senate", "__exec-process", "/tmp/spec.json"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::ExecProcess {
                manifest: PathBuf::from("/tmp/spec.json")
            })
        );

        let cli = Cli::try_parse_from(["senate", "tui"]).unwrap();
        assert_eq!(cli.command, Some(Command::Tui));

        let mission = MissionId::new().to_string();
        let cli = Cli::try_parse_from([
            "senate",
            "world",
            "--mission",
            &mission,
            "--port",
            "4123",
            "--no-open",
            "--demo",
            "review",
        ])
        .unwrap();
        assert_eq!(
            cli.command,
            Some(Command::World(WorldArgs {
                mission: Some(MissionId::from_str(&mission).unwrap()),
                port: 4123,
                no_open: true,
                demo: Some("review".to_owned()),
            }))
        );

        let cli = Cli::try_parse_from(["senate", "world"]).unwrap();
        assert_eq!(cli.command, Some(Command::World(WorldArgs::default())));

        let cli = Cli::try_parse_from([
            "senate",
            "eval",
            "run",
            "--provider",
            "codex",
            "--model",
            "fixture-model",
            "--repeat",
            "3",
            "--allow-native-usage",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Eval {
                command: EvalCommand::Run(EvalRunArgs { repeat: 3, .. })
            })
        ));
    }

    #[test]
    fn parses_mission_commands() {
        let mission = MissionId::new().to_string();
        let cli = Cli::try_parse_from([
            "senate",
            "mission",
            "add",
            &mission,
            "persistence",
            "--title",
            "T",
            "--goal",
            "G",
            "--accept",
            "a",
            "--accept",
            "b",
            "--depends-on",
            "x",
            "--workflow",
            "standard",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Mission {
                command: MissionCommand::Add {
                    contract: ContractArgs { ref accept, ref workflow, .. },
                    ref depends_on,
                    ..
                }
            }) if accept == &["a".to_owned(), "b".to_owned()]
                && workflow == "standard"
                && depends_on == &[WorkPackageId::new("x").unwrap()]
        ));

        let cli = Cli::try_parse_from([
            "senate",
            "mission",
            "start",
            &mission,
            "persistence",
            "--provider",
            "fake",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Mission {
                command: MissionCommand::Start { provider: Some(ref provider), .. }
            }) if provider == "fake"
        ));
    }
}
