//! The Markdown record of one verification pass, and its durable copy.
//!
//! Markdown rather than a log so the control room's artifact viewer renders
//! it like every other stage's output, and so the `## Bottom line` the TUI
//! quotes is the same sentence the stage failed with.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::domain::{ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactStatus, ProviderId, Role};
use crate::engine::ProviderRequest;
use crate::providers::ArtifactRecord;

use super::VerifyError;
use super::config::{CONFIG_FILE, CommandSource, DeclineReason, VerifyPlan};
use super::runner::{CommandExit, CommandReport};

/// Lines kept from the end of each stream. A failing test suite says what
/// failed at the bottom; the top is compiler noise the reader can rerun for.
const TAIL_LINES: usize = 200;

/// Bytes kept from the end of each stream after the line cut, so two hundred
/// enormous lines cannot push the artifact past the size the store accepts.
const TAIL_BYTES: usize = 64 * 1024;

/// Ceiling on the whole artifact, the same as the native adapters use.
const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;

/// What the pass concluded; the bottom line is its sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Every command exited zero.
    Passed { commands: usize },
    /// A command did not exit zero; the sentence names it.
    Failed(String),
    /// No commands were configured or detected, so nothing ran.
    NothingChecked,
}

impl Verdict {
    /// The one-line `## Bottom line`, also the `Failed` reason.
    pub(crate) fn bottom_line(&self) -> String {
        match self {
            Self::Passed { commands: 1 } => "passed — 1 command".to_owned(),
            Self::Passed { commands } => format!("passed — {commands} commands"),
            Self::Failed(reason) => format!("failed — {reason}"),
            Self::NothingChecked => {
                "nothing checked — no commands configured or detected".to_owned()
            }
        }
    }

    pub(crate) const fn artifact_status(&self) -> ArtifactStatus {
        match self {
            Self::Passed { .. } | Self::NothingChecked => ArtifactStatus::Complete,
            Self::Failed(_) => ArtifactStatus::Failed,
        }
    }
}

/// The verdict a sequence of reports amounts to. Reports stop at the first
/// failure, so at most one is non-zero and it is the last.
pub(crate) fn verdict(plan: &VerifyPlan, reports: &[CommandReport]) -> Verdict {
    if plan.commands.is_empty() {
        return Verdict::NothingChecked;
    }
    match reports.iter().find(|report| !report.exit.succeeded()) {
        Some(report) => Verdict::Failed(failure_sentence(report)),
        None => Verdict::Passed {
            commands: reports.len(),
        },
    }
}

fn failure_sentence(report: &CommandReport) -> String {
    match &report.exit {
        CommandExit::Code(code) => format!("{} exited {code}", report.command),
        CommandExit::Signal(signal) => {
            format!("{} was killed by signal {signal}", report.command)
        }
        CommandExit::TimedOut(limit) => {
            format!("{} timed out after {} s", report.command, limit.as_secs())
        }
        CommandExit::CouldNotStart(error) => {
            format!("{} could not start: {error}", report.command)
        }
        CommandExit::StatusUnavailable(error) => {
            format!("{} status could not be read: {error}", report.command)
        }
    }
}

/// Renders the whole artifact. `plan` is `None` when configuration failed
/// before any command ran; the verdict then carries the parse error, so the
/// operator reads it where they read every other verdict.
pub(crate) fn render(
    plan: Option<&VerifyPlan>,
    reports: &[CommandReport],
    verdict: &Verdict,
    repeated_from: Option<&str>,
) -> String {
    let mut text = String::from("# Verification\n\n## Bottom line\n");
    text.push_str(&verdict.bottom_line());
    text.push_str("\n\n## Source\n");
    text.push_str(&source_line(plan));
    text.push('\n');
    // Directly under the verdict, because it changes how the verdict should
    // be read and the lead may not get further down the page.
    if let Some(stage_id) = repeated_from {
        let _ = write!(
            text,
            "\n## Not the first time\n\
             Stage `{stage_id}` ended on this same line, at this same base commit, \
             earlier in this run. Two different trees failing identically points at \
             the repository's state rather than at either change — check whether this \
             also fails on an untouched checkout before treating it as a regression.\n"
        );
    }
    let Some(plan) = plan else {
        return text;
    };
    for (index, command) in plan.commands.iter().enumerate() {
        let _ = write!(text, "\n### $ {command}\n");
        match reports.get(index) {
            Some(report) => render_report(&mut text, report),
            None => text.push_str("skipped: not run after the first failure\n"),
        }
    }
    text
}

