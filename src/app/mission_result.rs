//! What a delivered package's run produced, read from the run store.
//!
//! Captured once, when the package is delivered, and kept with the package:
//! the run's worktree may be released later, and the result must not depend
//! on it still being there. Every field is committed run state or a section
//! quoted verbatim from a stage's own artifact; nothing here is composed.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::domain::{
    ChangedFile, Run, RunId, StageKind, StageOutcome, StageStatus, WorkPackageResult,
};
use crate::providers::section;
use crate::store::SqliteStore;
use crate::workspace::{WorkspaceManager, WorkspaceStatus};

use super::{AppError, RunDiffPreview, query};

/// The same ceiling `senate`'s diff preview reads under.
const DIFF_PREVIEW_LIMIT: usize = 2 * 1024 * 1024;

/// The delta apply would move, bounded and read-only, for a run whose
/// worktree is `Ready`.
///
/// # Errors
/// Returns workspace readiness/ownership or Git failures.
pub(crate) fn preview_delta(
    store: &mut SqliteStore,
    worktrees: &Path,
    run_id: RunId,
) -> Result<RunDiffPreview, AppError> {
    let preview =
        WorkspaceManager::new(worktrees).preview_patch(store, run_id, DIFF_PREVIEW_LIMIT)?;
    Ok(query::summarize_diff(
        String::from_utf8_lossy(&preview.bytes).into_owned(),
        preview.total_bytes,
        preview.truncated,
    ))
}

/// Files listed per result; the same bound the review handoff uses.
const MAX_CHANGED_FILES: usize = crate::providers::change_handoff::MAX_CHANGE_HANDOFF_LISTED_FILES;

/// Reads the delivery evidence for one completed run.
///
/// The changed-file list comes from the same delta apply would move, read
/// only while the run's worktree is `Ready`; a released worktree (an apply
/// that found nothing) leaves it empty and complete.
///
/// # Errors
/// Returns store, artifact integrity, or workspace errors.
pub(crate) fn capture(
    store: &mut SqliteStore,
    worktrees: &Path,
    run_id: RunId,
    now: DateTime<Utc>,
) -> Result<WorkPackageResult, AppError> {
    let run = store.load_run(run_id)?.run;
    let workspace = store
        .load_workspace(run_id)?
        .map(|workspace| workspace.status());
    let (changed_files, changes_complete) = if workspace == Some(WorkspaceStatus::Ready) {
        let preview = preview_delta(store, worktrees, run_id)?;
        let listed = preview.changed_files.len().min(MAX_CHANGED_FILES);
        let complete = !preview.truncated && listed == preview.changed_files.len();
        (
            preview
                .changed_files
                .into_iter()
                .take(MAX_CHANGED_FILES)
                .map(|file| ChangedFile {
                    path: file.path,
                    binary: file.binary,
                })
                .collect(),
            complete,
        )
    } else {
        (Vec::new(), true)
    };

    let outcome = |store: &SqliteStore, run: &Run, kind_filter: &dyn Fn(StageKind) -> bool| {
        latest_stage(run, kind_filter)
            .map(|stage| stage_outcome(store, run, stage))
            .transpose()
    };
    let verification = outcome(store, &run, &|kind| kind == StageKind::Verify)?;
    let decision = outcome(store, &run, &|kind| kind == StageKind::Decision)?;
    let reviews = run
        .stages()
        .iter()
        .filter(|stage| {
            matches!(
                stage.kind(),
                StageKind::CodeQualityReview
                    | StageKind::SpecReview
                    | StageKind::Review
                    | StageKind::IndependentReview
            )
        })
        .map(|stage| stage_outcome(store, &run, stage))
        .collect::<Result<Vec<_>, _>>()?;
    let bottom_line = latest_stage(&run, &|kind| kind.edits_workspace())
        .map(|stage| bottom_line_of(store, &run, stage))
        .transpose()?
        .flatten();
    let open_questions = match &decision {
        Some(decision) if decision.status == StageStatus::Completed => {
            artifact_section(store, &run, &decision.stage_id, "followups")?
        }
        _ => None,
    };

    Ok(WorkPackageResult {
        run_id,
        captured_at: now,
        stage_count: run.stages().len(),
        changed_files,
        changes_complete,
        bottom_line,
        verification,
        reviews,
        decision,
        open_questions,
    })
}

/// The last stage of a kind in workflow order: fix and continue cycles
/// append their own verify and decision stages, and the newest speaks for
/// the run.
fn latest_stage<'run>(
    run: &'run Run,
    kind_filter: &dyn Fn(StageKind) -> bool,
) -> Option<&'run crate::domain::Stage> {
    run.stages()
        .iter()
        .rev()
        .find(|stage| kind_filter(stage.kind()))
}

fn stage_outcome(
    store: &SqliteStore,
    run: &Run,
    stage: &crate::domain::Stage,
) -> Result<StageOutcome, AppError> {
    Ok(StageOutcome {
        stage_id: stage.id().clone(),
        role: stage.role(),
        status: stage.status(),
        bottom_line: bottom_line_of(store, run, stage)?,
    })
}

/// The stage's own `## Bottom line`, verbatim, when it completed and wrote
/// one. A stage that did not complete has nothing to quote.
fn bottom_line_of(
    store: &SqliteStore,
    run: &Run,
    stage: &crate::domain::Stage,
) -> Result<Option<String>, AppError> {
    if stage.status() != StageStatus::Completed {
        return Ok(None);
    }
    artifact_section(store, run, stage.id(), "bottomline")
}

/// One contracted section of a completed stage's artifact, verbatim. A stage
/// without an artifact yields nothing; a corrupt artifact fails the capture,
/// as it fails every other read: the result is kept for good, so a loss at
/// capture time would be permanent and silent.
fn artifact_section(
    store: &SqliteStore,
    run: &Run,
    stage_id: &crate::domain::StageId,
    section_name: &str,
) -> Result<Option<String>, AppError> {
    match query::read_artifact(store, run.id(), stage_id) {
        Ok(view) => Ok(section::extract_verbatim(&view.text, section_name)),
        Err(AppError::ArtifactNotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}
