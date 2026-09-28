//! Local Ratatui projection and control surface over application use cases.

mod app;
mod bottom_line;
mod desktop;
mod follow_ups;
mod format;
mod input;
mod markdown;
mod mascot;
mod motion;
mod render;

mod state;
mod terminal;
mod theme;
mod worker;

use anyhow::Result;

/// Opens interactive local control room.
///
/// # Errors
/// Returns application, terminal initialization, input, or rendering failures.
pub fn run() -> Result<()> {
    let repository = std::env::current_dir()?;
    let app = app::TuiApp::new(&repository)?;
    terminal::install_panic_restore_hook();
    let mut terminal = terminal::TerminalSession::enter()?;
    app.run(&mut terminal)
}
