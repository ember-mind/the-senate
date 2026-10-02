//! Mission persistence: immutable intent, versioned snapshot, event log,
//! and the run-to-package index.
//!
//! Mirrors the run store: state is a validated snapshot updated by
//! compare-and-swap, events commit in the same transaction as the state
//! they explain, and the immutable input lives in its own insert-only
//! table. `mission_runs` is an indexed projection of the packages' run
//! lists, kept in sync inside the same transaction so a run can be traced
//! back to its package without decoding every mission.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use sha2::{Digest, Sha256};

use crate::domain::{
    DecisionId, Mission, MissionDecision, MissionEvent, MissionEventKind, MissionId,
    MissionRehydrationData, MissionStatus, Run, RunId, StageId, WorkPackageContract, WorkPackageId,
    WorkPackageRehydrationData, WorkPackageResult, WorkPackageStatus,
};

use super::sqlite::{
    CommitResult, RunRevision, format_timestamp, i64_to_u64, parse_timestamp, u64_to_i64,
};
use super::{ResolvedConfigSnapshot, RunInput, SqliteStore, StoreError};

pub const MISSION_INPUT_SCHEMA_VERSION: u32 = 1;
pub const MISSION_SNAPSHOT_SCHEMA_VERSION: u32 = 2;

/// Immutable intent bound to one mission: what it is for and where it works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionInput {
    mission_id: MissionId,
    schema_version: u32,
    title: String,
    goal: String,
    source_repo_path: String,
    /// The source checkout's `HEAD` when the mission was created. Packages
    /// integrate on top of whatever the checkout holds later; this records
    /// where the plan began.
    base_commit: String,
    created_at: DateTime<Utc>,
}

impl MissionInput {
    /// Normalizes title and goal while preserving their content.
    ///
    /// # Errors
    /// Rejects a blank title or goal.
    pub fn new(
        mission_id: MissionId,
        title: impl Into<String>,
        goal: impl Into<String>,
        source_repo_path: impl Into<String>,
        base_commit: impl Into<String>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, MissionInputError> {
        let title = title.into().trim().to_owned();
        let goal = goal.into().trim().to_owned();
        if title.is_empty() {
            return Err(MissionInputError::EmptyTitle);
        }
        if goal.is_empty() {
            return Err(MissionInputError::EmptyGoal);
        }
        Ok(Self {
            mission_id,
            schema_version: MISSION_INPUT_SCHEMA_VERSION,
            title,
            goal,
            source_repo_path: source_repo_path.into(),
            base_commit: base_commit.into(),
            created_at,
        })
    }

    #[must_use]
    pub const fn mission_id(&self) -> MissionId {
        self.mission_id
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn goal(&self) -> &str {
        &self.goal
    }

    #[must_use]
    pub fn source_repo_path(&self) -> &str {
        &self.source_repo_path
    }

    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MissionInputError {
    #[error("mission title must not be empty")]
    EmptyTitle,
    #[error("mission goal must not be empty")]
    EmptyGoal,
    #[error("mission input schema version {0} is unsupported")]
    UnsupportedSchemaVersion(u32),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MissionRevision(u64);

impl MissionRevision {
    #[must_use]
    pub const fn initial() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedMission {
    pub mission: Mission,
    pub input: MissionInput,
    pub revision: MissionRevision,
}

/// Indexed mission projection for lists; no snapshot is decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionSummary {
    pub id: MissionId,
    pub status: MissionStatus,
    pub title: String,
    pub source_repo_path: String,
    pub revision: MissionRevision,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencedMissionEvent {
    pub sequence: u64,
    pub event: MissionEvent,
}

/// Which package of which mission one run serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionRunBinding {
    pub mission_id: MissionId,
    pub package_id: WorkPackageId,
}

/// The lead session bound to a mission: one run, kept for the mission's
/// life; a newer binding replaces it as current without deleting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionLeadBinding {
    pub mission_id: MissionId,
    pub run_id: RunId,
    pub created_at: DateTime<Utc>,
}

/// What one child run was told, bound to the mission state it came from.
///
/// The rendered text itself is the run's immutable input; this record holds
/// its hash and the hash of the contract it was rendered from, plus the
/// dependencies and decisions it named, so the handoff is checkable evidence
/// rather than a string somebody remembers sending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionHandoffRecord {
    pub run_id: RunId,
    pub mission_id: MissionId,
    pub package_id: WorkPackageId,
    pub contract_sha256: String,
    pub task_sha256: String,
    pub task_size: u64,
    pub dependencies: Vec<WorkPackageId>,
    pub decision_ids: Vec<DecisionId>,
    pub created_at: DateTime<Utc>,
}

/// The exact mission and lead answer revision a Plan Changes preview was
/// approved against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadProposalApproval {
    pub mission_revision: MissionRevision,
    pub run_id: RunId,
    pub run_revision: RunRevision,
    pub stage_id: StageId,
    pub answer_sha256: String,
}

/// SHA-256 of the contract's canonical (key-sorted, compact) JSON.
///
/// # Errors
/// Returns JSON encoding failures.
pub fn contract_sha256(contract: &WorkPackageContract) -> Result<String, StoreError> {
    let value = super::config_snapshot::canonical_value(serde_json::to_value(contract)?);
    Ok(sha256_hex(&serde_json::to_vec(&value)?))
}

/// SHA-256 of exact bytes, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hash = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = std::fmt::Write::write_fmt(&mut hash, format_args!("{byte:02x}"));
    }
    hash
}