fn source_line(plan: Option<&VerifyPlan>) -> String {
    match plan.map(|plan| &plan.source) {
        None => format!("`{CONFIG_FILE}` (could not be read)"),
        Some(CommandSource::ConfigFile(origin)) => {
            format!("`{CONFIG_FILE}` `[verify]` table ({})", origin.as_str())
        }
        Some(CommandSource::Detected(marker)) => format!("auto-detected from `{marker}`"),
        Some(CommandSource::Declined { marker, reason }) => format!(
            "nothing: `{marker}` was found but the command it implies was not run — {}. \
             Add a `[verify]` table to `{CONFIG_FILE}` naming the commands this repository \
             should be checked with.",
            decline_sentence(*reason)
        ),
        Some(CommandSource::Nothing) => {
            format!("nothing: no `[verify]` table in `{CONFIG_FILE}` and no recognised build file")
        }
    }
}

/// The half-sentence naming why a detected command was refused, written so
/// the `## Source` line reads as one sentence a reader can act on.
fn decline_sentence(reason: DeclineReason) -> &'static str {
    match reason {
        DeclineReason::Workspaces => {
            "this is a workspaces root, so `npm test` runs every package in the monorepo \
             rather than anything about this change"
        }
        DeclineReason::NoTestScript => {
            "it declares no `test` script, so `npm test` would fail on the missing script \
             without running a check"
        }
    }
}

fn render_report(text: &mut String, report: &CommandReport) {
    match &report.exit {
        CommandExit::Code(code) => {
            let _ = writeln!(text, "exit: {code}");
        }
        CommandExit::Signal(signal) => {
            let _ = writeln!(text, "exit: killed by signal {signal}");
        }
        CommandExit::TimedOut(limit) => {
            let _ = writeln!(text, "exit: timed out after {} s", limit.as_secs());
        }
        CommandExit::CouldNotStart(error) => {
            let _ = writeln!(text, "exit: could not start ({error})");
        }
        CommandExit::StatusUnavailable(error) => {
            let _ = writeln!(text, "exit: status could not be read ({error}); killed");
        }
    }
    for (name, captured) in [("stdout", &report.stdout), ("stderr", &report.stderr)] {
        if captured.bytes.is_empty() && captured.dropped == 0 {
            let _ = writeln!(text, "{name}: (empty)");
            continue;
        }
        let mut body = tail(&String::from_utf8_lossy(&captured.bytes));
        if captured.dropped > 0 {
            body = format!(
                "[… {} bytes not captured before this tail]\n{body}",
                captured.dropped
            );
        }
        let fence = fence_for(&body);
        let _ = write!(text, "{name}:\n{fence}text\n{body}");
        if !body.ends_with('\n') {
            text.push('\n');
        }
        let _ = writeln!(text, "{fence}");
    }
}

/// The last [`TAIL_LINES`] lines, then the last [`TAIL_BYTES`] of those,
/// each cut announced at the top of what remains.
fn tail(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let omitted = lines.len().saturating_sub(TAIL_LINES);
    let mut kept = lines[omitted..].join("\n");
    if text.ends_with('\n') {
        kept.push('\n');
    }
    if kept.len() > TAIL_BYTES {
        let cut = kept.len() - TAIL_BYTES;
        let mut start = cut;
        while !kept.is_char_boundary(start) {
            start += 1;
        }
        kept = format!("[… {start} bytes omitted]\n{}", &kept[start..]);
    }
    if omitted > 0 {
        kept = format!("[… {omitted} lines omitted]\n{kept}");
    }
    kept
}

/// A fence longer than any run of backticks in the body, so command output
/// that itself contains Markdown fences cannot close ours early.
fn fence_for(body: &str) -> String {
    let longest = body
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

/// Writes the artifact file once and describes it for the store.
///
/// Mirrors the native adapters: the file lands under
/// `<root>/<run-id>/artifacts/<stage-id>.md` (suffixed by attempt after the
/// first), is written atomically, and is never overwritten with different
/// bytes — a crash between writing and recording can only leave an identical
/// file behind.
///
/// # Errors
/// Filesystem failures, an oversized artifact, or a same-path file with
/// different content.
pub(crate) fn persist(
    root: &Path,
    request: &ProviderRequest,
    provider_id: &ProviderId,
    base_commit: Option<&str>,
    content: &str,
    status: ArtifactStatus,
    now: DateTime<Utc>,
) -> Result<ArtifactRecord, VerifyError> {
    let mut bytes = content.as_bytes().to_vec();
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(VerifyError::ArtifactTooLarge(MAX_ARTIFACT_BYTES));
    }
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    let path = artifact_path(root, request);
    std::fs::create_dir_all(path.parent().expect("artifact path has a directory"))?;
    write_once(&path, &bytes)?;
    describe(path, &bytes, request, provider_id, base_commit, status, now)
}

