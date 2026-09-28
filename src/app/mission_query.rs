//! Mission-facing read models: what a list, a detail screen, or a lead
//! needs to know without touching the aggregate or the store directly.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::domain::{
    DecisionAuthor, DecisionId, Mission, MissionAttention, MissionId, MissionStatus, RunId,
    RunStatus, WorkPackage, WorkPackageId, WorkPackageResult, WorkPackageStatus, WorkflowKind,
};
use crate::store::{LoadedMission, MissionHandoffRecord, MissionRevision, SqliteStore};

use super::AppError;
use super::mission_lead::{self, LeadSummary};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionListItem {
    pub id: MissionId,
    pub title: String,
    pub status: MissionStatus,
    pub repository: PathBuf,
    pub packages: usize,
    pub integrated: usize,
    /// Packages a run is currently serving.
    pub active: usize,
    /// Packages that need the user: blocked, failed, or awaiting integration.
    pub attention: usize,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionSummary {
    pub id: DecisionId,
    pub title: String,
    pub rationale: String,
    pub author: DecisionAuthor,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPackageSummary {
    pub id: WorkPackageId,
    pub title: String,
    pub goal: String,
    pub rationale: String,
    pub scope: String,
    pub acceptance_criteria: Vec<String>,
    pub verification: String,
    pub workflow: WorkflowKind,
    pub status: WorkPackageStatus,
    pub dependencies: Vec<WorkPackageId>,
    pub runs: Vec<RunId>,
    pub current_run: Option<RunId>,
    /// Committed status of the current run, read at projection time.
    pub run_status: Option<RunStatus>,
    pub reason: Option<String>,
    /// Evidence captured when the current run delivered.
    pub result: Option<WorkPackageResult>,
    /// What the current run was told, when the mission rendered its task.
    pub handoff: Option<HandoffSummary>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The recorded handoff of one run, with whether the package's contract
/// still hashes to what the run was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffSummary {
    pub run_id: RunId,
    pub task_sha256: String,
    pub task_size: u64,
    pub decisions_named: usize,
    /// False when the contract was revised after the handoff, which cannot
    /// happen through the aggregate but is the fact this record exists to
    /// check.
    pub contract_current: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionDetails {
    pub id: MissionId,
    pub title: String,
    pub goal: String,
    pub repository: PathBuf,
    pub base_commit: String,
    pub status: MissionStatus,
    /// Packages in dependency order.
    pub packages: Vec<WorkPackageSummary>,
    pub decisions: Vec<DecisionSummary>,
    pub attention: MissionAttention,
    /// The mission's lead session, once one has been asked.
    pub lead: Option<LeadSummary>,
    pub revision: MissionRevision,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MissionDetails {
    #[must_use]
    pub fn package(&self, id: &WorkPackageId) -> Option<&WorkPackageSummary> {
        self.packages.iter().find(|package| &package.id == id)
    }
}

/// One list row per mission. `load` is the caller's way of getting each
/// mission current (the service observes its runs first), so the counts a
/// row shows are never behind the runs.
pub(crate) fn list(
    store: &mut SqliteStore,
    load: impl Fn(&mut SqliteStore, MissionId) -> Result<LoadedMission, AppError>,
) -> Result<Vec<MissionListItem>, AppError> {
    let mut items = Vec::new();
    for summary in store.list_missions()? {
        let loaded = load(store, summary.id)?;
        let mission = &loaded.mission;
        items.push(MissionListItem {
            id: summary.id,
            title: summary.title,
            status: mission.status(),
            repository: PathBuf::from(summary.source_repo_path),
            packages: mission.packages().len(),
            integrated: count(mission, |status| status == WorkPackageStatus::Integrated),
            active: count(mission, WorkPackageStatus::has_active_run),
            attention: mission.attention().len(),
            updated_at: *mission.updated_at(),
        });
    }
    Ok(items)
}

fn count(mission: &Mission, predicate: impl Fn(WorkPackageStatus) -> bool) -> usize {
    mission
        .packages()
        .iter()
        .filter(|package| predicate(package.status()))
        .count()
}

pub(crate) fn details(
    store: &mut SqliteStore,
    loaded: &LoadedMission,
) -> Result<MissionDetails, AppError> {
    let mission = &loaded.mission;
    let handoffs = store.list_mission_handoffs(mission.id())?;
    let mut packages = Vec::with_capacity(mission.packages().len());
    for id in mission.topological_order() {
        let Some(package) = mission.package(&id) else {
            continue;
        };
        packages.push(package_summary(store, package, &handoffs)?);
    }
    Ok(MissionDetails {
        id: mission.id(),
        title: loaded.input.title().to_owned(),
        goal: loaded.input.goal().to_owned(),
        repository: PathBuf::from(loaded.input.source_repo_path()),
        base_commit: loaded.input.base_commit().to_owned(),
        status: mission.status(),
        packages,
        decisions: mission
            .decisions()
            .iter()
            .map(|decision| DecisionSummary {
                id: decision.id,
                title: decision.title.clone(),
                rationale: decision.rationale.clone(),
                author: decision.author,
                recorded_at: decision.recorded_at,
            })
            .collect(),
        attention: mission.attention(),
        lead: match store.mission_lead(mission.id())? {
            Some(binding) => Some(mission_lead::summary(store, binding.run_id)?),
            None => None,
        },
        revision: loaded.revision,
        created_at: *mission.created_at(),
        updated_at: *mission.updated_at(),
    })
}

fn package_summary(
    store: &mut SqliteStore,
    package: &WorkPackage,
    handoffs: &[MissionHandoffRecord],
) -> Result<WorkPackageSummary, AppError> {
    let run_status = match package.current_run() {
        Some(run_id) => Some(store.load_run(run_id)?.run.status()),
        None => None,
    };
    let contract = package.contract();
    let handoff = package
        .current_run()
        .and_then(|run_id| handoffs.iter().find(|handoff| handoff.run_id == run_id))
        .map(|handoff| {
            Ok::<_, AppError>(HandoffSummary {
                run_id: handoff.run_id,
                task_sha256: handoff.task_sha256.clone(),
                task_size: handoff.task_size,
                decisions_named: handoff.decision_ids.len(),
                contract_current: crate::store::contract_sha256(contract)?
                    == handoff.contract_sha256,
                created_at: handoff.created_at,
            })
        })
        .transpose()?;
    Ok(WorkPackageSummary {
        id: package.id().clone(),
        title: contract.title.clone(),
        goal: contract.goal.clone(),
        rationale: contract.rationale.clone(),
        scope: contract.scope.clone(),
        acceptance_criteria: contract.acceptance_criteria.clone(),
        verification: contract.verification.clone(),
        workflow: contract.workflow,
        status: package.status(),
        dependencies: package.dependencies().to_vec(),
        runs: package.runs().to_vec(),
        current_run: package.current_run(),
        run_status,
        reason: package.reason().map(str::to_owned),
        result: package.result().cloned(),
        handoff,
        created_at: *package.created_at(),
        updated_at: *package.updated_at(),
    })
}
