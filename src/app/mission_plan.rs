//! Preview and approval of one exact Consul answer, independent of the TUI.
use super::mission_lead::{self, LeadPlanPreview};
use super::mission_service::{now, observe_runs};
use super::{AppError, MissionDetails, MissionService};
use crate::domain::{
    DecisionAuthor, DecisionId, Mission, MissionChange, MissionError, MissionId, PlanChange,
    WorkPackageContract,
};
use crate::store::{LeadProposalApproval, SqliteStore, StoreError, sha256_hex};
use chrono::{DateTime, Utc};
use std::fmt::Write as _;

impl MissionService {
    /// Validates proposals without changing the Mission and pins both revisions.
    ///
    /// # Errors
    /// Refuses missing, malformed, already applied, empty, or invalid proposals.
    pub fn preview_lead_proposals(
        &self,
        mission_id: MissionId,
    ) -> Result<LeadPlanPreview, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let loaded = observe_runs(&mut store, &self.worktrees, mission_id)?;
        let binding = store
            .mission_lead(mission_id)?
            .ok_or(AppError::NoLeadAnswer(mission_id))?;
        let answer = mission_lead::latest_answer(&mut store, binding.run_id)?
            .ok_or(AppError::NoLeadAnswer(mission_id))?;
        if store.lead_proposals_applied(mission_id, answer.run_id, &answer.stage_id)? {
            return Err(StoreError::LeadProposalAlreadyApplied {
                mission_id,
                stage_id: answer.stage_id,
            }
            .into());
        }
        let changes =
            answer
                .proposals
                .clone()
                .map_err(|source| AppError::LeadProposalUnreadable {
                    run_id: answer.run_id,
                    stage_id: answer.stage_id.clone(),
                    source,
                })?;
        if changes.is_empty() {
            return Err(AppError::NoLeadProposals(mission_id));
        }
        let mut proposed = loaded.mission.clone();
        let at = now().max(*proposed.updated_at());
        if !apply_changes(&mut proposed, &changes, at)?.changed() {
            return Err(AppError::NoLeadProposals(mission_id));
        }
        let approval = LeadProposalApproval {
            mission_revision: loaded.revision,
            run_id: answer.run_id,
            run_revision: store.load_run(answer.run_id)?.revision,
            stage_id: answer.stage_id,
            answer_sha256: sha256_hex(answer.text.as_bytes()),
        };
        let text = render_changes(&changes);
        Ok(LeadPlanPreview {
            mission_id,
            approval,
            changes,
            text,
        })
    }

    /// Applies only the batch the operator previewed, once, in one transaction.
    ///
    /// # Errors
    /// Refuses stale Mission/answer evidence or any invalid domain operation.
    pub fn approve_lead_proposals(
        &self,
        preview: &LeadPlanPreview,
    ) -> Result<MissionDetails, AppError> {
        let mut store = SqliteStore::open(&self.database)?;
        let mut loaded = observe_runs(&mut store, &self.worktrees, preview.mission_id)?;
        let binding = store
            .mission_lead(preview.mission_id)?
            .ok_or(StoreError::LeadProposalStale(preview.mission_id))?;
        let answer = mission_lead::latest_answer(&mut store, binding.run_id)?
            .ok_or(StoreError::LeadProposalStale(preview.mission_id))?;
        if loaded.revision != preview.approval.mission_revision
            || answer.run_id != preview.approval.run_id
            || answer.stage_id != preview.approval.stage_id
            || sha256_hex(answer.text.as_bytes()) != preview.approval.answer_sha256
            || answer.proposals.as_ref().ok() != Some(&preview.changes)
        {
            return Err(StoreError::LeadProposalStale(preview.mission_id).into());
        }
        let at = now().max(*loaded.mission.updated_at());
        let change = apply_changes(&mut loaded.mission, &preview.changes, at)?;
        store.commit_mission_plan_approval(
            &loaded.mission,
            &change.events,
            &preview.approval,
            at,
        )?;
        drop(store);
        self.inspect_mission(preview.mission_id)
    }
}

