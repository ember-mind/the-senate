//! The mission's lead: a run-shaped conversation whose every turn reads
//! the canonical mission state and answers with prose and, when the plan
//! should move, a `## Plan changes` section.
//!
//! Nothing here changes a mission. The brief is rendered from the read
//! model, the answer is read from the run's artifact through the
//! integrity-checked path, and the proposals it carries are parsed, never
//! interpreted. Applying them is the service's job, after the user says so.

use std::fmt::Write as _;

use crate::domain::{
    PlanChange, PlanChangeParseError, RunId, RunStatus, StageId, StageKind, StageStatus,
    WorkPackageStatus, parse_plan_changes,
};
use crate::providers::section;
use crate::store::SqliteStore;

use super::mission_query::MissionDetails;
use super::{AppError, ExecutionReport, query};

/// One exchange with the lead: the run report of the turn, and the answer
/// when the turn finished with one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadTurn {
    pub report: ExecutionReport,
    pub answer: Option<LeadAnswer>,
}

/// The lead session a mission has, as the read model shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadSummary {
    pub run_id: RunId,
    /// Committed status of the lead run.
    pub run_status: RunStatus,
    /// Exchanges so far, counting the one in progress.
    pub turns: usize,
}

/// What the lead answered last, with the proposals its answer carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadAnswer {
    pub run_id: RunId,
    pub stage_id: StageId,
    pub turn: usize,
    /// The whole artifact, verbatim.
    pub text: String,
    /// The answer's own `## Bottom line`, verbatim, when it wrote one.
    pub bottom_line: Option<String>,
    /// The parsed `## Plan changes`; an empty list is an explicit "none",
    /// `Err(MissingSection)` an answer that proposed nothing.
    pub proposals: Result<Vec<PlanChange>, PlanChangeParseError>,
}

impl LeadAnswer {
    /// The answer with its `## Plan changes` section removed: what the
    /// user reads, since the proposals are shown as a list of their own.
    #[must_use]
    pub fn prose(&self) -> String {
        let mut prose = String::new();
        let mut skipping = false;
        for line in self.text.lines() {
            let trimmed = line.trim();
            if trimmed.eq_ignore_ascii_case(crate::domain::PLAN_CHANGES_HEADING) {
                skipping = true;
                continue;
            }
            if skipping && trimmed.starts_with("## ") {
                skipping = false;
            }
            if !skipping {
                prose.push_str(line);
                prose.push('\n');
            }
        }
        prose.trim_end().to_owned()
    }
}

