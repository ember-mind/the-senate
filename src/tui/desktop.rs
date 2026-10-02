//! The two things a pull request URL is for once it is on screen: opening it
//! in the browser and putting it on the clipboard. Both go through whatever
//! the platform ships rather than a dependency, and both report a failure as
//! a sentence for the footer instead of a panic.

use std::io::Write;
use std::process::{Command, Stdio};

/// Opens `url` with the platform's default browser.
///
/// # Errors
/// Returns a one-line reason when no opener could be started.
pub(crate) fn open_in_browser(url: &str) -> Result<(), String> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["open"]
    } else {
        &["xdg-open", "wslview"]
    };
    let mut last = String::from("no browser opener found");
    for program in candidates {
        match crate::exec::without_jira_credentials(Command::new(program))
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(source) => last = format!("{program}: {source}"),
        }
    }
    Err(last)
}

/// Opens the Senate — the local 3D view of a campaign — by starting
/// `senate world` as a separate process. It outlives the terminal UI on
/// purpose: closing either surface never stops the other, and never stops
/// an agent. When a Senate server is already running, the command only
/// opens a browser tab on it.
///
/// # Errors
/// Returns a one-line reason when the process could not be started.
pub(crate) fn enter_senate(mission: Option<crate::domain::MissionId>) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    let exe = std::env::current_exe().map_err(|source| format!("senate: {source}"))?;
    let mut command = crate::exec::without_jira_credentials(Command::new(exe));
    command.arg("world");
    if let Some(mission) = mission {
        command.arg("--mission").arg(mission.to_string());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group: Ctrl-C in this terminal reaches the TUI,
        // not the Senate server.
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(|source| format!("senate world: {source}"))
}

/// Puts `text` on the system clipboard.
///
/// # Errors
/// Returns a one-line reason when no clipboard tool accepted it.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    let mut last = String::from("no clipboard tool found");
    for (program, args) in candidates {
        let child = crate::exec::without_jira_credentials(Command::new(program))
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(source) => {
                last = format!("{program}: {source}");
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take()
            && let Err(source) = stdin.write_all(text.as_bytes())
        {
            last = format!("{program}: {source}");
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }
        match child.wait() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => last = format!("{program} exited with {status}"),
            Err(source) => last = format!("{program}: {source}"),
        }
    }
    Err(last)
}