/// Where this attempt's artifact lives, before or after it is written. One
/// deterministic path per attempt is what lets a poll repeated after a crash
/// find the file the crashed poll left behind.
pub(crate) fn artifact_path(root: &Path, request: &ProviderRequest) -> PathBuf {
    let filename = if request.attempt() == 1 {
        format!("{}.md", request.stage_id())
    } else {
        format!("{}-attempt-{}.md", request.stage_id(), request.attempt())
    };
    root.join(request.run_id().to_string())
        .join("artifacts")
        .join(filename)
}

/// The store record for artifact bytes already durable at `path`.
///
/// # Errors
/// An invalid record shape (never, for a path this module built).
pub(crate) fn describe(
    path: PathBuf,
    bytes: &[u8],
    request: &ProviderRequest,
    provider_id: &ProviderId,
    base_commit: Option<&str>,
    status: ArtifactStatus,
    now: DateTime<Utc>,
) -> Result<ArtifactRecord, VerifyError> {
    let hash = hex_sha256(bytes);
    let mut metadata = ArtifactMetadata::new(
        ArtifactId::new(),
        request.run_id(),
        request.stage_id().clone(),
        ArtifactKind::Verify,
        Role::Verifier,
        status,
        now,
    )
    .with_provider(provider_id.clone(), None);
    if let Some(base_commit) = base_commit {
        metadata = metadata.with_base_commit(base_commit);
    }
    ArtifactRecord::new(
        metadata,
        request.attempt(),
        path,
        hash,
        u64::try_from(bytes.len()).expect("bounded artifact length fits u64"),
        now,
    )
    .map_err(|error| VerifyError::Artifact(error.to_string()))
}

fn write_once(path: &Path, bytes: &[u8]) -> Result<(), VerifyError> {
    if path.exists() {
        return if std::fs::read(path)? == bytes {
            Ok(())
        } else {
            Err(VerifyError::ArtifactConflict(PathBuf::from(path)))
        };
    }
    let directory = path
        .parent()
        .ok_or_else(|| VerifyError::ArtifactConflict(path.to_path_buf()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(path)? != bytes {
                return Err(VerifyError::ArtifactConflict(path.to_path_buf()));
            }
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut result, "{byte:02x}").expect("String writes cannot fail");
    }
    result
}

/// The status a recorded bottom line stands for, mirroring
/// [`Verdict::artifact_status`] for an artifact read back from disk.
pub(crate) fn status_of_bottom_line(bottom_line: &str) -> ArtifactStatus {
    if bottom_line.starts_with("passed") || bottom_line.starts_with("nothing checked") {
        ArtifactStatus::Complete
    } else {
        ArtifactStatus::Failed
    }
}

