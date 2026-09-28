mod cli;
mod config;
mod exec;

pub mod app;
pub mod domain;
pub mod engine;
pub mod eval;
pub mod git;
pub mod image;
pub mod process;
pub mod providers;
pub mod store;
pub mod tui;
pub mod update;
pub mod workspace;
pub mod world;

use anyhow::{Context, Result};
use clap::Parser;
use std::io::IsTerminal as _;
use tracing_subscriber::EnvFilter;

/// Runs The Senate's command-line application.
///
/// # Errors
/// Returns an error when diagnostics or command execution cannot initialize.
pub fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let tui_mode = frontend_mode(cli.command.as_ref(), interactive)? == FrontendMode::Tui;
    init_tracing(tui_mode)?;
    if tui_mode {
        return tui::run();
    }
    cli::commands::execute(cli.command.as_ref())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrontendMode {
    Cli,
    Tui,
}

fn frontend_mode(command: Option<&cli::Command>, interactive: bool) -> Result<FrontendMode> {
    match command {
        Some(cli::Command::Tui) | None if interactive => Ok(FrontendMode::Tui),
        Some(cli::Command::Tui) => anyhow::bail!(
            "The Senate TUI requires interactive stdin and stdout. Use `senate --help` for CLI commands."
        ),
        _ => Ok(FrontendMode::Cli),
    }
}

fn init_tracing(tui_mode: bool) -> Result<()> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).context("RUST_LOG contains an invalid filter")?,
        Err(std::env::VarError::NotPresent) => EnvFilter::new("senate=info"),
        Err(error) => return Err(error).context("RUST_LOG is not valid Unicode"),
    };

    if tui_mode {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(std::io::sink)
            .compact()
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .context("failed to initialize TUI tracing subscriber")
    } else {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .context("failed to initialize tracing subscriber")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_uses_tui_only_for_interactive_terminal() {
        assert_eq!(frontend_mode(None, true).unwrap(), FrontendMode::Tui);
        assert_eq!(frontend_mode(None, false).unwrap(), FrontendMode::Cli);
    }

    #[test]
    fn hidden_and_normal_commands_never_enter_tui() {
        let hidden = cli::Command::RunProcess {
            manifest: "/tmp/process.json".into(),
        };
        assert_eq!(
            frontend_mode(Some(&hidden), true).unwrap(),
            FrontendMode::Cli
        );
        assert_eq!(
            frontend_mode(Some(&cli::Command::Runs), true).unwrap(),
            FrontendMode::Cli
        );
    }

    #[test]
    fn explicit_tui_rejects_non_interactive_use() {
        assert!(frontend_mode(Some(&cli::Command::Tui), false).is_err());
        assert_eq!(
            frontend_mode(Some(&cli::Command::Tui), true).unwrap(),
            FrontendMode::Tui
        );
    }
}
