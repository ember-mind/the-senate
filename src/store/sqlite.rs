use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;

use crate::domain::{
    ConfigSnapshotId, DomainEvent, DomainEventKind, Run, RunId, RunStatus, WorkflowKind,
};

use super::migrations;
use super::snapshot::{decode_run, encode_run};
use super::{RUN_SNAPSHOT_SCHEMA_VERSION, ResolvedConfigSnapshot, RunInput, StoreError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunRevision(u64);

impl RunRevision {
    #[must_use]
    pub const fn initial() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitResult {
    revision: RunRevision,
    last_sequence: u64,
}

impl CommitResult {
    #[must_use]
    pub const fn revision(self) -> RunRevision {
        self.revision
    }

    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedRun {
    pub run: Run,
    pub revision: RunRevision,
    pub config_snapshot: ResolvedConfigSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub id: RunId,
    pub status: RunStatus,
    pub workflow: WorkflowKind,
    pub task: Option<String>,
    pub repository_path: Option<String>,
    pub revision: RunRevision,
    pub updated_at: DateTime<Utc>,
    /// Operator-facing visibility; an archived run is left out of the
    /// default Runs list but stays fully intact until it is purged.
    pub archived: bool,
    /// Whether the operator armed this run to approve grantable permission
    /// requests without being asked each time.
    pub auto_approve: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencedEvent {
    pub sequence: u64,
    pub event: DomainEvent,
}

pub struct SqliteStore {
    pub(crate) connection: Connection,
}

impl SqliteStore {
    #[cfg(test)]
    pub(crate) fn install_event_insert_failure(&self) {
        self.connection
            .execute_batch(
                "CREATE TEMP TRIGGER senate_test_fail_event_insert
                 BEFORE INSERT ON events
                 BEGIN
                   SELECT RAISE(ABORT, 'injected event insert failure');
                 END;",
            )
            .expect("test failure trigger should install");
    }

    #[cfg(test)]
    pub(crate) fn remove_event_insert_failure(&self) {
        self.connection
            .execute_batch("DROP TRIGGER senate_test_fail_event_insert;")
            .expect("test failure trigger should be removed");
    }

    /// Opens one database, creating its parent directory and applying migrations.
    ///
    /// # Errors
    /// Returns typed filesystem, `SQLite`, or migration errors.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        // Reads open the store afresh every time — the interface does so
        // several times a second — and only the very first of those can find
        // the directory missing. Creating it is a filesystem round trip, so it
        // is asked for only when the database is not already sitting there.
        if !path.exists()
            && let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Self::initialize(connection)
    }

    /// Opens a process-local database with the same schema and constraints.
    ///
    /// # Errors
    /// Returns typed `SQLite` or migration errors.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(connection: Connection) -> Result<Self, StoreError> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        // Journal mode is a property of the database file, not of this
        // connection, and setting it takes a lock on a file other connections
        // may be writing to. Every open after the first would be paying for a
        // change that has already happened, so ask before telling.
        //
        // Two connections switching a fresh file at once can collide in a way
        // SQLite reports as busy at once rather than waiting out the busy
        // timeout (it would otherwise deadlock), so the switch is retried
        // within the same five seconds.
        let deadline = Instant::now() + Duration::from_secs(5);
        // An in-memory database answers `memory` whatever it is told, so
        // one accepted request ends the loop.
        while !journal_mode_is_wal(&connection)? {
            match connection.pragma_update(None, "journal_mode", "WAL") {
                Ok(()) => break,
                Err(error) if is_busy(&error) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(error) => return Err(error.into()),
            }
        }
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        // Left to itself the write-ahead log only truncates at a thousand
        // pages, so it sat at roughly the size of the database it fronts.
        // Checkpointing sooner keeps it to about a megabyte, which for a store
        // this small is the whole of it.
        connection.pragma_update(None, "wal_autocheckpoint", 256)?;
        migrations::migrate(&connection)?;
        Ok(Self { connection })
    }

    /// The file behind this store, or `None` for an in-memory database.
    #[must_use]
    pub fn database_path(&self) -> Option<PathBuf> {
        self.connection
            .path()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    }

    /// Returns applied `SQLite` schema version.
    ///
    /// # Errors
    /// Returns a `SQLite` error when the pragma cannot be read.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// Inserts immutable resolved configuration, allowing exact idempotent insert.
    ///
    /// # Errors
    /// Rejects a reused ID with different content.
    pub fn insert_config_snapshot(
        &mut self,
        snapshot: &ResolvedConfigSnapshot,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_config(&transaction, snapshot)?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads and revalidates immutable resolved configuration.
    ///
    /// # Errors
    /// Returns not-found, malformed JSON, invalid hash, timestamp, or `SQLite` errors.
    pub fn load_config_snapshot(
        &self,
        id: &ConfigSnapshotId,
    ) -> Result<ResolvedConfigSnapshot, StoreError> {
        load_config(&self.connection, id)
    }

    /// Atomically inserts config, initial run snapshot, and initial event batch.
    ///
    /// # Errors
    /// Rejects invalid aggregates, mismatched config, duplicate run, or invalid events.
    pub fn create_run(
        &mut self,
        run: &Run,
        config_snapshot: &ResolvedConfigSnapshot,
        events: &[DomainEvent],
    ) -> Result<CommitResult, StoreError> {
        self.create_run_internal(run, config_snapshot, None, events, |_| Ok(()))
    }

    /// Atomically inserts immutable input, config, initial run, and events.
    ///
    /// # Errors
    /// Rejects identity mismatches, invalid state, duplicate run, or invalid events.
    pub fn create_run_with_input(
        &mut self,
        run: &Run,
        input: &RunInput,
        config_snapshot: &ResolvedConfigSnapshot,
        events: &[DomainEvent],
    ) -> Result<CommitResult, StoreError> {
        self.create_run_internal(run, config_snapshot, Some(input), events, |_| Ok(()))
    }

    /// Atomically inserts a run and input, then lets a related store projection
    /// commit on the same write transaction before the run becomes visible.
    ///
    /// The hook runs after config, run, input, and initial events are inserted.
    /// Returning an error rolls all of them back.
    ///
    /// # Errors
    /// As [`Self::create_run_with_input`], plus any error returned by `after_insert`.
    pub(crate) fn create_run_with_input_hook(
        &mut self,
        run: &Run,
        input: &RunInput,
        config_snapshot: &ResolvedConfigSnapshot,
        events: &[DomainEvent],
        after_insert: impl FnOnce(&Transaction<'_>) -> Result<(), StoreError>,
    ) -> Result<CommitResult, StoreError> {
        self.create_run_internal(run, config_snapshot, Some(input), events, after_insert)
    }

    fn create_run_internal(
        &mut self,
        run: &Run,
        config_snapshot: &ResolvedConfigSnapshot,
        input: Option<&RunInput>,
        events: &[DomainEvent],
        after_insert: impl FnOnce(&Transaction<'_>) -> Result<(), StoreError>,
    ) -> Result<CommitResult, StoreError> {
        run.validate_invariants()?;
        if run.config_snapshot_id() != config_snapshot.id() {
            return Err(StoreError::SnapshotProjectionMismatch(
                "run config_snapshot_id differs from supplied config",
            ));
        }
        if let Some(input) = input {
            if input.run_id() != run.id() {
                return Err(StoreError::SnapshotProjectionMismatch(
                    "run input belongs to another run",
                ));
            }
            if input.created_at() != run.created_at() {
                return Err(StoreError::SnapshotProjectionMismatch(
                    "run input created_at differs from run",
                ));
            }
        }
        validate_event_batch(run, events, None)?;
        if !matches!(
            events.first().map(DomainEvent::kind),
            Some(DomainEventKind::RunCreated { workflow }) if *workflow == run.workflow_kind()
        ) || events
            .iter()
            .skip(1)
            .any(|event| matches!(event.kind(), DomainEventKind::RunCreated { .. }))
        {
            return Err(StoreError::InvalidInitialEvent);
        }
        let snapshot_json = encode_run(run)?;
        let status = enum_text(&run.status())?;
        let workflow = enum_text(&run.workflow_kind())?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if run_exists(&transaction, run.id())? {
            return Err(StoreError::RunAlreadyExists(run.id()));
        }
        insert_config(&transaction, config_snapshot)?;
        transaction.execute(
            "INSERT INTO runs (
                 id, status, workflow, config_snapshot_id, snapshot_schema_version,
                 snapshot_json, revision, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
            params![
                run.id().to_string(),
                status,
                workflow,
                run.config_snapshot_id().as_str(),
                i64::from(RUN_SNAPSHOT_SCHEMA_VERSION),
                snapshot_json,
                format_timestamp(run.created_at()),
                format_timestamp(run.updated_at()),
            ],
        )?;
        if let Some(input) = input {
            insert_run_input(&transaction, input)?;
        }
        insert_events(&transaction, run, events, 1)?;
        after_insert(&transaction)?;
        transaction.commit()?;

        Ok(CommitResult {
            revision: RunRevision::initial(),
            last_sequence: u64::try_from(events.len())
                .map_err(|_| StoreError::IntegerRange("event count"))?,
        })
    }

    /// Loads immutable user input. `None` identifies a pre-v3 legacy run.
    ///
    /// # Errors
    /// Returns malformed input, timestamp, identity, or `SQLite` errors.
    pub fn load_run_input(&self, run_id: RunId) -> Result<Option<RunInput>, StoreError> {
        load_run_input_optional(&self.connection, run_id)
    }

    /// Loads one snapshot, validates projections, then calls `Run::rehydrate`.
    ///
    /// # Errors
    /// Invalid or corrupt persisted state never produces a `Run`.
    pub fn load_run(&mut self, run_id: RunId) -> Result<LoadedRun, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let row = load_run_row(&transaction, run_id)?;
        let config_id = ConfigSnapshotId::new(row.config_snapshot_id.clone())
            .map_err(|_| StoreError::SnapshotProjectionMismatch("invalid config snapshot ID"))?;
        let config_snapshot = load_config(&transaction, &config_id)?;
        let run = decode_run(
            &row.snapshot_json,
            i64_to_u32(row.snapshot_schema_version, "snapshot schema version")?,
        )?;
        validate_run_projection(&run, &row)?;
        let revision = RunRevision(i64_to_u64(row.revision, "run revision")?);
        transaction.commit()?;
        Ok(LoadedRun {
            run,
            revision,
            config_snapshot,
        })
    }

    /// Returns lightweight indexed run projections without decoding snapshots.
    ///
    /// # Errors
    /// Returns typed projection, timestamp, or `SQLite` errors.
    pub fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT runs.id, runs.status, runs.workflow, run_inputs.task,
                    run_workspaces.source_repo_path, runs.revision, runs.updated_at,
                    runs.archived, runs.auto_approve
             FROM runs
             LEFT JOIN run_inputs ON run_inputs.run_id = runs.id
             LEFT JOIN run_workspaces ON run_workspaces.run_id = runs.id
             ORDER BY runs.updated_at DESC, runs.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, bool>(7)?,
                row.get::<_, bool>(8)?,
            ))
        })?;
        let mut summaries = Vec::new();
        for row in rows {
            let (
                id,
                status,
                workflow,
                task,
                repository_path,
                revision,
                updated_at,
                archived,
                auto_approve,
            ) = row?;
            summaries.push(RunSummary {
                id: id
                    .parse()
                    .map_err(|_| StoreError::SnapshotProjectionMismatch("invalid run ID"))?,
                status: enum_from_text(&status)?,
                workflow: enum_from_text(&workflow)?,
                task,
                repository_path,
                revision: RunRevision(i64_to_u64(revision, "run revision")?),
                updated_at: parse_timestamp(&updated_at)?,
                archived,
                auto_approve,
            });
        }
        Ok(summaries)
    }

    /// Sets the operator-facing visibility flag. Deliberately outside the
    /// CAS revision protocol: archiving is list metadata, not a run
    /// mutation, so it must neither bump the revision nor touch
    /// `updated_at`.
    ///
    /// # Errors
    /// Returns [`StoreError::RunNotFound`] for an unknown run, or `SQLite`
    /// errors.
    pub fn set_run_archived(&mut self, run_id: RunId, archived: bool) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE runs SET archived = ?1 WHERE id = ?2",
            rusqlite::params![archived, run_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::RunNotFound(run_id));
        }
        Ok(())
    }

    /// Arms or disarms automatic approval for one run.
    ///
    /// Outside the CAS revision protocol for the same reason as archiving:
    /// it records an operator's standing instruction about a run, not a
    /// change to what the run has done.
    ///
    /// # Errors
    /// Returns [`StoreError::RunNotFound`] for an unknown run, or `SQLite`
    /// errors.
    pub fn set_run_auto_approve(
        &mut self,
        run_id: RunId,
        auto_approve: bool,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE runs SET auto_approve = ?1 WHERE id = ?2",
            rusqlite::params![auto_approve, run_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::RunNotFound(run_id));
        }
        Ok(())
    }

    /// Whether this run is armed to approve permission requests by itself.
    ///
    /// # Errors
    /// Returns [`StoreError::RunNotFound`] for an unknown run, or `SQLite`
    /// errors.
    pub fn run_auto_approve(&self, run_id: RunId) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT auto_approve FROM runs WHERE id = ?1",
                rusqlite::params![run_id.to_string()],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::RunNotFound(run_id),
                other => other.into(),
            })
    }

    /// Deletes every row the run owns, in one transaction.
    ///
    /// The only path in The Senate that removes persisted history, and the
    /// end of the record rather than an edit of it: the tables it clears are
    /// insert-only precisely so that no smaller deletion exists. Child rows
    /// go before the run so the `ON DELETE RESTRICT` foreign keys stay
    /// satisfied at every step, and the whole thing is one transaction, so a
    /// failure part-way leaves the run exactly as it was. The run's config
    /// snapshot is shared with other runs and is never touched.
    ///
    /// Caller's duty, not this one's: the run must already be finished, its
    /// processes cleaned up, its workspace removed, and its files deleted —
    /// once the rows are gone, nothing remembers where those lived.
    ///
    /// # Errors
    /// Returns [`StoreError::RunNotFound`] for an unknown run, or `SQLite`
    /// errors.
    pub fn purge_run(&mut self, run_id: RunId) -> Result<(), StoreError> {
        let id = run_id.to_string();
        let transaction = self.connection.transaction()?;
        if let Some(binding) = super::mission::mission_of_run(&transaction, run_id)? {
            return Err(StoreError::RunBoundToMission {
                run_id,
                mission_id: binding.mission_id,
            });
        }
        if let Some(mission_id) = super::mission::mission_of_lead_run(&transaction, run_id)? {
            return Err(StoreError::RunBoundToMission { run_id, mission_id });
        }
        // Deepest dependents first: provider sessions point at managed
        // processes, apply operations point at workspaces, and everything
        // points at the run.
        for table in [
            "provider_sessions",
            "managed_processes",
            "run_apply_operations",
            "image_generations",
            "artifacts",
            "events",
            "run_inputs",
            "run_workspaces",
        ] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE run_id = ?1"),
                rusqlite::params![id],
            )?;
        }
        let changed = transaction.execute(
            "DELETE FROM runs WHERE id = ?1",
            rusqlite::params![id.clone()],
        )?;
        if changed == 0 {
            return Err(StoreError::RunNotFound(run_id));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomically updates snapshot and appends semantic events using CAS revision.
    ///
    /// # Errors
    /// Returns `ConcurrentModification` when expected revision is stale. Any
    /// later event failure rolls the state update back.
    pub fn commit_run_update(
        &mut self,
        run: &Run,
        expected_revision: RunRevision,
        events: &[DomainEvent],
    ) -> Result<CommitResult, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result =
            commit_run_update_transaction(&transaction, run, expected_revision, events, false)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Atomically rechecks execution infrastructure, updates run state, and
    /// appends its semantic event batch.
    ///
    /// # Errors
    /// Rejects missing/non-ready workspaces, active apply intent, stale run
    /// revisions, invalid aggregates/events, and `SQLite` failures.
    pub(crate) fn commit_run_execution_update(
        &mut self,
        run: &Run,
        expected_revision: RunRevision,
        events: &[DomainEvent],
    ) -> Result<CommitResult, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let workspace_status = transaction
            .query_row(
                "SELECT status FROM run_workspaces WHERE run_id = ?1",
                [run.id().to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if workspace_status.as_deref() != Some("ready") {
            return Err(StoreError::ExecutionWorkspaceNotReady {
                run_id: run.id(),
                status: workspace_status,
            });
        }
        let result =
            commit_run_update_transaction(&transaction, run, expected_revision, events, false)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Loads authoritative per-run event sequence and validates row projections.
    ///
    /// # Errors
    /// Returns corrupt JSON, non-contiguous sequence, projection, or `SQLite` errors.
    pub fn load_events(&self, run_id: RunId) -> Result<Vec<SequencedEvent>, StoreError> {
        self.load_events_after(run_id, 0)
    }

    /// Loads the run's events after one already-seen sequence number.
    ///
    /// The log only ever grows, so a reader that already holds a prefix of it
    /// can ask for the rest instead of re-reading and re-parsing the whole
    /// history. The contiguity check continues from `after` rather than
    /// restarting, so a gap is still a gap.
    ///
    /// # Errors
    /// Returns corrupt JSON, non-contiguous sequence, projection, or `SQLite` errors.
    pub fn load_events_after(
        &self,
        run_id: RunId,
        after: u64,
    ) -> Result<Vec<SequencedEvent>, StoreError> {
        if !run_exists(&self.connection, run_id)? {
            return Err(StoreError::RunNotFound(run_id));
        }
        let mut statement = self.connection.prepare(
            "SELECT sequence, event_id, event_type, payload_json, occurred_at
             FROM events WHERE run_id = ?1 AND sequence > ?2 ORDER BY sequence",
        )?;
        let after_bound = u64_to_i64(after, "event sequence")?;
        let rows =
            statement.query_map(rusqlite::params![run_id.to_string(), after_bound], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
        let mut events = Vec::new();
        let mut expected = after
            .checked_add(1)
            .ok_or(StoreError::IntegerRange("event sequence"))?;
        for row in rows {
            let (sequence, event_id, event_type, payload_json, occurred_at) = row?;
            let sequence = i64_to_u64(sequence, "event sequence")?;
            if sequence != expected {
                return Err(StoreError::EventSequenceGap {
                    run_id,
                    expected,
                    actual: sequence,
                });
            }
            let event: DomainEvent = serde_json::from_str(&payload_json)?;
            if event.id().to_string() != event_id {
                return Err(StoreError::EventProjectionMismatch("event ID"));
            }
            if event.run_id() != run_id {
                return Err(StoreError::EventProjectionMismatch("run ID"));
            }
            if event_type_text(&event)? != event_type {
                return Err(StoreError::EventProjectionMismatch("event type"));
            }
            if format_timestamp(event.occurred_at()) != occurred_at {
                return Err(StoreError::EventProjectionMismatch("occurred_at"));
            }
            events.push(SequencedEvent { sequence, event });
            expected = expected
                .checked_add(1)
                .ok_or(StoreError::IntegerRange("event sequence"))?;
        }
        Ok(events)
    }
}

/// A run's stage graph is immutable, with exactly one exception: an operator
/// sending a completed run back to remediate its own result appends a cycle.
///
/// The exception is kept as narrow as the thing it exists for. The kind cannot
/// change, every stage that already existed must be byte-identical and in the
/// same position, growth must be an append, and the batch must carry a
/// `RunFixRequested` or `RunContinueRequested` event naming exactly the
/// stages that appeared — the two sibling remediation cycles share this one
/// guard. Anything else — a reordering, an edited definition, a silent append
/// with no event to account for it — is still a rejected mutation of
/// persisted identity.
fn validate_workflow_growth(
    current: &crate::domain::WorkflowDefinition,
    next: &crate::domain::WorkflowDefinition,
    events: &[DomainEvent],
) -> Result<(), StoreError> {
    let changed = || StoreError::ImmutableRunFieldChanged("workflow");
    if current.kind() != next.kind() {
        return Err(changed());
    }
    let existing = current.stages();
    let grown = next.stages();
    if grown.len() <= existing.len() || grown[..existing.len()] != *existing {
        return Err(changed());
    }
    let appended = grown[existing.len()..]
        .iter()
        .map(crate::domain::StageDefinition::id)
        .collect::<Vec<_>>();
    let declared = events
        .iter()
        .find_map(|event| match event.kind() {
            DomainEventKind::RunFixRequested { stage_ids }
            | DomainEventKind::RunContinueRequested { stage_ids } => Some(stage_ids),
            _ => None,
        })
        .ok_or_else(changed)?;
    if declared.iter().collect::<Vec<_>>() != appended {
        return Err(changed());
    }
    Ok(())
}

pub(crate) fn commit_run_update_transaction(
    transaction: &Transaction<'_>,
    run: &Run,
    expected_revision: RunRevision,
    events: &[DomainEvent],
    allow_active_apply: bool,
) -> Result<CommitResult, StoreError> {
    run.validate_invariants()?;
    let snapshot_json = encode_run(run)?;
    let status = enum_text(&run.status())?;
    let workflow = enum_text(&run.workflow_kind())?;
    let next_revision = expected_revision
        .value()
        .checked_add(1)
        .ok_or(StoreError::IntegerRange("next run revision"))?;
    if !allow_active_apply && active_apply_exists(transaction, run.id())? {
        return Err(StoreError::RunFrozenForApply(run.id()));
    }
    let current = load_commit_state(transaction, run.id())?;
    if current.run.workflow() != run.workflow() {
        validate_workflow_growth(current.run.workflow(), run.workflow(), events)?;
    }
    if current.run.config_snapshot_id() != run.config_snapshot_id() {
        return Err(StoreError::ImmutableRunFieldChanged(
            "config snapshot binding",
        ));
    }
    if current.run.created_at() != run.created_at() {
        return Err(StoreError::ImmutableRunFieldChanged("created_at"));
    }
    validate_event_batch(run, events, current.last_occurred_at)?;
    if events
        .iter()
        .any(|event| matches!(event.kind(), DomainEventKind::RunCreated { .. }))
    {
        return Err(StoreError::UnexpectedRunCreatedEvent);
    }

    let changed = transaction.execute(
        "UPDATE runs
         SET status = ?1, workflow = ?2, snapshot_schema_version = ?3,
             snapshot_json = ?4, revision = ?5, updated_at = ?6
         WHERE id = ?7 AND revision = ?8",
        params![
            status,
            workflow,
            i64::from(RUN_SNAPSHOT_SCHEMA_VERSION),
            snapshot_json,
            u64_to_i64(next_revision, "next run revision")?,
            format_timestamp(run.updated_at()),
            run.id().to_string(),
            u64_to_i64(expected_revision.value(), "expected run revision")?,
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::ConcurrentModification {
            run_id: run.id(),
            expected: expected_revision.value(),
        });
    }

    let first_sequence = current
        .last_sequence
        .checked_add(1)
        .ok_or(StoreError::IntegerRange("next event sequence"))?;
    insert_events(transaction, run, events, first_sequence)?;
    let event_count =
        u64::try_from(events.len()).map_err(|_| StoreError::IntegerRange("event count"))?;
    let last_sequence = first_sequence
        .checked_add(event_count - 1)
        .ok_or(StoreError::IntegerRange("last event sequence"))?;

    Ok(CommitResult {
        revision: RunRevision(next_revision),
        last_sequence,
    })
}

fn active_apply_exists(connection: &Connection, run_id: RunId) -> Result<bool, StoreError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM run_apply_operations
             WHERE run_id = ?1 AND status IN ('prepared', 'applied_to_source')",
            [run_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

struct StoredRunRow {
    id: String,
    status: String,
    workflow: String,
    config_snapshot_id: String,
    snapshot_schema_version: i64,
    snapshot_json: String,
    revision: i64,
    created_at: String,
    updated_at: String,
}

struct CommitState {
    run: Run,
    last_sequence: u64,
    last_occurred_at: Option<DateTime<Utc>>,
}

fn insert_run_input(connection: &Connection, input: &RunInput) -> Result<(), StoreError> {
    let existing = load_run_input_optional(connection, input.run_id())?;
    if let Some(existing) = existing {
        if &existing == input {
            return Ok(());
        }
        return Err(StoreError::RunInputConflict(input.run_id()));
    }
    connection.execute(
        "INSERT INTO run_inputs (run_id, schema_version, task, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            input.run_id().to_string(),
            i64::from(input.schema_version()),
            input.task(),
            format_timestamp(input.created_at()),
        ],
    )?;
    Ok(())
}

fn load_run_input_optional(
    connection: &Connection,
    run_id: RunId,
) -> Result<Option<RunInput>, StoreError> {
    let row = connection
        .query_row(
            "SELECT schema_version, task, created_at FROM run_inputs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(schema_version, task, created_at)| {
        RunInput::from_stored(
            run_id,
            i64_to_u32(schema_version, "run input schema version")?,
            task,
            parse_timestamp(&created_at)?,
        )
        .map_err(StoreError::from)
    })
    .transpose()
}

fn insert_config(
    connection: &Connection,
    snapshot: &ResolvedConfigSnapshot,
) -> Result<(), StoreError> {
    let existing = load_config_optional(connection, snapshot.id())?;
    if let Some(existing) = existing {
        if &existing == snapshot {
            return Ok(());
        }
        return Err(StoreError::ConfigSnapshotConflict(snapshot.id().clone()));
    }
    connection.execute(
        "INSERT INTO config_snapshots (
             id, schema_version, payload_json, content_hash, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            snapshot.id().as_str(),
            i64::from(snapshot.schema_version()),
            snapshot.payload_json()?,
            snapshot.content_hash(),
            format_timestamp(snapshot.created_at()),
        ],
    )?;
    Ok(())
}

fn load_config(
    connection: &Connection,
    id: &ConfigSnapshotId,
) -> Result<ResolvedConfigSnapshot, StoreError> {
    load_config_optional(connection, id)?
        .ok_or_else(|| StoreError::ConfigSnapshotNotFound(id.clone()))
}

fn load_config_optional(
    connection: &Connection,
    id: &ConfigSnapshotId,
) -> Result<Option<ResolvedConfigSnapshot>, StoreError> {
    let row = connection
        .query_row(
            "SELECT schema_version, payload_json, content_hash, created_at
             FROM config_snapshots WHERE id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(schema_version, payload_json, content_hash, created_at)| {
        ResolvedConfigSnapshot::from_stored(
            id.clone(),
            i64_to_u32(schema_version, "config schema version")?,
            serde_json::from_str(&payload_json)?,
            &content_hash,
            parse_timestamp(&created_at)?,
        )
    })
    .transpose()
}

fn load_run_row(connection: &Connection, run_id: RunId) -> Result<StoredRunRow, StoreError> {
    connection
        .query_row(
            "SELECT id, status, workflow, config_snapshot_id, snapshot_schema_version,
                    snapshot_json, revision, created_at, updated_at
             FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| {
                Ok(StoredRunRow {
                    id: row.get(0)?,
                    status: row.get(1)?,
                    workflow: row.get(2)?,
                    config_snapshot_id: row.get(3)?,
                    snapshot_schema_version: row.get(4)?,
                    snapshot_json: row.get(5)?,
                    revision: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            },
        )
        .optional()?
        .ok_or(StoreError::RunNotFound(run_id))
}

fn load_commit_state(connection: &Connection, run_id: RunId) -> Result<CommitState, StoreError> {
    let row = load_run_row(connection, run_id)?;
    let run = decode_run(
        &row.snapshot_json,
        i64_to_u32(row.snapshot_schema_version, "snapshot schema version")?,
    )?;
    validate_run_projection(&run, &row)?;
    let event = connection
        .query_row(
            "SELECT sequence, occurred_at FROM events
             WHERE run_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [run_id.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let (last_sequence, last_occurred_at) = event.map_or(Ok((0, None)), |(sequence, at)| {
        Ok::<_, StoreError>((
            i64_to_u64(sequence, "event sequence")?,
            Some(parse_timestamp(&at)?),
        ))
    })?;
    Ok(CommitState {
        run,
        last_sequence,
        last_occurred_at,
    })
}

fn validate_run_projection(run: &Run, row: &StoredRunRow) -> Result<(), StoreError> {
    if run.id().to_string() != row.id {
        return Err(StoreError::SnapshotProjectionMismatch("run ID"));
    }
    if enum_text(&run.status())? != row.status {
        return Err(StoreError::SnapshotProjectionMismatch("status"));
    }
    if enum_text(&run.workflow_kind())? != row.workflow {
        return Err(StoreError::SnapshotProjectionMismatch("workflow"));
    }
    if run.config_snapshot_id().as_str() != row.config_snapshot_id {
        return Err(StoreError::SnapshotProjectionMismatch("config snapshot ID"));
    }
    if format_timestamp(run.created_at()) != row.created_at {
        return Err(StoreError::SnapshotProjectionMismatch("created_at"));
    }
    if format_timestamp(run.updated_at()) != row.updated_at {
        return Err(StoreError::SnapshotProjectionMismatch("updated_at"));
    }
    Ok(())
}

fn validate_event_batch(
    run: &Run,
    events: &[DomainEvent],
    previous: Option<DateTime<Utc>>,
) -> Result<(), StoreError> {
    if events.is_empty() {
        return Err(StoreError::EmptyEventBatch);
    }
    let mut last = previous.unwrap_or(*run.created_at());
    for event in events {
        if event.run_id() != run.id() {
            return Err(StoreError::EventRunMismatch {
                event_id: event.id(),
                expected: run.id(),
                actual: event.run_id(),
            });
        }
        if let Some(stage_id) = event.stage_id()
            && run.stage(stage_id).is_none()
        {
            return Err(StoreError::EventStageMismatch {
                event_id: event.id(),
                stage_id: stage_id.clone(),
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
    if &last != run.updated_at() {
        return Err(StoreError::EventStateTimestampMismatch);
    }
    Ok(())
}

fn insert_events(
    transaction: &Transaction<'_>,
    run: &Run,
    events: &[DomainEvent],
    first_sequence: u64,
) -> Result<(), StoreError> {
    for (offset, event) in events.iter().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| StoreError::IntegerRange("event offset"))?;
        let sequence = first_sequence
            .checked_add(offset)
            .ok_or(StoreError::IntegerRange("event sequence"))?;
        let payload_json = serde_json::to_string(event)?;
        transaction.execute(
            "INSERT INTO events (
                 run_id, sequence, event_id, event_type, payload_json, occurred_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.id().to_string(),
                u64_to_i64(sequence, "event sequence")?,
                event.id().to_string(),
                event_type_text(event)?,
                payload_json,
                format_timestamp(event.occurred_at()),
            ],
        )?;
    }
    Ok(())
}

fn run_exists(connection: &Connection, run_id: RunId) -> Result<bool, StoreError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn enum_text(value: &impl Serialize) -> Result<String, StoreError> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or(StoreError::SnapshotProjectionMismatch(
            "enum did not serialize as text",
        ))
}

fn enum_from_text<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StoreError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        value.to_owned(),
    ))?)
}

fn event_type_text(event: &DomainEvent) -> Result<String, StoreError> {
    serde_json::to_value(event)?
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or(StoreError::EventProjectionMismatch("serialized event type"))
}

pub(crate) fn format_timestamp(timestamp: &DateTime<Utc>) -> String {
    timestamp.to_rfc3339()
}

pub(crate) fn parse_timestamp(timestamp: &str) -> Result<DateTime<Utc>, StoreError> {
    Ok(DateTime::parse_from_rfc3339(timestamp)?.with_timezone(&Utc))
}

/// Whether the database already keeps a write-ahead log.
///
/// An in-memory database cannot, and says so by reporting "memory"; that is not
/// a failure, just a database this does not apply to.
fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(failure.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn journal_mode_is_wal(connection: &Connection) -> Result<bool, StoreError> {
    let mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?;
    Ok(mode.eq_ignore_ascii_case("wal"))
}

pub(crate) fn i64_to_u64(value: i64, field: &'static str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::IntegerRange(field))
}

fn i64_to_u32(value: i64, field: &'static str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::IntegerRange(field))
}

pub(crate) fn u64_to_i64(value: u64, field: &'static str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::IntegerRange(field))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rusqlite::ErrorCode;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        AttentionKind, AttentionRequest, AttentionRequestId, Dependency, EventId, EventMetadata,
        Role, RunTransition, StageDefinition, StageId, StageKind, StageTransition,
        WorkflowDefinition,
    };

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 14, 8, 0, second)
            .single()
            .unwrap()
    }

    fn metadata(id: u128, second: u32) -> EventMetadata {
        EventMetadata::new(EventId::from_u128(id), at(second))
    }

    /// The stage graph is persisted identity. It grows for exactly one reason,
    /// and every other difference is still a rejected mutation.
    #[test]
    fn a_stage_graph_grows_only_by_an_accounted_for_append() {
        use crate::domain::{RunId, WorkflowKind};

        fn definition(id: &str, kind: StageKind, dependencies: Vec<Dependency>) -> StageDefinition {
            StageDefinition::new(
                StageId::new(id).unwrap(),
                kind,
                Role::Implementer,
                dependencies,
            )
        }
        fn workflow(stages: Vec<StageDefinition>) -> WorkflowDefinition {
            WorkflowDefinition::new(WorkflowKind::Standard, stages).unwrap()
        }
        fn fix_event(stage_ids: &[&str]) -> Vec<DomainEvent> {
            vec![DomainEvent::new(
                metadata(1, 1),
                RunId::from_u128(1),
                None,
                DomainEventKind::RunFixRequested {
                    stage_ids: stage_ids
                        .iter()
                        .map(|id| StageId::new(*id).unwrap())
                        .collect(),
                },
            )]
        }

        let current = workflow(vec![
            definition("implementation", StageKind::Implementation, vec![]),
            definition(
                "decision",
                StageKind::Decision,
                vec![Dependency::required(
                    StageId::new("implementation").unwrap(),
                )],
            ),
        ]);
        let appended = vec![definition(
            "fix_1",
            StageKind::Fix,
            vec![Dependency::required(StageId::new("decision").unwrap())],
        )];
        let grown = current.extended(appended.clone()).unwrap();

        // The one sanctioned growth: an append the batch accounts for.
        validate_workflow_growth(&current, &grown, &fix_event(&["fix_1"])).unwrap();

        // An append no event accounts for is a silent mutation.
        assert!(matches!(
            validate_workflow_growth(&current, &grown, &[]),
            Err(StoreError::ImmutableRunFieldChanged("workflow"))
        ));

        // An event that does not name what actually appeared is not an
        // account of it.
        assert!(matches!(
            validate_workflow_growth(&current, &grown, &fix_event(&["fix_2"])),
            Err(StoreError::ImmutableRunFieldChanged("workflow"))
        ));

        // Editing a stage that already existed is not growth, even alongside a
        // legitimate append.
        let edited = workflow(vec![
            definition("implementation", StageKind::Research, vec![]),
            definition(
                "decision",
                StageKind::Decision,
                vec![Dependency::required(
                    StageId::new("implementation").unwrap(),
                )],
            ),
            appended[0].clone(),
        ]);
        assert!(matches!(
            validate_workflow_growth(&current, &edited, &fix_event(&["fix_1"])),
            Err(StoreError::ImmutableRunFieldChanged("workflow"))
        ));

        // Nor is reordering what was already there.
        let reordered = workflow(vec![
            definition("decision", StageKind::Decision, vec![]),
            definition(
                "implementation",
                StageKind::Implementation,
                vec![Dependency::required(StageId::new("decision").unwrap())],
            ),
            definition(
                "fix_1",
                StageKind::Fix,
                vec![Dependency::required(StageId::new("decision").unwrap())],
            ),
        ]);
        assert!(matches!(
            validate_workflow_growth(&current, &reordered, &fix_event(&["fix_1"])),
            Err(StoreError::ImmutableRunFieldChanged("workflow"))
        ));

        // Shrinking is not growth.
        let shrunk = workflow(vec![definition(
            "implementation",
            StageKind::Implementation,
            vec![],
        )]);
        assert!(matches!(
            validate_workflow_growth(&current, &shrunk, &fix_event(&["fix_1"])),
            Err(StoreError::ImmutableRunFieldChanged("workflow"))
        ));
    }

    /// The continue cycle's own event is the sibling account this guard also
    /// accepts, under its own stage identities.
    #[test]
    fn a_stage_graph_also_grows_by_a_continue_cycle_the_sibling_event_accounts_for() {
        use crate::domain::{RunId, WorkflowKind};

        fn definition(id: &str, kind: StageKind, dependencies: Vec<Dependency>) -> StageDefinition {
            StageDefinition::new(
                StageId::new(id).unwrap(),
                kind,
                Role::Implementer,
                dependencies,
            )
        }
        fn workflow(stages: Vec<StageDefinition>) -> WorkflowDefinition {
            WorkflowDefinition::new(WorkflowKind::Standard, stages).unwrap()
        }
        fn continue_event(stage_ids: &[&str]) -> Vec<DomainEvent> {
            vec![DomainEvent::new(
                metadata(1, 1),
                RunId::from_u128(1),
                None,
                DomainEventKind::RunContinueRequested {
                    stage_ids: stage_ids
                        .iter()
                        .map(|id| StageId::new(*id).unwrap())
                        .collect(),
                },
            )]
        }

        let current = workflow(vec![definition("decision", StageKind::Decision, vec![])]);
        let appended = vec![definition(
            "followup_1",
            StageKind::FollowUp,
            vec![Dependency::required(StageId::new("decision").unwrap())],
        )];
        let grown = current.extended(appended).unwrap();

        validate_workflow_growth(&current, &grown, &continue_event(&["followup_1"])).unwrap();
        // An event that does not name what actually appeared is not an
        // account of it, exactly like its sibling `RunFixRequested` case.
        assert!(matches!(
            validate_workflow_growth(&current, &grown, &continue_event(&["followup_2"])),
            Err(StoreError::ImmutableRunFieldChanged("workflow"))
        ));
    }

    fn stage_id(value: &str) -> StageId {
        StageId::new(value).unwrap()
    }

    fn config(id: &str) -> ResolvedConfigSnapshot {
        ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new(id).unwrap(),
            1,
            json!({
                "providers": {"review": "fake"},
                "limits": {"parallel": 2, "budget": 1000}
            }),
            at(0),
        )
        .unwrap()
    }

    fn workflow() -> WorkflowDefinition {
        WorkflowDefinition::new(
            WorkflowKind::Deep,
            vec![
                StageDefinition::new(
                    stage_id("research"),
                    StageKind::Research,
                    Role::Researcher,
                    vec![],
                ),
                StageDefinition::new(
                    stage_id("probe"),
                    StageKind::DeepAnalysis,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    stage_id("optional_review"),
                    StageKind::Review,
                    Role::Reviewer,
                    vec![],
                ),
                StageDefinition::new(
                    stage_id("implementation"),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![
                        Dependency::required(stage_id("research")),
                        Dependency::optional(stage_id("probe")),
                        Dependency::optional(stage_id("optional_review")),
                    ],
                ),
            ],
        )
        .unwrap()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "acceptance fixture intentionally shows every persisted lifecycle transition"
    )]
    fn complex_run(run_value: u128, config_id: &str, event_base: u128) -> (Run, Vec<DomainEvent>) {
        let mut run = Run::new(
            RunId::from_u128(run_value),
            workflow(),
            ConfigSnapshotId::new(config_id).unwrap(),
            at(0),
        );
        let mut events = vec![run.created_event(metadata(event_base, 0))];
        events.push(
            run.transition(RunTransition::BeginPreparation, metadata(event_base + 1, 1))
                .unwrap(),
        );
        events.push(
            run.transition(
                RunTransition::FinishPreparation,
                metadata(event_base + 2, 2),
            )
            .unwrap(),
        );
        events.push(
            run.transition(RunTransition::Start, metadata(event_base + 3, 3))
                .unwrap(),
        );

        let research = stage_id("research");
        events.push(
            run.transition_stage(
                &research,
                StageTransition::MarkReady,
                metadata(event_base + 4, 4),
            )
            .unwrap(),
        );
        events.push(
            run.transition_stage(
                &research,
                StageTransition::Start,
                metadata(event_base + 5, 5),
            )
            .unwrap(),
        );
        events.push(
            run.transition_stage(
                &research,
                StageTransition::Complete,
                metadata(event_base + 6, 6),
            )
            .unwrap(),
        );

        let probe = stage_id("probe");
        events.push(
            run.transition_stage(
                &probe,
                StageTransition::MarkReady,
                metadata(event_base + 7, 7),
            )
            .unwrap(),
        );
        events.push(
            run.transition_stage(&probe, StageTransition::Start, metadata(event_base + 8, 8))
                .unwrap(),
        );
        events.push(
            run.transition_stage(&probe, StageTransition::Fail, metadata(event_base + 9, 9))
                .unwrap(),
        );

        events.push(
            run.transition_stage(
                &stage_id("optional_review"),
                StageTransition::Skip,
                metadata(event_base + 10, 10),
            )
            .unwrap(),
        );
        let implementation = stage_id("implementation");
        events.push(
            run.transition_stage(
                &implementation,
                StageTransition::MarkReady,
                metadata(event_base + 11, 11),
            )
            .unwrap(),
        );
        events.push(
            run.transition_stage(
                &implementation,
                StageTransition::Start,
                metadata(event_base + 12, 12),
            )
            .unwrap(),
        );
        let request_id = AttentionRequestId::from_u128(run_value + 500);
        events.push(
            run.request_attention(
                AttentionRequest::new(
                    request_id,
                    run.id(),
                    implementation,
                    AttentionKind::Decision,
                    "Choose recovery policy",
                    at(13),
                )
                .unwrap(),
                metadata(event_base + 13, 13),
            )
            .unwrap(),
        );
        events.push(
            run.resolve_attention(request_id, metadata(event_base + 14, 14))
                .unwrap(),
        );
        events.push(
            run.transition(RunTransition::Pause, metadata(event_base + 15, 15))
                .unwrap(),
        );
        events.push(
            run.transition(RunTransition::Resume, metadata(event_base + 16, 16))
                .unwrap(),
        );
        events.push(
            run.transition(RunTransition::Interrupt, metadata(event_base + 17, 17))
                .unwrap(),
        );
        (run, events)
    }

    fn database(temp: &TempDir) -> std::path::PathBuf {
        temp.path().join("nested").join("senate.db")
    }

    fn create_complex(
        store: &mut SqliteStore,
        run_value: u128,
        config_id: &str,
        event_base: u128,
    ) -> (Run, Vec<DomainEvent>, ResolvedConfigSnapshot) {
        let config = config(config_id);
        let (run, events) = complex_run(run_value, config_id, event_base);
        store.create_run(&run, &config, &events).unwrap();
        (run, events, config)
    }

    #[test]
    fn failed_creation_hook_rolls_back_run_input_config_and_events() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let config = config("hook-rollback");
        let (run, events) = complex_run(9_001, "hook-rollback", 50_000);
        let input = RunInput::new(run.id(), "hook rollback task", *run.created_at()).unwrap();

        let error = store
            .create_run_with_input_hook(&run, &input, &config, &events, |_| {
                Err(StoreError::SnapshotProjectionMismatch(
                    "injected hook failure",
                ))
            })
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::SnapshotProjectionMismatch("injected hook failure")
        ));
        assert!(matches!(
            store.load_run(run.id()),
            Err(StoreError::RunNotFound(id)) if id == run.id()
        ));
        assert!(store.load_run_input(run.id()).unwrap().is_none());
        assert!(matches!(
            store.load_config_snapshot(config.id()),
            Err(StoreError::ConfigSnapshotNotFound(_))
        ));
        let run_count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1",
                [run.id().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_count, 0);
    }

    /// Several processes open a fresh database at once: the control room, a
    /// run, the 3D view. Every open must succeed and leave the schema current;
    /// a step applied twice used to fail on a table that already existed.
    #[test]
    fn concurrent_first_opens_all_succeed_and_migrate_once() {
        for _ in 0..20 {
            let temp = TempDir::new().unwrap();
            let path = database(&temp);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let opens: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    let barrier = std::sync::Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        SqliteStore::open(&path).map(|store| store.schema_version())
                    })
                })
                .collect();
            for open in opens {
                let version = open
                    .join()
                    .unwrap()
                    .expect("a concurrent first open failed");
                assert_eq!(version.unwrap(), migrations::DATABASE_SCHEMA_VERSION);
            }
            // And the database is sound for whoever opens it next.
            let later = SqliteStore::open(&path).unwrap();
            assert_eq!(
                later.schema_version().unwrap(),
                migrations::DATABASE_SCHEMA_VERSION
            );
        }
    }

    #[test]
    fn migration_enables_constraints_and_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let path = database(&temp);
        let store = SqliteStore::open(&path).unwrap();

        assert_eq!(
            store.schema_version().unwrap(),
            migrations::DATABASE_SCHEMA_VERSION
        );
        assert_eq!(
            store
                .connection
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let tables = store
            .connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name IN (
                     'runs', 'events', 'config_snapshots', 'run_workspaces',
                     'run_apply_operations'
                 )
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            tables,
            vec![
                "config_snapshots",
                "events",
                "run_apply_operations",
                "run_workspaces",
                "runs"
            ]
        );
        drop(store);

        let reopened = SqliteStore::open(path).unwrap();
        assert_eq!(
            reopened.schema_version().unwrap(),
            migrations::DATABASE_SCHEMA_VERSION
        );
    }

    #[test]
    fn future_database_schema_version_is_rejected() {
        let temp = TempDir::new().unwrap();
        let path = database(&temp);
        let store = SqliteStore::open(&path).unwrap();
        store
            .connection
            .pragma_update(None, "user_version", 99)
            .unwrap();
        drop(store);

        assert!(matches!(
            SqliteStore::open(path),
            Err(StoreError::UnsupportedDatabaseVersion(99))
        ));
    }

    #[test]
    fn complex_run_round_trips_after_reopen_with_event_history_and_config() {
        let temp = TempDir::new().unwrap();
        let path = database(&temp);
        let (original, original_events, original_config) = {
            let mut store = SqliteStore::open(&path).unwrap();
            create_complex(&mut store, 100, "config-complex", 1_000)
        };

        let mut store = SqliteStore::open(path).unwrap();
        let restored = store.load_run(original.id()).unwrap();
        let stored_events = store.load_events(original.id()).unwrap();

        assert_eq!(restored.run, original);
        assert_eq!(restored.revision, RunRevision::initial());
        assert_eq!(restored.config_snapshot, original_config);
        assert_eq!(
            stored_events
                .iter()
                .map(|item| item.event.clone())
                .collect::<Vec<_>>(),
            original_events
        );
        assert_eq!(stored_events.last().unwrap().sequence, 18);
    }

    /// Reading the log in pieces has to give exactly what reading it whole
    /// gives, because that equivalence is the only thing that makes it safe for
    /// the drive loop to keep a prefix and fetch the rest.
    #[test]
    fn reading_the_event_log_in_pieces_matches_reading_it_whole() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 300, "config-tail", 4_000);
        let whole = store.load_events(run.id()).unwrap();
        assert!(whole.len() > 2, "the fixture needs a log worth splitting");

        let split_at = whole.len() / 2;
        let mut pieced = store.load_events_after(run.id(), 0).unwrap();
        pieced.truncate(split_at);
        let resumed_from = pieced.last().unwrap().sequence;
        pieced.extend(store.load_events_after(run.id(), resumed_from).unwrap());

        assert_eq!(pieced, whole);
        assert!(
            store
                .load_events_after(run.id(), whole.last().unwrap().sequence)
                .unwrap()
                .is_empty(),
            "a reader already at the end of the log is told there is nothing new"
        );
    }

    /// A gap is still a gap when the read starts part-way: the contiguity check
    /// continues from where the reader was, rather than restarting at one and
    /// accepting whatever it finds.
    #[test]
    fn a_gap_after_the_resume_point_is_still_refused() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 301, "config-gap", 5_000);
        let whole = store.load_events(run.id()).unwrap();
        let removed = whole[whole.len() / 2].sequence;
        store
            .connection
            .execute(
                "DELETE FROM events WHERE run_id = ?1 AND sequence = ?2",
                rusqlite::params![run.id().to_string(), u64_to_i64(removed, "seq").unwrap()],
            )
            .unwrap();

        let error = store
            .load_events_after(run.id(), removed - 1)
            .expect_err("a hole in the log is not a shorter log");

        assert!(
            matches!(error, StoreError::EventSequenceGap { expected, .. } if expected == removed),
            "{error:?}"
        );
    }

    #[test]
    fn multiple_runs_list_and_sequence_independently() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (first, _, shared_config) = create_complex(&mut store, 200, "shared-config", 2_000);
        let (second, second_events) = complex_run(201, "shared-config", 3_000);
        store
            .create_run(&second, &shared_config, &second_events)
            .unwrap();

        let summaries = store.list_runs().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(store.load_events(first.id()).unwrap()[0].sequence, 1);
        assert_eq!(store.load_events(second.id()).unwrap()[0].sequence, 1);
        assert_eq!(store.load_run(first.id()).unwrap().run, first);
        assert_eq!(store.load_run(second.id()).unwrap().run, second);
    }

    /// Hiding is list metadata, not a run mutation: the flag round-trips
    /// through `list_runs`, while the run's revision, snapshot and
    /// `updated_at` stay exactly as they were.
    #[test]
    fn hiding_a_run_flags_the_summary_without_mutating_the_run() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 210, "hide-config", 4_000);
        let before = store
            .list_runs()
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == run.id())
            .unwrap();
        assert!(!before.archived);

        store.set_run_archived(run.id(), true).unwrap();
        let after = store
            .list_runs()
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == run.id())
            .unwrap();
        assert!(after.archived);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.updated_at, before.updated_at);
        assert_eq!(store.load_run(run.id()).unwrap().run, run);

        store.set_run_archived(run.id(), false).unwrap();
        assert!(!store.list_runs().unwrap()[0].archived);

        let missing = crate::domain::RunId::from_u128(999_999);
        assert!(matches!(
            store.set_run_archived(missing, false),
            Err(StoreError::RunNotFound(id)) if id == missing
        ));
    }

    /// Standing approval belongs to the run it was given for: it survives
    /// reopening the database, it never touches what the run has done, and it
    /// reaches no other run.
    #[test]
    fn auto_approve_is_per_run_durable_and_not_a_run_mutation() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (armed, _, _) = create_complex(&mut store, 220, "armed-config", 4_100);
        let (other, _, _) = create_complex(&mut store, 221, "other-config", 4_200);
        assert!(!store.run_auto_approve(armed.id()).unwrap());

        let before = store.load_run(armed.id()).unwrap();
        store.set_run_auto_approve(armed.id(), true).unwrap();
        let after = store.load_run(armed.id()).unwrap();
        assert!(store.run_auto_approve(armed.id()).unwrap());
        assert!(!store.run_auto_approve(other.id()).unwrap());
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.run, before.run);
        let summary = store
            .list_runs()
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == armed.id())
            .unwrap();
        assert!(summary.auto_approve);

        store.set_run_auto_approve(armed.id(), false).unwrap();
        assert!(!store.run_auto_approve(armed.id()).unwrap());

        let missing = crate::domain::RunId::from_u128(999_998);
        assert!(matches!(
            store.set_run_auto_approve(missing, true),
            Err(StoreError::RunNotFound(id)) if id == missing
        ));
        assert!(matches!(
            store.run_auto_approve(missing),
            Err(StoreError::RunNotFound(id)) if id == missing
        ));
    }

    #[test]
    fn run_input_is_created_atomically_and_database_immutable() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, events) = complex_run(202, "input-config", 3_500);
        let config = config("input-config");
        let input = RunInput::new(run.id(), "  Unicode α\nsecond line  ", at(0)).unwrap();

        store
            .create_run_with_input(&run, &input, &config, &events)
            .unwrap();

        assert_eq!(store.load_run_input(run.id()).unwrap(), Some(input));
        let update = store.connection.execute(
            "UPDATE run_inputs SET task = 'changed' WHERE run_id = ?1",
            [run.id().to_string()],
        );
        assert!(matches!(
            update,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::ConstraintViolation
        ));
    }

    /// A purge is the one deletion the store performs, and it performs it
    /// whole: every table the run owns is emptied of it, the shared config
    /// snapshot other runs still point at is not, and a second purge of the
    /// same run finds nothing left to delete.
    #[test]
    fn purging_a_run_removes_every_row_it_owns_and_nothing_shared() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, events) = complex_run(206, "purge-config", 3_700);
        let config = config("purge-config");
        let input = RunInput::new(run.id(), "delete me", at(0)).unwrap();
        store
            .create_run_with_input(&run, &input, &config, &events)
            .unwrap();
        let (survivor, survivor_events) = complex_run(207, "purge-config", 3_800);
        let survivor_input = RunInput::new(survivor.id(), "keep me", at(0)).unwrap();
        store
            .create_run_with_input(&survivor, &survivor_input, &config, &survivor_events)
            .unwrap();

        store.purge_run(run.id()).unwrap();

        assert!(matches!(
            store.load_run(run.id()),
            Err(StoreError::RunNotFound(_))
        ));
        assert_eq!(store.load_run_input(run.id()).unwrap(), None);
        assert!(matches!(
            store.load_events(run.id()),
            Err(StoreError::RunNotFound(_))
        ));
        let listed: Vec<_> = store
            .list_runs()
            .unwrap()
            .into_iter()
            .map(|summary| summary.id)
            .collect();
        assert_eq!(listed, vec![survivor.id()]);
        // The other run still loads, so its shared config snapshot survived.
        assert!(store.load_run(survivor.id()).is_ok());
        assert!(matches!(
            store.purge_run(run.id()),
            Err(StoreError::RunNotFound(_))
        ));
    }

    #[test]
    fn initial_event_failure_rolls_back_run_input_run_and_config() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (first, first_events) = complex_run(204, "first-atomic", 3_600);
        let first_config = config("first-atomic");
        let first_input = RunInput::new(first.id(), "first task", at(0)).unwrap();
        store
            .create_run_with_input(&first, &first_input, &first_config, &first_events)
            .unwrap();

        let (second, duplicate_events) = complex_run(205, "second-atomic", 3_600);
        let second_config = config("second-atomic");
        let second_input = RunInput::new(second.id(), "second task", at(0)).unwrap();
        assert!(matches!(
            store.create_run_with_input(&second, &second_input, &second_config, &duplicate_events),
            Err(StoreError::Sqlite(_))
        ));
        assert!(matches!(
            store.load_run(second.id()),
            Err(StoreError::RunNotFound(id)) if id == second.id()
        ));
        assert!(store.load_run_input(second.id()).unwrap().is_none());
        assert!(matches!(
            store.load_config_snapshot(second_config.id()),
            Err(StoreError::ConfigSnapshotNotFound(_))
        ));
    }

    #[test]
    fn current_snapshot_excludes_task_and_legacy_v1_snapshot_still_loads() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 203, "legacy-v1", 3_700);
        let snapshot_json: String = store
            .connection
            .query_row(
                "SELECT snapshot_json FROM runs WHERE id = ?1",
                [run.id().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let mut snapshot: serde_json::Value = serde_json::from_str(&snapshot_json).unwrap();
        assert!(snapshot.get("task").is_none());
        snapshot["schema_version"] = json!(1);
        snapshot["task"] = json!("legacy task ignored by aggregate");
        store
            .connection
            .execute(
                "UPDATE runs SET snapshot_schema_version = 1, snapshot_json = ?1 WHERE id = ?2",
                params![
                    serde_json::to_string(&snapshot).unwrap(),
                    run.id().to_string()
                ],
            )
            .unwrap();

        assert_eq!(store.load_run(run.id()).unwrap().run, run);
        assert!(store.load_run_input(run.id()).unwrap().is_none());
    }

    #[test]
    fn immutable_config_rejects_changed_payload_and_preserves_original() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let original = config("immutable");
        store.insert_config_snapshot(&original).unwrap();
        store.insert_config_snapshot(&original).unwrap();
        let direct_update = store.connection.execute(
            "UPDATE config_snapshots SET payload_json = '{}' WHERE id = ?1",
            [original.id().as_str()],
        );
        assert!(matches!(
            direct_update,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::ConstraintViolation
        ));
        let changed = ResolvedConfigSnapshot::new(
            ConfigSnapshotId::new("immutable").unwrap(),
            1,
            json!({"different": true}),
            at(1),
        )
        .unwrap();

        assert!(matches!(
            store.insert_config_snapshot(&changed),
            Err(StoreError::ConfigSnapshotConflict(_))
        ));
        assert_eq!(
            store
                .load_config_snapshot(&ConfigSnapshotId::new("immutable").unwrap())
                .unwrap(),
            original
        );
    }

    #[test]
    fn corrupted_config_hash_is_rejected_on_load() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let original = config("bad-hash");
        store.insert_config_snapshot(&original).unwrap();
        store
            .connection
            .execute_batch("DROP TRIGGER config_snapshots_no_update")
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE config_snapshots SET content_hash = ?1 WHERE id = ?2",
                params!["0".repeat(64), original.id().as_str()],
            )
            .unwrap();

        assert!(matches!(
            store.load_config_snapshot(original.id()),
            Err(StoreError::InvalidConfigHash(_))
        ));
    }

    #[test]
    fn corrupt_snapshot_json_never_constructs_run() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 300, "corrupt-json", 4_000);
        store
            .connection
            .execute(
                "UPDATE runs SET snapshot_json = '{' WHERE id = ?1",
                [run.id().to_string()],
            )
            .unwrap();

        assert!(matches!(store.load_run(run.id()), Err(StoreError::Json(_))));
    }

    #[test]
    fn unknown_snapshot_version_is_rejected_before_rehydration() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 301, "future-version", 5_000);
        let snapshot_json: String = store
            .connection
            .query_row(
                "SELECT snapshot_json FROM runs WHERE id = ?1",
                [run.id().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let mut snapshot: serde_json::Value = serde_json::from_str(&snapshot_json).unwrap();
        snapshot["schema_version"] = json!(99);
        store
            .connection
            .execute(
                "UPDATE runs SET snapshot_schema_version = 99, snapshot_json = ?1 WHERE id = ?2",
                params![
                    serde_json::to_string(&snapshot).unwrap(),
                    run.id().to_string()
                ],
            )
            .unwrap();

        assert!(matches!(
            store.load_run(run.id()),
            Err(StoreError::UnsupportedSnapshotVersion(99))
        ));
    }

    #[test]
    fn semantically_invalid_snapshot_is_rejected_by_domain_rehydration() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 302, "invalid-domain", 6_000);
        let snapshot_json: String = store
            .connection
            .query_row(
                "SELECT snapshot_json FROM runs WHERE id = ?1",
                [run.id().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let mut snapshot: serde_json::Value = serde_json::from_str(&snapshot_json).unwrap();
        snapshot["status"] = json!("completed");
        snapshot["suspended_from"] = serde_json::Value::Null;
        store
            .connection
            .execute(
                "UPDATE runs SET status = 'completed', snapshot_json = ?1 WHERE id = ?2",
                params![
                    serde_json::to_string(&snapshot).unwrap(),
                    run.id().to_string()
                ],
            )
            .unwrap();

        assert!(matches!(
            store.load_run(run.id()),
            Err(StoreError::Rehydration(_))
        ));
    }

    #[test]
    fn advanced_stage_with_invalid_dependency_is_rejected() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 303, "bad-dependency", 6_500);
        let snapshot_json: String = store
            .connection
            .query_row(
                "SELECT snapshot_json FROM runs WHERE id = ?1",
                [run.id().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let mut snapshot: serde_json::Value = serde_json::from_str(&snapshot_json).unwrap();
        let stages = snapshot["stages"].as_array_mut().unwrap();
        let probe = stages
            .iter_mut()
            .find(|stage| stage["id"] == "probe")
            .unwrap();
        probe["status"] = json!("pending");
        store
            .connection
            .execute(
                "UPDATE runs SET snapshot_json = ?1 WHERE id = ?2",
                params![
                    serde_json::to_string(&snapshot).unwrap(),
                    run.id().to_string()
                ],
            )
            .unwrap();

        assert!(matches!(
            store.load_run(run.id()),
            Err(StoreError::Rehydration(_))
        ));
    }

    #[test]
    fn event_insert_failure_rolls_state_and_revision_back() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (original, _, _) = create_complex(&mut store, 400, "atomic", 7_000);
        let loaded = store.load_run(original.id()).unwrap();
        let before_events = store.load_events(original.id()).unwrap();
        let duplicate_id = before_events[0].event.id();
        let mut candidate = loaded.run.clone();
        let duplicate_event = candidate
            .transition(
                RunTransition::Recover,
                EventMetadata::new(duplicate_id, at(18)),
            )
            .unwrap();

        assert!(matches!(
            store.commit_run_update(&candidate, loaded.revision, &[duplicate_event]),
            Err(StoreError::Sqlite(_))
        ));
        let restored = store.load_run(original.id()).unwrap();
        assert_eq!(restored.run, original);
        assert_eq!(restored.revision, RunRevision::initial());
        assert_eq!(store.load_events(original.id()).unwrap(), before_events);
    }

    #[test]
    fn stale_writer_gets_concurrent_modification() {
        let temp = TempDir::new().unwrap();
        let path = database(&temp);
        let run_id = {
            let mut creator = SqliteStore::open(&path).unwrap();
            create_complex(&mut creator, 500, "concurrent", 8_000)
                .0
                .id()
        };
        let mut first_store = SqliteStore::open(&path).unwrap();
        let mut second_store = SqliteStore::open(&path).unwrap();
        let first_loaded = first_store.load_run(run_id).unwrap();
        let second_loaded = second_store.load_run(run_id).unwrap();
        let mut first_candidate = first_loaded.run;
        let first_event = first_candidate
            .transition(RunTransition::Recover, metadata(8_100, 18))
            .unwrap();
        let mut second_candidate = second_loaded.run;
        let second_event = second_candidate
            .transition(RunTransition::Recover, metadata(8_101, 18))
            .unwrap();

        let committed = first_store
            .commit_run_update(&first_candidate, first_loaded.revision, &[first_event])
            .unwrap();
        assert_eq!(committed.revision().value(), 1);
        assert!(matches!(
            second_store.commit_run_update(
                &second_candidate,
                second_loaded.revision,
                &[second_event]
            ),
            Err(StoreError::ConcurrentModification { expected: 0, .. })
        ));
        assert_eq!(second_store.load_run(run_id).unwrap().run, first_candidate);
    }

    #[test]
    fn equal_timestamps_append_in_contiguous_order() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (original, _, _) = create_complex(&mut store, 600, "same-time", 9_000);
        let loaded = store.load_run(original.id()).unwrap();
        let mut candidate = loaded.run;
        let recover = candidate
            .transition(RunTransition::Recover, metadata(9_100, 18))
            .unwrap();
        let pause = candidate
            .transition(RunTransition::Pause, metadata(9_101, 18))
            .unwrap();

        let committed = store
            .commit_run_update(&candidate, loaded.revision, &[recover, pause])
            .unwrap();
        let events = store.load_events(original.id()).unwrap();
        assert_eq!(committed.last_sequence(), 20);
        assert_eq!(
            events[18].event.occurred_at(),
            events[19].event.occurred_at()
        );
        assert_eq!(events[18].sequence, 19);
        assert_eq!(events[19].sequence, 20);
    }

    #[test]
    fn timestamp_regression_is_rejected_even_when_domain_candidate_is_valid() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (original, _, _) = create_complex(&mut store, 601, "time-regression", 9_500);
        let loaded = store.load_run(original.id()).unwrap();
        let mut candidate = loaded.run;
        let backdated = candidate
            .transition(RunTransition::Recover, metadata(9_600, 16))
            .unwrap();

        assert!(matches!(
            store.commit_run_update(&candidate, loaded.revision, &[backdated]),
            Err(StoreError::EventTimestampRegression { .. })
        ));
        assert_eq!(store.load_run(original.id()).unwrap().run, original);
    }

    #[test]
    fn duplicate_sequence_and_missing_foreign_key_fail_at_database_boundary() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let (run, _, _) = create_complex(&mut store, 700, "constraints", 10_000);
        let first_payload: String = store
            .connection
            .query_row(
                "SELECT payload_json FROM events WHERE run_id = ?1 AND sequence = 1",
                [run.id().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let duplicate = store.connection.execute(
            "INSERT INTO events (run_id, sequence, event_id, event_type, payload_json, occurred_at)
             VALUES (?1, 1, ?2, 'run_created', ?3, ?4)",
            params![
                run.id().to_string(),
                EventId::from_u128(99_999).to_string(),
                first_payload,
                format_timestamp(&at(0)),
            ],
        );
        assert!(matches!(
            duplicate,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::ConstraintViolation
        ));

        let foreign_key = store.connection.execute(
            "INSERT INTO events (run_id, sequence, event_id, event_type, payload_json, occurred_at)
             VALUES (?1, 1, ?2, 'run_created', '{}', ?3)",
            params![
                RunId::from_u128(999).to_string(),
                EventId::from_u128(100_000).to_string(),
                format_timestamp(&at(0)),
            ],
        );
        assert!(matches!(
            foreign_key,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::ConstraintViolation
        ));
    }
}