/// The bottom line of an artifact this module wrote earlier, read back so a
/// poll repeated after a crash reports the recorded verdict rather than
/// running the commands again.
pub(crate) fn bottom_line_of(content: &str) -> Option<String> {
    let mut lines = content.lines();
    lines.find(|line| line.trim() == "## Bottom line")?;
    lines
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::providers::repo_config::ConfigOrigin;
    use crate::providers::verify::runner::Captured;

    fn plan(commands: &[&str], source: CommandSource) -> VerifyPlan {
        VerifyPlan {
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
            timeout: Duration::from_secs(10),
            source,
        }
    }

    fn report(command: &str, exit: CommandExit, stdout: &str, stderr: &str) -> CommandReport {
        let captured = |text: &str| Captured {
            bytes: text.as_bytes().to_vec(),
            dropped: 0,
        };
        CommandReport {
            command: command.to_owned(),
            exit,
            stdout: captured(stdout),
            stderr: captured(stderr),
        }
    }

    #[test]
    fn a_passing_pass_renders_one_section_per_command_with_its_exit_code() {
        let plan = plan(
            &["cargo fmt --check", "cargo test"],
            CommandSource::ConfigFile(ConfigOrigin::Worktree),
        );
        let reports = [
            report("cargo fmt --check", CommandExit::Code(0), "", ""),
            report("cargo test", CommandExit::Code(0), "ok\n", "warning\n"),
        ];
        let verdict = verdict(&plan, &reports);
        assert_eq!(verdict, Verdict::Passed { commands: 2 });

        let text = render(Some(&plan), &reports, &verdict, None);

        assert!(text.starts_with("# Verification\n\n## Bottom line\npassed — 2 commands\n"));
        assert!(text.contains("## Source\n`.senate.toml` `[verify]` table (worktree)\n"));
        assert!(
            text.contains("### $ cargo fmt --check\nexit: 0\nstdout: (empty)\nstderr: (empty)\n")
        );
        assert!(text.contains(
            "### $ cargo test\nexit: 0\nstdout:\n```text\nok\n```\nstderr:\n```text\nwarning\n```\n"
        ));
    }

    #[test]
    fn the_first_failure_is_the_bottom_line_and_later_commands_are_marked_skipped() {
        let plan = plan(
            &["cargo test", "cargo clippy"],
            CommandSource::ConfigFile(ConfigOrigin::Worktree),
        );
        let reports = [report("cargo test", CommandExit::Code(101), "", "boom\n")];
        let verdict = verdict(&plan, &reports);
        assert_eq!(verdict.bottom_line(), "failed — cargo test exited 101");
        assert_eq!(verdict.artifact_status(), ArtifactStatus::Failed);

        let text = render(Some(&plan), &reports, &verdict, None);

        assert!(text.contains("### $ cargo test\nexit: 101\n"));
        assert!(text.contains("### $ cargo clippy\nskipped: not run after the first failure\n"));
    }

    #[test]
    fn timeouts_signals_and_missing_programs_each_name_themselves() {
        assert_eq!(
            failure_sentence(&report(
                "sleep 9",
                CommandExit::TimedOut(Duration::from_secs(1)),
                "",
                ""
            )),
            "sleep 9 timed out after 1 s"
        );
        assert_eq!(
            failure_sentence(&report("x", CommandExit::Signal(9), "", "")),
            "x was killed by signal 9"
        );
        assert_eq!(
            failure_sentence(&report(
                "x",
                CommandExit::CouldNotStart("nope".into()),
                "",
                ""
            )),
            "x could not start: nope"
        );
    }

    #[test]
    fn bytes_dropped_while_capturing_are_announced_above_the_tail() {
        let mut report = report("noisy", CommandExit::Code(0), "tail\n", "");
        report.stdout.dropped = 4096;

        let mut text = String::new();
        render_report(&mut text, &report);

        assert!(text.contains(
            "stdout:\n```text\n[… 4096 bytes not captured before this tail]\ntail\n```\n"
        ));
    }

    #[test]
    fn nothing_detected_says_so_and_keeps_complete_status() {
        let plan = plan(&[], CommandSource::Nothing);
        let verdict = verdict(&plan, &[]);
        assert_eq!(verdict, Verdict::NothingChecked);
        assert_eq!(verdict.artifact_status(), ArtifactStatus::Complete);

        let text = render(Some(&plan), &[], &verdict, None);

        assert!(text.contains("nothing checked — no commands configured or detected"));
        assert!(text.contains("## Source\nnothing:"));
        assert!(!text.contains("### $"));
    }

    #[test]
    fn streams_keep_only_their_last_two_hundred_lines() {
        let long = (1..=250).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "line {n}");
            text
        });
        let cut = tail(&long);

        assert!(cut.starts_with("[… 50 lines omitted]\nline 51\n"));
        assert!(cut.ends_with("line 250\n"));
        assert_eq!(cut.lines().count(), 201);
        assert_eq!(tail("short\n"), "short\n");
    }

    #[test]
    fn a_fence_always_outruns_the_backticks_in_the_body() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("has ``` inside"), "````");
        assert_eq!(fence_for("has ````` inside"), "``````");
    }

    #[test]
    fn the_bottom_line_reads_back_from_a_rendered_artifact() {
        let plan = plan(&["true"], CommandSource::ConfigFile(ConfigOrigin::Worktree));
        let reports = [report("true", CommandExit::Code(0), "", "")];
        let text = render(Some(&plan), &reports, &verdict(&plan, &reports), None);

        assert_eq!(bottom_line_of(&text).as_deref(), Some("passed — 1 command"));
        assert_eq!(bottom_line_of("# Nothing here\n"), None);
    }
}