pub(super) fn apply_changes(
    mission: &mut Mission,
    changes: &[PlanChange],
    now: DateTime<Utc>,
) -> Result<MissionChange, MissionError> {
    let mut events = Vec::new();
    for change in changes {
        let change = match change.clone() {
            PlanChange::AddPackage {
                id,
                contract,
                dependencies,
            } => mission.add_package(id, contract, dependencies, now)?,
            PlanChange::RevisePackage {
                id,
                title,
                goal,
                rationale,
                scope,
                acceptance_criteria,
                verification,
                workflow,
                dependencies,
            } => {
                let current = mission
                    .package(&id)
                    .ok_or_else(|| MissionError::PackageNotFound(mission.id(), id.clone()))?
                    .contract()
                    .clone();
                let contract = WorkPackageContract {
                    title: title.unwrap_or_else(|| current.title.clone()),
                    goal: goal.unwrap_or_else(|| current.goal.clone()),
                    rationale: rationale.unwrap_or_else(|| current.rationale.clone()),
                    scope: scope.unwrap_or_else(|| current.scope.clone()),
                    acceptance_criteria: acceptance_criteria
                        .unwrap_or_else(|| current.acceptance_criteria.clone()),
                    verification: verification.unwrap_or_else(|| current.verification.clone()),
                    workflow: workflow.unwrap_or(current.workflow),
                };
                let mut change = if contract == current {
                    MissionChange { events: Vec::new() }
                } else {
                    mission.revise_contract(&id, contract, now)?
                };
                if let Some(dependencies) = dependencies {
                    change
                        .events
                        .extend(mission.set_dependencies(&id, dependencies, now)?.events);
                }
                change
            }
            PlanChange::CancelPackage { id, reason } => mission.cancel_package(&id, reason, now)?,
            PlanChange::RecordDecision { title, rationale } => mission.record_decision(
                DecisionId::new(),
                title,
                rationale,
                DecisionAuthor::Lead,
                now,
            )?,
        };
        events.extend(change.events);
    }
    Ok(MissionChange { events })
}

fn render_changes(changes: &[PlanChange]) -> String {
    let mut text = String::new();
    for (index, change) in changes.iter().enumerate() {
        let _ = writeln!(text, "{}. {}", index + 1, change.describe());
        match change {
            PlanChange::AddPackage {
                contract,
                dependencies,
                ..
            } => {
                contract_fields(&mut text, contract);
                let _ = writeln!(
                    text,
                    "   Dependencies: {}",
                    dependencies
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            PlanChange::RevisePackage {
                title,
                goal,
                rationale,
                scope,
                acceptance_criteria,
                verification,
                workflow,
                dependencies,
                ..
            } => {
                for (label, value) in [
                    ("Title", title),
                    ("Goal", goal),
                    ("Why", rationale),
                    ("Scope", scope),
                    ("Verification", verification),
                ] {
                    if let Some(value) = value {
                        let _ = writeln!(text, "   {label}: {value}");
                    }
                }
                if let Some(criteria) = acceptance_criteria {
                    text.push_str("   Acceptance (replaces the current list):\n");
                    for criterion in criteria {
                        let _ = writeln!(text, "   - {criterion}");
                    }
                }
                if let Some(workflow) = workflow {
                    let _ = writeln!(text, "   Workflow: {workflow:?}");
                }
                if let Some(dependencies) = dependencies {
                    let _ = writeln!(
                        text,
                        "   Dependencies (replaces the current list): {}",
                        dependencies
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            PlanChange::CancelPackage { reason, .. } => {
                let _ = writeln!(text, "   Reason: {reason}");
            }
            PlanChange::RecordDecision { rationale, .. } => {
                let _ = writeln!(text, "   Why: {rationale}");
            }
        }
        text.push('\n');
    }
    text
}

fn contract_fields(text: &mut String, contract: &WorkPackageContract) {
    for (label, value) in [
        ("Title", &contract.title),
        ("Goal", &contract.goal),
        ("Why", &contract.rationale),
        ("Scope", &contract.scope),
        ("Verification", &contract.verification),
    ] {
        let _ = writeln!(text, "   {label}: {value}");
    }
    let _ = writeln!(text, "   Workflow: {:?}", contract.workflow);
    text.push_str("   Acceptance:\n");
    for criterion in &contract.acceptance_criteria {
        let _ = writeln!(text, "   - {criterion}");
    }
}
