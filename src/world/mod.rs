//! `senate world`: the Senate — a local-only HTTP server that hands a
//! browser the embedded `world/` app and a small JSON API over the current
//! campaign, read fresh on every request from [`projection::snapshot`].

mod assets;
pub mod projection;
mod server;

use crate::cli::WorldArgs;

/// Opens the Senate for one CLI invocation. Reuses an already-running,
/// still-healthy instance instead of starting a second one; otherwise binds
/// a fresh server and blocks until the user closes it (Ctrl-C/SIGTERM).
///
/// # Errors
/// Returns an error when the data directory, the listener, or the
/// single-instance marker file cannot be reached.
pub fn run(args: &WorldArgs) -> anyhow::Result<()> {
    server::run(args)
}