#[derive(Debug, Serialize, Deserialize)]
struct MissionSnapshotV1 {
    schema_version: u32,
    id: MissionId,
    status: MissionStatus,
    packages: Vec<PackageSnapshotV1>,
    decisions: Vec<MissionDecision>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PackageSnapshotV1 {
    id: WorkPackageId,
    contract: WorkPackageContract,
    dependencies: Vec<WorkPackageId>,
    status: WorkPackageStatus,
    runs: Vec<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// v2 adds the delivery result. A v1 reader would drop it silently, so the
/// version moves rather than the field being optional in place.
#[derive(Debug, Serialize, Deserialize)]
struct MissionSnapshotV2 {
    schema_version: u32,
    id: MissionId,
    status: MissionStatus,
    packages: Vec<PackageSnapshotV2>,
    decisions: Vec<MissionDecision>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PackageSnapshotV2 {
    id: WorkPackageId,
    contract: WorkPackageContract,
    dependencies: Vec<WorkPackageId>,
    status: WorkPackageStatus,
    runs: Vec<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<WorkPackageResult>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<PackageSnapshotV1> for PackageSnapshotV2 {
    fn from(package: PackageSnapshotV1) -> Self {
        Self {
            id: package.id,
            contract: package.contract,
            dependencies: package.dependencies,
            status: package.status,
            runs: package.runs,
            reason: package.reason,
            result: None,
            created_at: package.created_at,
            updated_at: package.updated_at,
        }
    }
}

impl From<PackageSnapshotV2> for WorkPackageRehydrationData {
    fn from(package: PackageSnapshotV2) -> Self {
        Self {
            id: package.id,
            contract: package.contract,
            dependencies: package.dependencies,
            status: package.status,
            runs: package.runs,
            reason: package.reason,
            result: package.result,
            created_at: package.created_at,
            updated_at: package.updated_at,
        }
    }
}

fn encode_mission(mission: &Mission) -> Result<String, StoreError> {
    let snapshot = MissionSnapshotV2 {
        schema_version: MISSION_SNAPSHOT_SCHEMA_VERSION,
        id: mission.id(),
        status: mission.status(),
        packages: mission
            .packages()
            .iter()
            .map(|package| PackageSnapshotV2 {
                id: package.id().clone(),
                contract: package.contract().clone(),
                dependencies: package.dependencies().to_vec(),
                status: package.status(),
                runs: package.runs().to_vec(),
                reason: package.reason().map(str::to_owned),
                result: package.result().cloned(),
                created_at: *package.created_at(),
                updated_at: *package.updated_at(),
            })
            .collect(),
        decisions: mission.decisions().to_vec(),
        created_at: *mission.created_at(),
        updated_at: *mission.updated_at(),
    };
    Ok(serde_json::to_string(&snapshot)?)
}

fn decode_mission(snapshot_json: &str, column_version: u32) -> Result<Mission, StoreError> {
    #[derive(Deserialize)]
    struct Envelope {
        schema_version: u32,
    }
    let envelope: Envelope =
        serde_json::from_str(snapshot_json).map_err(|_| StoreError::InvalidSnapshotEnvelope)?;
    if envelope.schema_version != column_version {
        return Err(StoreError::SnapshotVersionMismatch {
            snapshot: envelope.schema_version,
            column: column_version,
        });
    }
    let snapshot = match envelope.schema_version {
        1 => {
            let snapshot: MissionSnapshotV1 = serde_json::from_str(snapshot_json)?;
            MissionSnapshotV2 {
                schema_version: MISSION_SNAPSHOT_SCHEMA_VERSION,
                id: snapshot.id,
                status: snapshot.status,
                packages: snapshot.packages.into_iter().map(Into::into).collect(),
                decisions: snapshot.decisions,
                created_at: snapshot.created_at,
                updated_at: snapshot.updated_at,
            }
        }
        MISSION_SNAPSHOT_SCHEMA_VERSION => serde_json::from_str(snapshot_json)?,
        unsupported => return Err(StoreError::UnsupportedSnapshotVersion(unsupported)),
    };
    let data = MissionRehydrationData {
        id: snapshot.id,
        status: snapshot.status,
        packages: snapshot.packages.into_iter().map(Into::into).collect(),
        decisions: snapshot.decisions,
        created_at: snapshot.created_at,
        updated_at: snapshot.updated_at,
    };
    Ok(Mission::rehydrate(data)?)
}

impl SqliteStore {
    /// Atomically inserts immutable input, the initial mission, and its
    /// creation event.
    ///
    /// # Errors
    /// Rejects invalid aggregates, identity mismatches, duplicate missions,
    /// and invalid events.
    pub fn create_mission(
        &mut self,
        mission: &Mission,
        input: &MissionInput,
        events: &[MissionEvent],
    ) -> Result<MissionRevision, StoreError> {
        mission.validate_invariants()?;
        if input.mission_id() != mission.id() {
            return Err(StoreError::SnapshotProjectionMismatch(
                "mission input belongs to another mission",
            ));
        }
        if input.created_at() != mission.created_at() {
            return Err(StoreError::SnapshotProjectionMismatch(
                "mission input created_at differs from mission",
            ));
        }
        validate_mission_events(mission, events, None)?;
        if !matches!(
            events.first().map(MissionEvent::kind),
            Some(MissionEventKind::MissionCreated)
        ) || events
            .iter()
            .skip(1)
            .any(|event| matches!(event.kind(), MissionEventKind::MissionCreated))
        {
            return Err(StoreError::InvalidInitialEvent);
        }
        let snapshot_json = encode_mission(mission)?;
        let status = status_text(mission.status())?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if mission_exists(&transaction, mission.id())? {
            return Err(StoreError::MissionAlreadyExists(mission.id()));
        }
        transaction.execute(
            "INSERT INTO missions (
                 id, status, snapshot_schema_version, snapshot_json, revision,
                 created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![
                mission.id().to_string(),
                status,
                i64::from(MISSION_SNAPSHOT_SCHEMA_VERSION),
                snapshot_json,
                format_timestamp(mission.created_at()),
                format_timestamp(mission.updated_at()),
            ],
        )?;
        transaction.execute(
            "INSERT INTO mission_inputs (
                 mission_id, schema_version, title, goal, source_repo_path, base_commit,
                 created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                input.mission_id().to_string(),
                i64::from(MISSION_INPUT_SCHEMA_VERSION),
                input.title(),
                input.goal(),
                input.source_repo_path(),
                input.base_commit(),
                format_timestamp(input.created_at()),
            ],
        )?;
        insert_mission_events(&transaction, mission, events, 1)?;
        sync_mission_runs(&transaction, mission)?;
        transaction.commit()?;
        Ok(MissionRevision::initial())
    }

    /// Loads one mission with its immutable input and current revision.
    ///
    /// # Errors
    /// Returns not-found, corrupt, or invariant-violating persisted state.
    pub fn load_mission(&mut self, mission_id: MissionId) -> Result<LoadedMission, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let row = load_mission_row(&transaction, mission_id)?;
        let mission = decode_mission(&row.snapshot_json, row.snapshot_schema_version)?;
        if mission.id() != mission_id {
            return Err(StoreError::SnapshotProjectionMismatch("mission ID"));
        }
        if status_text(mission.status())? != row.status {
            return Err(StoreError::SnapshotProjectionMismatch("mission status"));
        }
        if format_timestamp(mission.updated_at()) != row.updated_at {
            return Err(StoreError::SnapshotProjectionMismatch("mission updated_at"));
        }
        let input = load_mission_input(&transaction, mission_id)?;
        transaction.commit()?;
        Ok(LoadedMission {
            mission,
            input,
            revision: MissionRevision(row.revision),
        })
    }

    /// Indexed mission projections, newest first; no snapshot is decoded.
    ///
    /// # Errors
    /// Returns projection or `SQLite` errors.
    pub fn list_missions(&self) -> Result<Vec<MissionSummary>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT missions.id, missions.status, mission_inputs.title,
                    mission_inputs.source_repo_path, missions.revision, missions.updated_at
             FROM missions
             JOIN mission_inputs ON mission_inputs.mission_id = missions.id
             ORDER BY missions.updated_at DESC, missions.id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut summaries = Vec::new();
        for row in rows {
            let (id, status, title, source_repo_path, revision, updated_at) = row?;
            summaries.push(MissionSummary {
                id: id
                    .parse()
                    .map_err(|_| StoreError::SnapshotProjectionMismatch("mission ID"))?,
                status: status_from_text(&status)?,
                title,
                source_repo_path,
                revision: MissionRevision(i64_to_u64(revision, "mission revision")?),
                updated_at: parse_timestamp(&updated_at)?,
            });
        }
        Ok(summaries)
    }

    /// Atomically creates a child Run, records its input and initial events,
    /// and binds it with its handoff to a Mission package.
    ///
    /// # Errors
    /// Any validation, stale-revision, drive-limit, or persistence failure
    /// rolls back both the Run and Mission changes.
    #[allow(
        clippy::too_many_arguments,
        reason = "one atomic run and mission commit"
    )]
    pub fn create_mission_run_with_input(
        &mut self,
        run: &Run,
        input: &RunInput,
        config: &ResolvedConfigSnapshot,
        run_events: &[crate::domain::DomainEvent],
        mission: &Mission,
        expected_revision: MissionRevision,
        mission_events: &[MissionEvent],
        handoff: &MissionHandoffRecord,
        auto_approve: bool,
    ) -> Result<CommitResult, StoreError> {
        if handoff.run_id != run.id() || handoff.mission_id != mission.id() {
            return Err(StoreError::SnapshotProjectionMismatch(
                "mission handoff identity differs from supplied run or mission",
            ));
        }
        self.create_run_with_input_hook(run, input, config, run_events, |transaction| {
            ensure_mission_drive_capacity(transaction, mission.id())?;
            commit_mission_update_in_transaction(
                transaction,
                mission,
                expected_revision,
                mission_events,
                Some(handoff),
            )?;
            if auto_approve {
                transaction.execute(
                    "UPDATE runs SET auto_approve = 1 WHERE id = ?1",
                    [run.id().to_string()],
                )?;
            }
            Ok(())
        })
    }

    /// Atomically creates and binds a Mission's initial Consul lead Run.
    /// A current lead may be replaced only after its Run is Failed or
    /// Discarded; a concurrent caller cannot create a second active lead.
    ///
    /// # Errors
    /// Returns validation, active-lead, or persistence errors. Failure rolls
    /// back the Run, input, config, and events together.
    pub fn create_mission_lead_run_with_input(
        &mut self,
        run: &Run,
        input: &RunInput,
        config: &ResolvedConfigSnapshot,
        events: &[crate::domain::DomainEvent],
        mission_id: MissionId,
        created_at: DateTime<Utc>,
    ) -> Result<CommitResult, StoreError> {
        self.create_run_with_input_hook(run, input, config, events, |transaction| {
            bind_mission_lead_in_transaction(transaction, mission_id, run.id(), &created_at)
        })
    }

    /// Atomically updates the snapshot, appends its event batch, and keeps
    /// the run index in step, using compare-and-swap on the revision.
    ///
    /// # Errors
    /// Returns [`StoreError::MissionConcurrentModification`] when the
    /// expected revision is stale; any later failure rolls everything back.
    pub fn commit_mission_update(
        &mut self,
        mission: &Mission,
        expected_revision: MissionRevision,
        events: &[MissionEvent],
    ) -> Result<MissionRevision, StoreError> {
        self.commit_mission_update_with(mission, expected_revision, events, None)
    }

    /// [`Self::commit_mission_update`] that also records the handoff of a
    /// run the update binds, in the same transaction, so a bound run never
    /// exists without the evidence of what it was told.
    ///
    /// # Errors
    /// As [`Self::commit_mission_update`]; a handoff naming a run the
    /// mission does not bind is rejected.
    pub fn commit_mission_update_with(
        &mut self,
        mission: &Mission,
        expected_revision: MissionRevision,
        events: &[MissionEvent],
        handoff: Option<&MissionHandoffRecord>,
    ) -> Result<MissionRevision, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision = commit_mission_update_in_transaction(
            &transaction,
            mission,
            expected_revision,
            events,
            handoff,
        )?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Applies a lead proposal only while the Mission and lead Run remain at
    /// the revisions shown by the preview, and stores a receipt atomically.
    ///
    /// # Errors
    /// Returns `LeadProposalAlreadyApplied` on replay and
    /// `LeadProposalStale` if either revision or the lead binding changed.
    pub fn commit_mission_plan_approval(
        &mut self,
        mission: &Mission,
        mission_events: &[MissionEvent],
        approval: &LeadProposalApproval,
        applied_at: DateTime<Utc>,
    ) -> Result<MissionRevision, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let already_applied: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM mission_lead_applications
                 WHERE mission_id = ?1 AND run_id = ?2 AND stage_id = ?3
             )",
            params![
                mission.id().to_string(),
                approval.run_id.to_string(),
                approval.stage_id.as_str(),
            ],
            |row| row.get(0),
        )?;
        if already_applied {
            return Err(StoreError::LeadProposalAlreadyApplied {
                mission_id: mission.id(),
                stage_id: approval.stage_id.clone(),
            });
        }

        let mission_revision = load_mission_row(&transaction, mission.id())?.revision;
        let run_revision: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM runs WHERE id = ?1",
                [approval.run_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let lead_bound: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM mission_leads WHERE mission_id = ?1 AND run_id = ?2
             )",
            params![mission.id().to_string(), approval.run_id.to_string()],
            |row| row.get(0),
        )?;
        let run_is_current = run_revision
            .map(|revision| i64_to_u64(revision, "run revision"))
            .transpose()?
            .is_some_and(|revision| revision == approval.run_revision.value());
        if mission_revision != approval.mission_revision.value() || !run_is_current || !lead_bound {
            return Err(StoreError::LeadProposalStale(mission.id()));
        }

        let revision = commit_mission_update_in_transaction(
            &transaction,
            mission,
            approval.mission_revision,
            mission_events,
            None,
        )?;
        transaction.execute(
            "INSERT INTO mission_lead_applications (
                 mission_id, run_id, stage_id, answer_sha256, applied_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                mission.id().to_string(),
                approval.run_id.to_string(),
                approval.stage_id.as_str(),
                approval.answer_sha256,
                format_timestamp(&applied_at),
            ],
        )?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Whether a lead stage's Plan Changes have already been approved.
    ///
    /// # Errors
    /// Returns `SQLite` query errors.
    pub fn lead_proposals_applied(
        &self,
        mission_id: MissionId,
        run_id: RunId,
        stage_id: &StageId,
    ) -> Result<bool, StoreError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM mission_lead_applications
                 WHERE mission_id = ?1 AND run_id = ?2 AND stage_id = ?3
             )",
            params![
                mission_id.to_string(),
                run_id.to_string(),
                stage_id.as_str()
            ],
            |row| row.get(0),
        )?)
    }

    /// Every handoff recorded for one mission, oldest first.
    ///
    /// # Errors
    /// Returns projection or `SQLite` errors.
    pub fn list_mission_handoffs(
        &self,
        mission_id: MissionId,
    ) -> Result<Vec<MissionHandoffRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT run_id, package_id, contract_sha256, task_sha256, task_size,
                    dependencies_json, decision_ids_json, created_at
             FROM mission_handoffs WHERE mission_id = ?1 ORDER BY created_at, run_id",
        )?;
        let rows = statement.query_map([mission_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (run_id, package_id, contract, task, size, dependencies, decisions, created_at) =
                row?;
            records.push(MissionHandoffRecord {
                run_id: run_id
                    .parse()
                    .map_err(|_| StoreError::SnapshotProjectionMismatch("handoff run ID"))?,
                mission_id,
                package_id: WorkPackageId::new(package_id)
                    .map_err(|_| StoreError::SnapshotProjectionMismatch("handoff package ID"))?,
                contract_sha256: contract,
                task_sha256: task,
                task_size: i64_to_u64(size, "handoff task size")?,
                dependencies: serde_json::from_str(&dependencies)?,
                decision_ids: serde_json::from_str(&decisions)?,
                created_at: parse_timestamp(&created_at)?,
            });
        }
        Ok(records)
    }

    /// Every committed event of one mission in sequence order.
    ///
    /// # Errors
    /// Returns not-found, corrupt JSON, sequence gaps, or `SQLite` errors.
    pub fn load_mission_events(
        &self,
        mission_id: MissionId,
    ) -> Result<Vec<SequencedMissionEvent>, StoreError> {
        if !mission_exists(&self.connection, mission_id)? {
            return Err(StoreError::MissionNotFound(mission_id));
        }
        let mut statement = self.connection.prepare(
            "SELECT sequence, event_id, event_type, payload_json, occurred_at
             FROM mission_events WHERE mission_id = ?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map([mission_id.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut events = Vec::new();
        for (expected, row) in (1u64..).zip(rows) {
            let (sequence, event_id, event_type, payload_json, occurred_at) = row?;
            let sequence = i64_to_u64(sequence, "mission event sequence")?;
            if sequence != expected {
                return Err(StoreError::MissionEventSequenceGap {
                    mission_id,
                    expected,
                    actual: sequence,
                });
            }
            let event: MissionEvent = serde_json::from_str(&payload_json)?;
            if event.id().to_string() != event_id {
                return Err(StoreError::EventProjectionMismatch("event ID"));
            }
            if event.mission_id() != mission_id {
                return Err(StoreError::EventProjectionMismatch("mission ID"));
            }
            if event_type_text(&event)? != event_type {
                return Err(StoreError::EventProjectionMismatch("event type"));
            }
            if format_timestamp(event.occurred_at()) != occurred_at {
                return Err(StoreError::EventProjectionMismatch("occurred_at"));
            }
            events.push(SequencedMissionEvent { sequence, event });
        }
        Ok(events)
    }

    /// The package one run serves, if a mission started it.
    ///
    /// # Errors
    /// Returns projection or `SQLite` errors.
    pub fn mission_of_run(&self, run_id: RunId) -> Result<Option<MissionRunBinding>, StoreError> {
        mission_of_run(&self.connection, run_id)
    }

    /// Binds a run as the mission's lead session. Insert-only: an older
    /// lead stays on record and stops being current.
    ///
    /// # Errors
    /// Returns persistence errors, including a run already bound.
    pub fn bind_mission_lead(
        &mut self,
        mission_id: MissionId,
        run_id: RunId,
        now: &DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO mission_leads (run_id, mission_id, created_at) VALUES (?1, ?2, ?3)",
            params![
                run_id.to_string(),
                mission_id.to_string(),
                format_timestamp(now)
            ],
        )?;
        Ok(())
    }

    /// The mission's current lead session, if one was ever bound.
    ///
    /// # Errors
    /// Returns persistence errors.
    pub fn mission_lead(
        &self,
        mission_id: MissionId,
    ) -> Result<Option<MissionLeadBinding>, StoreError> {
        self.connection
            .query_row(
                "SELECT run_id, created_at FROM mission_leads
                 WHERE mission_id = ?1
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT 1",
                [mission_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(run_id, created_at)| {
                Ok(MissionLeadBinding {
                    mission_id,
                    run_id: run_id
                        .parse()
                        .map_err(|_| StoreError::SnapshotProjectionMismatch("run ID"))?,
                    created_at: parse_timestamp(&created_at)?,
                })
            })
            .transpose()
    }

    /// The mission a run serves as lead, if any.
    ///
    /// # Errors
    /// Returns persistence errors.
    pub fn mission_of_lead_run(&self, run_id: RunId) -> Result<Option<MissionId>, StoreError> {
        mission_of_lead_run(&self.connection, run_id)
    }
}

fn commit_mission_update_in_transaction(
    transaction: &Transaction<'_>,
    mission: &Mission,
    expected_revision: MissionRevision,
    events: &[MissionEvent],
    handoff: Option<&MissionHandoffRecord>,
) -> Result<MissionRevision, StoreError> {
    mission.validate_invariants()?;
    if let Some(handoff) = handoff {
        let bound = mission
            .package(&handoff.package_id)
            .is_some_and(|package| package.runs().contains(&handoff.run_id));
        if handoff.mission_id != mission.id() || !bound {
            return Err(StoreError::SnapshotProjectionMismatch(
                "handoff names a run the mission does not bind",
            ));
        }
    }
    let snapshot_json = encode_mission(mission)?;
    let status = status_text(mission.status())?;
    let next_revision = expected_revision
        .value()
        .checked_add(1)
        .ok_or(StoreError::IntegerRange("next mission revision"))?;
    if events
        .iter()
        .any(|event| matches!(event.kind(), MissionEventKind::MissionCreated))
    {
        return Err(StoreError::UnexpectedRunCreatedEvent);
    }

    let row = load_mission_row(transaction, mission.id())?;
    let current = decode_mission(&row.snapshot_json, row.snapshot_schema_version)?;
    if current.created_at() != mission.created_at() {
        return Err(StoreError::ImmutableRunFieldChanged("mission created_at"));
    }
    let last_occurred_at = last_mission_event(transaction, mission.id())?;
    validate_mission_events(mission, events, last_occurred_at.map(|(_, at)| at))?;

    let changed = transaction.execute(
        "UPDATE missions
         SET status = ?1, snapshot_schema_version = ?2, snapshot_json = ?3,
             revision = ?4, updated_at = ?5
         WHERE id = ?6 AND revision = ?7",
        params![
            status,
            i64::from(MISSION_SNAPSHOT_SCHEMA_VERSION),
            snapshot_json,
            u64_to_i64(next_revision, "next mission revision")?,
            format_timestamp(mission.updated_at()),
            mission.id().to_string(),
            u64_to_i64(expected_revision.value(), "expected mission revision")?,
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::MissionConcurrentModification {
            mission_id: mission.id(),
            expected: expected_revision.value(),
        });
    }
    let first_sequence = last_occurred_at
        .map_or(0, |(sequence, _)| sequence)
        .checked_add(1)
        .ok_or(StoreError::IntegerRange("next mission event sequence"))?;
    insert_mission_events(transaction, mission, events, first_sequence)?;
    sync_mission_runs(transaction, mission)?;
    if let Some(handoff) = handoff {
        insert_handoff(transaction, handoff)?;
    }
    Ok(MissionRevision(next_revision))
}

fn ensure_mission_drive_capacity(
    transaction: &Transaction<'_>,
    mission_id: MissionId,
) -> Result<(), StoreError> {
    let configured_limit: Option<i64> = transaction
        .query_row(
            "SELECT max_parallel FROM mission_drives WHERE mission_id = ?1",
            [mission_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(configured_limit) = configured_limit else {
        return Ok(());
    };
    let limit = usize::try_from(configured_limit)
        .map_err(|_| StoreError::IntegerRange("mission drive max_parallel"))?;
    let active: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM mission_runs AS bindings
         JOIN runs ON runs.id = bindings.run_id
         WHERE bindings.mission_id = ?1
           AND runs.status IN (
               'created', 'preparing', 'ready', 'running', 'needs_user', 'paused', 'interrupted'
           )",
        [mission_id.to_string()],
        |row| row.get(0),
    )?;
    if usize::try_from(active).map_err(|_| StoreError::IntegerRange("active mission runs"))?
        >= limit
    {
        return Err(StoreError::MissionDriveLimit { mission_id, limit });
    }
    Ok(())
}

fn bind_mission_lead_in_transaction(
    transaction: &Transaction<'_>,
    mission_id: MissionId,
    run_id: RunId,
    created_at: &DateTime<Utc>,
) -> Result<(), StoreError> {
    if !mission_exists(transaction, mission_id)? {
        return Err(StoreError::MissionNotFound(mission_id));
    }
    let current: Option<(String, String)> = transaction
        .query_row(
            "SELECT leads.run_id, runs.status
             FROM mission_leads AS leads
             JOIN runs ON runs.id = leads.run_id
             WHERE leads.mission_id = ?1
             ORDER BY leads.created_at DESC, leads.rowid DESC
             LIMIT 1",
            [mission_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((current_run, status)) = current
        && status != "failed"
        && status != "discarded"
    {
        let current_run = current_run
            .parse()
            .map_err(|_| StoreError::SnapshotProjectionMismatch("run ID"))?;
        return Err(StoreError::MissionLeadActive {
            mission_id,
            run_id: current_run,
        });
    }
    transaction.execute(
        "INSERT INTO mission_leads (run_id, mission_id, created_at) VALUES (?1, ?2, ?3)",
        params![
            run_id.to_string(),
            mission_id.to_string(),
            format_timestamp(created_at)
        ],
    )?;
    Ok(())
}

pub(crate) fn mission_of_lead_run(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<MissionId>, StoreError> {
    connection
        .query_row(
            "SELECT mission_id FROM mission_leads WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|mission_id| {
            mission_id
                .parse()
                .map_err(|_| StoreError::SnapshotProjectionMismatch("mission ID"))
        })
        .transpose()
}

pub(crate) fn mission_of_run(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<MissionRunBinding>, StoreError> {
    connection
        .query_row(
            "SELECT mission_id, package_id FROM mission_runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(mission_id, package_id)| {
            Ok(MissionRunBinding {
                mission_id: mission_id
                    .parse()
                    .map_err(|_| StoreError::SnapshotProjectionMismatch("mission ID"))?,
                package_id: WorkPackageId::new(package_id)
                    .map_err(|_| StoreError::SnapshotProjectionMismatch("package ID"))?,
            })
        })
        .transpose()
}

struct MissionRow {
    status: String,
    snapshot_schema_version: u32,
    snapshot_json: String,
    revision: u64,
    updated_at: String,
}

fn load_mission_row(
    connection: &Connection,
    mission_id: MissionId,
) -> Result<MissionRow, StoreError> {
    connection
        .query_row(
            "SELECT status, snapshot_schema_version, snapshot_json, revision, updated_at
             FROM missions WHERE id = ?1",
            [mission_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::MissionNotFound(mission_id))
        .and_then(|(status, version, snapshot_json, revision, updated_at)| {
            Ok(MissionRow {
                status,
                snapshot_schema_version: u32::try_from(version)
                    .map_err(|_| StoreError::IntegerRange("mission snapshot schema version"))?,
                snapshot_json,
                revision: i64_to_u64(revision, "mission revision")?,
                updated_at,
            })
        })
}

fn load_mission_input(
    connection: &Connection,
    mission_id: MissionId,
) -> Result<MissionInput, StoreError> {
    let row = connection
        .query_row(
            "SELECT schema_version, title, goal, source_repo_path, base_commit, created_at
             FROM mission_inputs WHERE mission_id = ?1",
            [mission_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(StoreError::MissionInputNotFound(mission_id))?;
    let (schema_version, title, goal, source_repo_path, base_commit, created_at) = row;
    let schema_version = u32::try_from(schema_version)
        .map_err(|_| StoreError::IntegerRange("mission input schema version"))?;
    if schema_version != MISSION_INPUT_SCHEMA_VERSION {
        return Err(MissionInputError::UnsupportedSchemaVersion(schema_version).into());
    }
    Ok(MissionInput::new(
        mission_id,
        title,
        goal,
        source_repo_path,
        base_commit,
        parse_timestamp(&created_at)?,
    )?)
}

fn mission_exists(connection: &Connection, mission_id: MissionId) -> Result<bool, StoreError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM missions WHERE id = ?1",
            [mission_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn last_mission_event(
    connection: &Connection,
    mission_id: MissionId,
) -> Result<Option<(u64, DateTime<Utc>)>, StoreError> {
    connection
        .query_row(
            "SELECT sequence, occurred_at FROM mission_events
             WHERE mission_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [mission_id.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(sequence, occurred_at)| {
            Ok((
                i64_to_u64(sequence, "mission event sequence")?,
                parse_timestamp(&occurred_at)?,
            ))
        })
        .transpose()
}

fn validate_mission_events(
    mission: &Mission,
    events: &[MissionEvent],
    previous: Option<DateTime<Utc>>,
) -> Result<(), StoreError> {
    if events.is_empty() {
        return Err(StoreError::EmptyEventBatch);
    }
    let mut last = previous.unwrap_or(*mission.created_at());
    for event in events {
        if event.mission_id() != mission.id() {
            return Err(StoreError::EventMissionMismatch {
                event_id: event.id(),
                expected: mission.id(),
                actual: event.mission_id(),
            });
        }
        if let Some(package_id) = event.package_id()
            && mission.package(package_id).is_none()
        {
            return Err(StoreError::EventPackageMismatch {
                event_id: event.id(),
                package_id: package_id.clone(),
            });
        }
        if event.occurred_at() < &last {
            return Err(StoreError::EventTimestampRegression {
                event_id: event.id(),
                previous: last,
                occurred_at: *event.occurred_at(),
            });
        }
        last = *event.occurred_at();
    }
    if &last != mission.updated_at() {
        return Err(StoreError::EventStateTimestampMismatch);
    }
    Ok(())
}

fn insert_mission_events(
    transaction: &Transaction<'_>,
    mission: &Mission,
    events: &[MissionEvent],
    first_sequence: u64,
) -> Result<(), StoreError> {
    for (offset, event) in events.iter().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| StoreError::IntegerRange("event offset"))?;
        let sequence = first_sequence
            .checked_add(offset)
            .ok_or(StoreError::IntegerRange("mission event sequence"))?;
        transaction.execute(
            "INSERT INTO mission_events (
                 mission_id, sequence, event_id, event_type, payload_json, occurred_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                mission.id().to_string(),
                u64_to_i64(sequence, "mission event sequence")?,
                event.id().to_string(),
                event_type_text(event)?,
                serde_json::to_string(event)?,
                format_timestamp(event.occurred_at()),
            ],
        )?;
    }
    Ok(())
}

/// Inserts an index row for every run the snapshot binds that the index
/// does not know yet, and refuses a run the index already assigns elsewhere.
/// Rows are never removed: a package keeps its run history.
fn sync_mission_runs(transaction: &Transaction<'_>, mission: &Mission) -> Result<(), StoreError> {
    for package in mission.packages() {
        for run_id in package.runs() {
            match mission_of_run(transaction, *run_id)? {
                Some(binding)
                    if binding.mission_id == mission.id()
                        && &binding.package_id == package.id() => {}
                Some(binding) => {
                    return Err(StoreError::RunBoundToMission {
                        run_id: *run_id,
                        mission_id: binding.mission_id,
                    });
                }
                None => {
                    transaction.execute(
                        "INSERT INTO mission_runs (run_id, mission_id, package_id, created_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            run_id.to_string(),
                            mission.id().to_string(),
                            package.id().as_str(),
                            format_timestamp(package.updated_at()),
                        ],
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn insert_handoff(
    transaction: &Transaction<'_>,
    handoff: &MissionHandoffRecord,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO mission_handoffs (
             run_id, mission_id, package_id, contract_sha256, task_sha256, task_size,
             dependencies_json, decision_ids_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            handoff.run_id.to_string(),
            handoff.mission_id.to_string(),
            handoff.package_id.as_str(),
            handoff.contract_sha256,
            handoff.task_sha256,
            u64_to_i64(handoff.task_size, "handoff task size")?,
            serde_json::to_string(&handoff.dependencies)?,
            serde_json::to_string(&handoff.decision_ids)?,
            format_timestamp(&handoff.created_at),
        ],
    )?;
    Ok(())
}

fn status_text(status: MissionStatus) -> Result<String, StoreError> {
    serde_json::to_value(status)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or(StoreError::SnapshotProjectionMismatch(
            "mission status did not serialize as text",
        ))
}

fn status_from_text(value: &str) -> Result<MissionStatus, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.to_owned(),
    ))?)
}

fn event_type_text(event: &MissionEvent) -> Result<String, StoreError> {
    serde_json::to_value(event.kind())?
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or(StoreError::SnapshotProjectionMismatch(
            "mission event kind did not serialize with a type tag",
        ))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, DecisionAuthor, DecisionId, DomainEvent, DomainEventKind, EventId,
        EventMetadata, Run, RunStatus, RunTransition, WorkflowDefinition, WorkflowKind,
    };
    use crate::store::{ResolvedConfigSnapshot, RunInput};

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 22, 12, 0, second).unwrap()
    }

    fn contract(title: &str) -> WorkPackageContract {
        WorkPackageContract {
            title: title.to_owned(),
            goal: format!("deliver {title}"),
            rationale: "because".to_owned(),
            scope: String::new(),
            acceptance_criteria: vec!["done".to_owned()],
            verification: String::new(),
            workflow: WorkflowKind::Fast,
        }
    }

    fn package(value: &str) -> WorkPackageId {
        WorkPackageId::new(value).unwrap()
    }

    fn new_mission(store: &mut SqliteStore) -> (Mission, MissionInput, MissionRevision) {
        let id = MissionId::from_u128(1);
        let mission = Mission::new(id, at(0));
        let input =
            MissionInput::new(id, "JEV", "persistent lives", "/repo", "abc123", at(0)).unwrap();
        let revision = store
            .create_mission(&mission, &input, &mission.created_events())
            .unwrap();
        (mission, input, revision)
    }

    fn create_run(store: &mut SqliteStore, run_id: RunId) -> Run {
        let config = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("cfg").unwrap(),
            1,
            serde_json::json!({"provider": "fake"}),
            at(0),
        )
        .unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config.id().clone(),
            at(0),
        );
        let input = RunInput::new(run_id, "task", at(0)).unwrap();
        let event = DomainEvent::new(
            EventMetadata::new(EventId::new(), at(0)),
            run_id,
            None,
            DomainEventKind::RunCreated {
                workflow: WorkflowKind::Fast,
            },
        );
        store
            .create_run_with_input(&run, &input, &config, &[event])
            .unwrap();
        run
    }

    fn run_parts(
        run_id: RunId,
        config_id: &str,
    ) -> (Run, RunInput, ResolvedConfigSnapshot, Vec<DomainEvent>) {
        let config = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new(config_id).unwrap(),
            1,
            serde_json::json!({"provider": "fake"}),
            at(0),
        )
        .unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config.id().clone(),
            at(0),
        );
        let input = RunInput::new(run_id, "task", at(0)).unwrap();
        let event = DomainEvent::new(
            EventMetadata::new(EventId::new(), at(0)),
            run_id,
            None,
            DomainEventKind::RunCreated {
                workflow: WorkflowKind::Fast,
            },
        );
        (run, input, config, vec![event])
    }

    fn handoff(
        run_id: RunId,
        mission_id: MissionId,
        package_id: WorkPackageId,
    ) -> MissionHandoffRecord {
        let title = package_id.as_str().to_ascii_uppercase();
        let contract = contract(&title);
        MissionHandoffRecord {
            run_id,
            mission_id,
            package_id,
            contract_sha256: contract_sha256(&contract).unwrap(),
            task_sha256: sha256_hex(b"task"),
            task_size: 4,
            dependencies: vec![],
            decision_ids: vec![],
            created_at: at(2),
        }
    }

    #[test]
    fn a_mission_survives_a_restart_with_its_input_packages_and_events() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, input, revision) = new_mission(&mut store);
        let change = mission
            .add_package(
                package("persistence"),
                contract("Persistence"),
                vec![],
                at(1),
            )
            .unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let change = mission
            .record_decision(
                DecisionId::from_u128(9),
                "Persist first",
                "memory needs it",
                DecisionAuthor::User,
                at(2),
            )
            .unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        assert_eq!(revision.value(), 2);

        let loaded = store.load_mission(mission.id()).unwrap();
        assert_eq!(loaded.mission, mission);
        assert_eq!(loaded.input, input);
        assert_eq!(loaded.revision, revision);
        let events = store.load_mission_events(mission.id()).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert!(matches!(
            events[3].event.kind(),
            MissionEventKind::DecisionRecorded { .. }
        ));
        let summaries = store.list_missions().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "JEV");
        assert_eq!(summaries[0].status, MissionStatus::Planning);
    }

    #[test]
    fn stale_revisions_are_rejected_and_roll_back_events() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, revision) = new_mission(&mut store);
        let mut stale = mission.clone();
        let change = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap();
        store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let change = stale
            .add_package(package("b"), contract("B"), vec![], at(1))
            .unwrap();
        let error = store
            .commit_mission_update(&stale, revision, &change.events)
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::MissionConcurrentModification { expected: 0, .. }
        ));
        assert_eq!(store.load_mission_events(mission.id()).unwrap().len(), 3);
        assert_eq!(store.load_mission(mission.id()).unwrap().mission, mission);
    }

    #[test]
    fn binding_a_run_indexes_it_once_and_refuses_a_second_owner() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let run_id = RunId::from_u128(7);
        create_run(&mut store, run_id);
        let (mut mission, _, revision) = new_mission(&mut store);
        let change = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let change = mission.start_package(&package("a"), run_id, at(2)).unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let binding = store.mission_of_run(run_id).unwrap().unwrap();
        assert_eq!(binding.mission_id, mission.id());
        assert_eq!(binding.package_id, package("a"));

        // A second commit with the same binding is idempotent.
        let change = mission
            .observe_run(
                &package("a"),
                run_id,
                RunStatus::Completed,
                None,
                Some(WorkPackageResult {
                    run_id,
                    captured_at: at(3),
                    stage_count: 1,
                    changed_files: vec![],
                    changes_complete: true,
                    bottom_line: None,
                    verification: None,
                    reviews: vec![],
                    decision: None,
                    open_questions: None,
                }),
                at(3),
            )
            .unwrap();
        store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();

        // Another mission claiming the same run is refused.
        let other_id = MissionId::from_u128(2);
        let mut other = Mission::new(other_id, at(0));
        let other_input =
            MissionInput::new(other_id, "Other", "goal", "/repo", "abc123", at(0)).unwrap();
        let other_revision = store
            .create_mission(&other, &other_input, &other.created_events())
            .unwrap();
        let change = other
            .add_package(package("x"), contract("X"), vec![], at(1))
            .unwrap();
        let other_revision = store
            .commit_mission_update(&other, other_revision, &change.events)
            .unwrap();
        let change = other.start_package(&package("x"), run_id, at(2)).unwrap();
        let error = store
            .commit_mission_update(&other, other_revision, &change.events)
            .unwrap_err();
        assert!(matches!(error, StoreError::RunBoundToMission { .. }));
        assert_eq!(
            store.load_mission(other_id).unwrap().revision,
            other_revision
        );

        // The run cannot be purged while a mission remembers it.
        let error = store.purge_run(run_id).unwrap_err();
        assert!(matches!(error, StoreError::RunBoundToMission { .. }));
    }

    #[test]
    fn initial_lead_run_and_binding_are_atomic_and_only_one_active_lead_is_allowed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mission, _, _) = new_mission(&mut store);
        let first_id = RunId::from_u128(80);
        let (first, first_input, first_config, first_events) = run_parts(first_id, "lead-first");
        store
            .create_mission_lead_run_with_input(
                &first,
                &first_input,
                &first_config,
                &first_events,
                mission.id(),
                at(1),
            )
            .unwrap();
        assert_eq!(
            store.mission_lead(mission.id()).unwrap().unwrap().run_id,
            first_id
        );
        assert_eq!(store.load_run_input(first_id).unwrap(), Some(first_input));

        let second_id = RunId::from_u128(81);
        let (second, second_input, second_config, second_events) =
            run_parts(second_id, "lead-second");
        assert!(matches!(
            store.create_mission_lead_run_with_input(
                &second,
                &second_input,
                &second_config,
                &second_events,
                mission.id(),
                at(2),
            ),
            Err(StoreError::MissionLeadActive { mission_id, run_id })
                if mission_id == mission.id() && run_id == first_id
        ));
        assert!(matches!(
            store.load_run(second_id),
            Err(StoreError::RunNotFound(id)) if id == second_id
        ));
        assert!(store.load_run_input(second_id).unwrap().is_none());
        assert!(matches!(
            store.load_config_snapshot(second_config.id()),
            Err(StoreError::ConfigSnapshotNotFound(_))
        ));
        assert_eq!(
            store.mission_lead(mission.id()).unwrap().unwrap().run_id,
            first_id
        );
    }

    #[test]
    fn a_handoff_commits_with_the_bind_and_never_for_a_run_the_mission_lacks() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let run_id = RunId::from_u128(7);
        create_run(&mut store, run_id);
        let (mut mission, _, revision) = new_mission(&mut store);
        let change = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let handoff = MissionHandoffRecord {
            run_id,
            mission_id: mission.id(),
            package_id: package("a"),
            contract_sha256: contract_sha256(&contract("A")).unwrap(),
            task_sha256: sha256_hex(b"task"),
            task_size: 4,
            dependencies: vec![],
            decision_ids: vec![DecisionId::from_u128(3)],
            created_at: at(2),
        };
        // Not bound yet: refused, nothing written.
        assert!(matches!(
            store.commit_mission_update_with(&mission, revision, &change.events, Some(&handoff)),
            Err(StoreError::SnapshotProjectionMismatch(_))
        ));
        let change = mission.start_package(&package("a"), run_id, at(2)).unwrap();
        store
            .commit_mission_update_with(&mission, revision, &change.events, Some(&handoff))
            .unwrap();
        let recorded = store.list_mission_handoffs(mission.id()).unwrap();
        assert_eq!(recorded, vec![handoff]);
        assert_eq!(
            contract_sha256(&contract("A")).unwrap(),
            contract_sha256(&WorkPackageContract {
                acceptance_criteria: vec!["done".to_owned()],
                ..contract("A")
            })
            .unwrap()
        );
        assert_ne!(
            contract_sha256(&contract("A")).unwrap(),
            contract_sha256(&contract("B")).unwrap()
        );
    }

    #[test]
    fn atomic_mission_run_creation_rolls_back_on_stale_revision() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, revision) = new_mission(&mut store);
        let change = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let run_id = RunId::from_u128(70);
        let (run, input, config, run_events) = run_parts(run_id, "atomic-stale");
        let change = mission.start_package(&package("a"), run_id, at(2)).unwrap();
        let handoff = handoff(run_id, mission.id(), package("a"));

        let error = store
            .create_mission_run_with_input(
                &run,
                &input,
                &config,
                &run_events,
                &mission,
                MissionRevision::initial(),
                &change.events,
                &handoff,
                false,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::MissionConcurrentModification { expected: 0, .. }
        ));
        assert_eq!(store.load_mission(mission.id()).unwrap().revision, revision);
        assert!(matches!(
            store.load_run(run_id),
            Err(StoreError::RunNotFound(id)) if id == run_id
        ));
        assert!(store.load_run_input(run_id).unwrap().is_none());
        assert!(matches!(
            store.load_config_snapshot(config.id()),
            Err(StoreError::ConfigSnapshotNotFound(_))
        ));
        assert!(store.mission_of_run(run_id).unwrap().is_none());
        assert!(
            store
                .list_mission_handoffs(mission.id())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mission_run_creation_enforces_drive_limit_and_binds_everything_together() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, revision) = new_mission(&mut store);
        let mut events = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap()
            .events;
        events.extend(
            mission
                .add_package(package("b"), contract("B"), vec![], at(1))
                .unwrap()
                .events,
        );
        let revision = store
            .commit_mission_update(&mission, revision, &events)
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO mission_drives (
                     mission_id, max_parallel, policy_json, pause_reason, updated_at
                 ) VALUES (?1, 1, '{}', NULL, ?2)",
                params![mission.id().to_string(), format_timestamp(&at(1))],
            )
            .unwrap();

        let first_id = RunId::from_u128(71);
        let (first, first_input, first_config, first_events) = run_parts(first_id, "drive-first");
        let first_change = mission
            .start_package(&package("a"), first_id, at(2))
            .unwrap();
        let first_handoff = handoff(first_id, mission.id(), package("a"));
        store
            .create_mission_run_with_input(
                &first,
                &first_input,
                &first_config,
                &first_events,
                &mission,
                revision,
                &first_change.events,
                &first_handoff,
                true,
            )
            .unwrap();
        assert!(store.run_auto_approve(first_id).unwrap());
        assert_eq!(
            store.mission_of_run(first_id).unwrap().unwrap().package_id,
            package("a")
        );
        assert_eq!(
            store.list_mission_handoffs(mission.id()).unwrap(),
            vec![first_handoff.clone()]
        );

        let loaded = store.load_mission(mission.id()).unwrap();
        let mut second_mission = loaded.mission;
        let second_revision = loaded.revision;
        let second_id = RunId::from_u128(72);
        let (second, second_input, second_config, second_events) =
            run_parts(second_id, "drive-second");
        let second_change = second_mission
            .start_package(&package("b"), second_id, at(3))
            .unwrap();
        let second_handoff = handoff(second_id, mission.id(), package("b"));
        assert!(matches!(
            store.create_mission_run_with_input(
                &second,
                &second_input,
                &second_config,
                &second_events,
                &second_mission,
                second_revision,
                &second_change.events,
                &second_handoff,
                false,
            ),
            Err(StoreError::MissionDriveLimit { limit: 1, .. })
        ));
        assert!(matches!(
            store.load_run(second_id),
            Err(StoreError::RunNotFound(id)) if id == second_id
        ));
        assert!(store.mission_of_run(second_id).unwrap().is_none());
        assert_eq!(
            store.load_mission(mission.id()).unwrap().revision,
            second_revision
        );
        assert_eq!(
            store.list_mission_handoffs(mission.id()).unwrap(),
            vec![first_handoff]
        );
    }

    #[test]
    fn failed_auto_approval_write_rolls_back_run_binding_and_handoff() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, revision) = new_mission(&mut store);
        let change = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let run_id = RunId::from_u128(73);
        let (run, input, config, run_events) = run_parts(run_id, "atomic-auto-approve");
        let change = mission.start_package(&package("a"), run_id, at(2)).unwrap();
        let handoff = handoff(run_id, mission.id(), package("a"));
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_auto_approve BEFORE UPDATE OF auto_approve ON runs BEGIN SELECT RAISE(ABORT, 'injected auto-approve failure'); END;").unwrap();
        let refused = store.create_mission_run_with_input(
            &run,
            &input,
            &config,
            &run_events,
            &mission,
            revision,
            &change.events,
            &handoff,
            true,
        );
        assert!(matches!(refused, Err(StoreError::Sqlite(_))));
        assert!(matches!(
            store.load_run(run_id),
            Err(StoreError::RunNotFound(_))
        ));
        assert!(store.load_run_input(run_id).unwrap().is_none());
        assert!(store.mission_of_run(run_id).unwrap().is_none());
        assert!(
            store
                .list_mission_handoffs(mission.id())
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.load_mission(mission.id()).unwrap().revision, revision);
        store
            .connection
            .execute_batch("DROP TRIGGER fail_auto_approve;")
            .unwrap();
        store
            .create_mission_run_with_input(
                &run,
                &input,
                &config,
                &run_events,
                &mission,
                revision,
                &change.events,
                &handoff,
                true,
            )
            .unwrap();
        assert!(store.run_auto_approve(run_id).unwrap());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one approval transaction sequence and rollback assertions"
    )]
    fn plan_approval_receipt_is_atomic_idempotency_guard_and_rejects_stale_preview() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, revision) = new_mission(&mut store);
        let lead_id = RunId::from_u128(73);
        create_run(&mut store, lead_id);
        store
            .bind_mission_lead(mission.id(), lead_id, &at(0))
            .unwrap();
        let approval = LeadProposalApproval {
            mission_revision: revision,
            run_id: lead_id,
            run_revision: RunRevision::initial(),
            stage_id: StageId::new("lead_plan").unwrap(),
            answer_sha256: sha256_hex(b"proposal"),
        };
        let first_change = mission
            .record_decision(
                DecisionId::from_u128(74),
                "Approved direction",
                "Matches the agreed scope",
                DecisionAuthor::User,
                at(1),
            )
            .unwrap();
        assert_eq!(
            store
                .commit_mission_plan_approval(&mission, &first_change.events, &approval, at(2))
                .unwrap()
                .value(),
            1
        );
        assert!(
            store
                .lead_proposals_applied(mission.id(), lead_id, &approval.stage_id)
                .unwrap()
        );
        assert!(matches!(
            store.commit_mission_plan_approval(&mission, &first_change.events, &approval, at(3)),
            Err(StoreError::LeadProposalAlreadyApplied { .. })
        ));

        let stale_approval = LeadProposalApproval {
            stage_id: StageId::new("another_lead_plan").unwrap(),
            ..approval
        };
        let stale_change = mission
            .record_decision(
                DecisionId::from_u128(75),
                "Another change",
                "Would make the preview stale",
                DecisionAuthor::User,
                at(4),
            )
            .unwrap();
        assert!(matches!(
            store.commit_mission_plan_approval(
                &mission,
                &stale_change.events,
                &stale_approval,
                at(5)
            ),
            Err(StoreError::LeadProposalStale(id)) if id == mission.id()
        ));
        assert_eq!(
            store.load_mission(mission.id()).unwrap().revision.value(),
            1
        );
        assert_eq!(store.load_mission_events(mission.id()).unwrap().len(), 2);
        let receipts: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM mission_lead_applications WHERE mission_id = ?1",
                [mission.id().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipts, 1);

        let mut next_candidate = store.load_mission(mission.id()).unwrap().mission;
        let next_change = next_candidate
            .record_decision(
                DecisionId::from_u128(76),
                "Receipt transaction",
                "Its failure must roll back the decision too",
                DecisionAuthor::User,
                at(6),
            )
            .unwrap();
        let next_approval = LeadProposalApproval {
            mission_revision: MissionRevision(1),
            run_id: lead_id,
            run_revision: RunRevision::initial(),
            stage_id: StageId::new("receipt_failure").unwrap(),
            answer_sha256: sha256_hex(b"receipt failure proposal"),
        };
        store
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER senate_test_fail_lead_receipt
                 BEFORE INSERT ON mission_lead_applications
                 BEGIN
                   SELECT RAISE(ABORT, 'injected lead receipt failure');
                 END;",
            )
            .unwrap();
        assert!(
            store
                .commit_mission_plan_approval(
                    &next_candidate,
                    &next_change.events,
                    &next_approval,
                    at(7)
                )
                .is_err()
        );
        store
            .connection
            .execute_batch("DROP TRIGGER senate_test_fail_lead_receipt;")
            .unwrap();
        assert_eq!(
            store.load_mission(mission.id()).unwrap().revision.value(),
            1
        );
        assert_eq!(store.load_mission_events(mission.id()).unwrap().len(), 2);
        assert!(
            !store
                .lead_proposals_applied(mission.id(), lead_id, &next_approval.stage_id)
                .unwrap()
        );
    }

    #[test]
    fn plan_approval_rejects_a_lead_run_that_advanced_after_preview() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, mission_revision) = new_mission(&mut store);
        let lead_id = RunId::from_u128(82);
        create_run(&mut store, lead_id);
        store
            .bind_mission_lead(mission.id(), lead_id, &at(0))
            .unwrap();
        let loaded_run = store.load_run(lead_id).unwrap();
        let mut advanced_run = loaded_run.run;
        let event = advanced_run
            .transition(
                RunTransition::BeginPreparation,
                EventMetadata::new(EventId::new(), at(1)),
            )
            .unwrap();
        let new_run_revision = store
            .commit_run_update(&advanced_run, loaded_run.revision, &[event])
            .unwrap()
            .revision();

        let change = mission
            .record_decision(
                DecisionId::from_u128(83),
                "Stale lead turn",
                "The lead advanced while this preview was open",
                DecisionAuthor::User,
                at(2),
            )
            .unwrap();
        let approval = LeadProposalApproval {
            mission_revision,
            run_id: lead_id,
            run_revision: RunRevision::initial(),
            stage_id: StageId::new("lead_plan").unwrap(),
            answer_sha256: sha256_hex(b"old answer"),
        };
        assert_ne!(new_run_revision, approval.run_revision);
        assert!(matches!(
            store.commit_mission_plan_approval(&mission, &change.events, &approval, at(3)),
            Err(StoreError::LeadProposalStale(id)) if id == mission.id()
        ));
        assert_eq!(
            store.load_mission(mission.id()).unwrap().revision,
            mission_revision
        );
        assert_eq!(store.load_mission_events(mission.id()).unwrap().len(), 1);
        assert!(
            !store
                .lead_proposals_applied(mission.id(), lead_id, &approval.stage_id)
                .unwrap()
        );
    }

    #[test]
    fn a_v1_snapshot_still_loads_without_a_result() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mission, _, _) = new_mission(&mut store);
        let v1 = serde_json::json!({
            "schema_version": 1,
            "id": mission.id().to_string(),
            "status": "planning",
            "packages": [{
                "id": "a",
                "contract": {"title": "A", "goal": "g", "workflow": "fast"},
                "dependencies": [],
                "status": "ready",
                "runs": [],
                "created_at": "2026-09-22T12:00:00Z",
                "updated_at": "2026-09-22T12:00:00Z"
            }],
            "decisions": [],
            "created_at": "2026-09-22T12:00:00Z",
            "updated_at": "2026-09-22T12:00:00Z"
        });
        store
            .connection
            .execute(
                "UPDATE missions SET snapshot_json = ?1, snapshot_schema_version = 1 WHERE id = ?2",
                params![v1.to_string(), mission.id().to_string()],
            )
            .unwrap();
        let loaded = store.load_mission(mission.id()).unwrap();
        let package = loaded.mission.package(&package("a")).unwrap();
        assert_eq!(package.status(), WorkPackageStatus::Ready);
        assert!(package.result().is_none());
    }

    #[test]
    fn a_run_a_mission_binds_must_exist() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, revision) = new_mission(&mut store);
        let change = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap();
        let revision = store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
        let change = mission
            .start_package(&package("a"), RunId::from_u128(404), at(2))
            .unwrap();
        assert!(
            store
                .commit_mission_update(&mission, revision, &change.events)
                .is_err()
        );
    }

    #[test]
    fn corrupt_snapshots_never_produce_a_mission() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mission, _, _) = new_mission(&mut store);
        store
            .connection
            .execute(
                "UPDATE missions SET snapshot_json = ?1 WHERE id = ?2",
                params![
                    r#"{"schema_version":1,"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","status":"completed","packages":[],"decisions":[],"created_at":"2026-09-22T12:00:00Z","updated_at":"2026-09-22T12:00:00Z"}"#,
                    mission.id().to_string()
                ],
            )
            .unwrap();
        assert!(store.load_mission(mission.id()).is_err());
    }

    #[test]
    fn event_batches_must_belong_to_the_mission_and_its_packages() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (mut mission, _, revision) = new_mission(&mut store);
        let change = mission
            .add_package(package("a"), contract("A"), vec![], at(1))
            .unwrap();
        let foreign = MissionEvent::new(
            EventMetadata::new(EventId::new(), at(1)),
            MissionId::from_u128(99),
            None,
            MissionEventKind::PackageAdded,
        );
        assert!(matches!(
            store.commit_mission_update(&mission, revision, &[foreign]),
            Err(StoreError::EventMissionMismatch { .. })
        ));
        let unknown_package = MissionEvent::new(
            EventMetadata::new(EventId::new(), at(1)),
            mission.id(),
            Some(package("ghost")),
            MissionEventKind::PackageAdded,
        );
        assert!(matches!(
            store.commit_mission_update(&mission, revision, &[unknown_package]),
            Err(StoreError::EventPackageMismatch { .. })
        ));
        assert!(matches!(
            store.commit_mission_update(&mission, revision, &[]),
            Err(StoreError::EmptyEventBatch)
        ));
        store
            .commit_mission_update(&mission, revision, &change.events)
            .unwrap();
    }
}