/// The canonical mission state, rendered for the lead. Deterministic for
/// a given read model: the same mission renders the same brief.
#[must_use]
pub(crate) fn brief(details: &MissionDetails) -> String {
    let mut text = String::new();
    let _ = write!(
        text,
        "# Mission brief: {}\n\nGoal: {}\nRepository: {}\nStatus: {}\n\n",
        details.title,
        details.goal,
        details.repository.display(),
        status_word(details)
    );
    text.push_str("## Plan\n\n");
    if details.packages.is_empty() {
        text.push_str("No packages yet.\n");
    }
    for package in &details.packages {
        let _ = writeln!(
            text,
            "- `{}` ({}, {}): {} — {}",
            package.id,
            package_word(package.status),
            enum_text(package.workflow),
            package.title,
            package.goal
        );
        if !package.dependencies.is_empty() {
            let _ = writeln!(
                text,
                "  depends on: {}",
                package
                    .dependencies
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !package.rationale.is_empty() {
            let _ = writeln!(text, "  why: {}", package.rationale);
        }
        for criterion in &package.acceptance_criteria {
            let _ = writeln!(text, "  accept: {criterion}");
        }
        if let Some(reason) = package.reason.as_deref() {
            let _ = writeln!(text, "  note: {reason}");
        }
        if let Some(result) = package.result.as_ref()
            && let Some(bottom_line) = result.bottom_line.as_deref()
        {
            let _ = writeln!(text, "  delivered: {}", bottom_line.trim());
        }
    }
    text.push('\n');
    if !details.decisions.is_empty() {
        text.push_str("## Decisions\n\n");
        for decision in &details.decisions {
            let _ = writeln!(
                text,
                "- {} ({}): {}",
                decision.title,
                enum_text(decision.author),
                decision.rationale
            );
        }
        text.push('\n');
    }
    text.push_str("## Needs the user\n\n");
    if details.attention.is_empty() {
        text.push_str("Nothing.\n");
    }
    for (id, reason) in &details.attention.blocked {
        let _ = writeln!(text, "- `{id}` is blocked: {reason}");
    }
    for (id, reason) in &details.attention.failed {
        let _ = writeln!(text, "- `{id}` failed: {reason}");
    }
    for id in &details.attention.awaiting_integration {
        let _ = writeln!(text, "- `{id}` is delivered and waits to be integrated");
    }
    text
}

/// One turn's instruction: the brief, then the user's message.
#[must_use]
pub(crate) fn turn(brief: &str, message: &str) -> String {
    format!("{brief}\n## Message from the user\n\n{}\n", message.trim())
}

/// The lead run's latest finished turn, or `None` while it has not
/// answered yet. A turn still at work, or one that failed, does not hide
/// the answer before it: the newest completed turn is the answer. An
/// artifact that fails its integrity check is an error, never a silent
/// blank.
pub(crate) fn latest_answer(
    store: &mut SqliteStore,
    run_id: RunId,
) -> Result<Option<LeadAnswer>, AppError> {
    let loaded = store.load_run(run_id)?;
    let turns: Vec<_> = loaded
        .run
        .stages()
        .iter()
        .filter(|stage| stage.kind() == StageKind::Lead)
        .collect();
    let Some((turn, last)) = turns
        .iter()
        .enumerate()
        .rev()
        .find(|(_, stage)| stage.status() == StageStatus::Completed)
    else {
        return Ok(None);
    };
    let view = match query::read_artifact(store, run_id, last.id()) {
        Ok(view) => view,
        Err(AppError::ArtifactNotFound { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(LeadAnswer {
        run_id,
        stage_id: last.id().clone(),
        turn: turn + 1,
        bottom_line: section::extract_verbatim(&view.text, "bottomline"),
        proposals: parse_plan_changes(&view.text),
        text: view.text,
    }))
}

/// The lead as the read model shows it, when the mission has one.
pub(crate) fn summary(store: &mut SqliteStore, run_id: RunId) -> Result<LeadSummary, AppError> {
    let loaded = store.load_run(run_id)?;
    Ok(LeadSummary {
        run_id,
        run_status: loaded.run.status(),
        turns: loaded
            .run
            .stages()
            .iter()
            .filter(|stage| stage.kind() == StageKind::Lead)
            .count(),
    })
}

fn status_word(details: &MissionDetails) -> String {
    enum_text(details.status)
}

fn package_word(status: WorkPackageStatus) -> &'static str {
    match status {
        WorkPackageStatus::Planned => "planned",
        WorkPackageStatus::Ready => "ready",
        WorkPackageStatus::Running => "running",
        WorkPackageStatus::Blocked => "blocked",
        WorkPackageStatus::Delivered => "delivered",
        WorkPackageStatus::Integrated => "integrated",
        WorkPackageStatus::Failed => "failed",
        WorkPackageStatus::Cancelled => "cancelled",
    }
}

fn enum_text(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_drops_the_plan_changes_section_and_keeps_everything_else() {
        let answer = LeadAnswer {
            run_id: RunId::from_u128(1),
            stage_id: StageId::new("lead_1").unwrap(),
            turn: 1,
            text:
                "## Bottom line\n\nSplit it.\n\n## Plan changes\n\n- none\n\n## Notes\n\nLater.\n"
                    .to_owned(),
            bottom_line: None,
            proposals: Ok(Vec::new()),
        };
        assert_eq!(
            answer.prose(),
            "## Bottom line\n\nSplit it.\n\n## Notes\n\nLater."
        );
    }
}
