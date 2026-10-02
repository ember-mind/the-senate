use rusqlite::Connection;

use super::StoreError;

pub const DATABASE_SCHEMA_VERSION: u32 = 13;

/// Every schema step in order; index `n` takes a version-`n` database to
/// version `n + 1`.
type Migration = fn(&Connection) -> Result<(), StoreError>;

const MIGRATIONS: [Migration; DATABASE_SCHEMA_VERSION as usize] = [
    migrate_v1,
    migrate_v2,
    migrate_v3,
    migrate_v4,
    migrate_v5,
    migrate_v6,
    migrate_v7,
    migrate_v8,
    migrate_v9,
    migrate_v10,
    migrate_v11,
    migrate_v12,
    migrate_v13,
];

/// Brings the database up to [`DATABASE_SCHEMA_VERSION`], one step per
/// write transaction.
///
/// Several processes can open a fresh database at once (the control room,
/// a run, the 3D view). The version is therefore read *inside* the write
/// lock that applies the step: a process that read it outside could apply a
/// step another process had already committed, fail on a table that already
/// exists, and leave every later open failing the same way. An up-to-date
/// database, the everyday case, is recognised without taking the lock.
pub(crate) fn migrate(connection: &Connection) -> Result<(), StoreError> {
    if schema_version(connection)? == DATABASE_SCHEMA_VERSION {
        return Ok(());
    }
    loop {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let applied = apply_next_step(connection);
        match applied {
            Ok(true) => connection.execute_batch("COMMIT")?,
            Ok(false) => {
                connection.execute_batch("COMMIT")?;
                return Ok(());
            }
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
    }
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?)
}

/// Applies the step after the version read under the caller's write lock;
/// `false` when there is none left.
fn apply_next_step(connection: &Connection) -> Result<bool, StoreError> {
    // Touch the schema first. A connection that opened while another was
    // mid-migration still holds the schema it saw then, and some statements
    // (`ALTER TABLE ... RENAME COLUMN`) check columns against that stale copy
    // before SQLite notices the change. Reading `sqlite_master` under the
    // write lock makes it reload.
    connection.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
    let version = schema_version(connection)?;
    if version > DATABASE_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedDatabaseVersion(version));
    }
    let Some(step) = MIGRATIONS.get(version as usize) else {
        return Ok(false);
    };
    step(connection)?;
    Ok(true)
}

fn migrate_v1(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
                "
                 CREATE TABLE config_snapshots (
                     id TEXT PRIMARY KEY NOT NULL,
                     schema_version INTEGER NOT NULL CHECK (schema_version > 0),
                     payload_json TEXT NOT NULL,
                     content_hash TEXT NOT NULL CHECK (length(content_hash) = 64),
                     created_at TEXT NOT NULL
                 );
                 CREATE TABLE runs (
                     id TEXT PRIMARY KEY NOT NULL,
                     status TEXT NOT NULL,
                     workflow TEXT NOT NULL,
                     config_snapshot_id TEXT NOT NULL,
                     snapshot_schema_version INTEGER NOT NULL CHECK (snapshot_schema_version > 0),
                     snapshot_json TEXT NOT NULL,
                     revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     FOREIGN KEY (config_snapshot_id) REFERENCES config_snapshots(id) ON DELETE RESTRICT
                 );
                 CREATE TABLE events (
                     run_id TEXT NOT NULL,
                     sequence INTEGER NOT NULL CHECK (sequence > 0),
                     event_id TEXT NOT NULL UNIQUE,
                     event_type TEXT NOT NULL,
                     payload_json TEXT NOT NULL,
                     occurred_at TEXT NOT NULL,
                     recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                     PRIMARY KEY (run_id, sequence),
                     FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT
                 );
                 CREATE TRIGGER config_snapshots_no_update
                 BEFORE UPDATE ON config_snapshots
                 BEGIN
                     SELECT RAISE(ABORT, 'config snapshots are immutable');
                 END;
                 CREATE TRIGGER config_snapshots_no_delete
                 BEFORE DELETE ON config_snapshots
                 BEGIN
                     SELECT RAISE(ABORT, 'config snapshots are immutable');
                 END;
                 CREATE INDEX runs_status_updated_idx ON runs(status, updated_at DESC);
                 CREATE INDEX runs_config_snapshot_idx ON runs(config_snapshot_id);
                 CREATE INDEX events_run_occurred_idx ON events(run_id, occurred_at);
                 PRAGMA user_version = 1;",
    )?;
    Ok(())
}

fn migrate_v2(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         CREATE TABLE run_workspaces (
             run_id TEXT PRIMARY KEY NOT NULL,
             source_repo_path TEXT NOT NULL,
             git_common_dir TEXT NOT NULL,
             base_commit TEXT NOT NULL CHECK (length(base_commit) IN (40, 64)),
             worktree_path TEXT NOT NULL UNIQUE,
             branch_name TEXT,
             mode TEXT NOT NULL CHECK (mode IN ('branch', 'detached')),
             status TEXT NOT NULL CHECK (
                 status IN ('preparing', 'ready', 'removing', 'removed', 'broken')
             ),
             branch_owned INTEGER NOT NULL DEFAULT 0 CHECK (branch_owned IN (0, 1)),
             removal_head TEXT CHECK (removal_head IS NULL OR length(removal_head) IN (40, 64)),
             last_error TEXT,
             revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             CHECK (
                 (mode = 'branch' AND branch_name IS NOT NULL) OR
                 (mode = 'detached' AND branch_name IS NULL)
             )
         );
         CREATE TABLE run_apply_operations (
             run_id TEXT PRIMARY KEY NOT NULL,
             status TEXT NOT NULL CHECK (
                 status IN ('prepared', 'applied_to_source', 'recorded', 'failed')
             ),
             patch_hash TEXT NOT NULL CHECK (length(patch_hash) = 64),
             run_revision INTEGER NOT NULL CHECK (run_revision >= 0),
             last_error TEXT,
             revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             FOREIGN KEY (run_id) REFERENCES run_workspaces(run_id) ON DELETE RESTRICT
         );
         CREATE INDEX run_workspaces_status_idx ON run_workspaces(status, updated_at);
         CREATE INDEX run_workspaces_common_dir_idx ON run_workspaces(git_common_dir);
         CREATE INDEX run_apply_operations_status_idx
             ON run_apply_operations(status, updated_at);
         PRAGMA user_version = 2;",
    )?;
    Ok(())
}

fn migrate_v3(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         CREATE TABLE run_inputs (
             run_id TEXT PRIMARY KEY NOT NULL,
             schema_version INTEGER NOT NULL CHECK (schema_version > 0),
             task TEXT NOT NULL CHECK (length(trim(task)) > 0),
             created_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT
         );
         CREATE TRIGGER run_inputs_no_update
         BEFORE UPDATE ON run_inputs
         BEGIN
             SELECT RAISE(ABORT, 'run inputs are immutable');
         END;
         CREATE TRIGGER run_inputs_no_delete
         BEFORE DELETE ON run_inputs
         BEGIN
             SELECT RAISE(ABORT, 'run inputs are immutable');
         END;
         PRAGMA user_version = 3;",
    )?;
    Ok(())
}

fn migrate_v4(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         CREATE TABLE managed_processes (
             id TEXT PRIMARY KEY NOT NULL,
             run_id TEXT NOT NULL,
             stage_id TEXT NOT NULL,
             attempt INTEGER NOT NULL CHECK (attempt >= 0),
             backend_kind TEXT NOT NULL CHECK (length(backend_kind) > 0),
             backend_session_id TEXT NOT NULL UNIQUE,
             status TEXT NOT NULL CHECK (
                 status IN (
                     'preparing', 'starting', 'running', 'interrupting', 'exited',
                     'interrupted', 'missing', 'broken', 'cleaned'
                 )
             ),
             spec_schema_version INTEGER NOT NULL CHECK (spec_schema_version > 0),
             spec_json TEXT NOT NULL,
             command_fingerprint TEXT NOT NULL CHECK (length(command_fingerprint) = 64),
             stdout_offset INTEGER NOT NULL DEFAULT 0 CHECK (stdout_offset >= 0),
             stdout_cursor_revision INTEGER NOT NULL DEFAULT 0
                 CHECK (stdout_cursor_revision >= 0),
             stderr_offset INTEGER NOT NULL DEFAULT 0 CHECK (stderr_offset >= 0),
             stderr_cursor_revision INTEGER NOT NULL DEFAULT 0
                 CHECK (stderr_cursor_revision >= 0),
             exit_code INTEGER,
             term_signal INTEGER,
             runner_error TEXT,
             interrupt_requested INTEGER NOT NULL DEFAULT 0
                 CHECK (interrupt_requested IN (0, 1)),
             last_error TEXT,
             revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             started_at TEXT,
             finished_at TEXT,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             UNIQUE (run_id, stage_id, attempt),
             CHECK (
                 (exit_code IS NOT NULL) +
                 (term_signal IS NOT NULL) +
                 (runner_error IS NOT NULL) <= 1
             )
         );
         CREATE TRIGGER managed_processes_identity_immutable
         BEFORE UPDATE ON managed_processes
         WHEN OLD.id IS NOT NEW.id
           OR OLD.run_id IS NOT NEW.run_id
           OR OLD.stage_id IS NOT NEW.stage_id
           OR OLD.attempt IS NOT NEW.attempt
           OR OLD.backend_kind IS NOT NEW.backend_kind
           OR OLD.backend_session_id IS NOT NEW.backend_session_id
           OR OLD.spec_schema_version IS NOT NEW.spec_schema_version
           OR OLD.spec_json IS NOT NEW.spec_json
           OR OLD.command_fingerprint IS NOT NEW.command_fingerprint
           OR OLD.created_at IS NOT NEW.created_at
         BEGIN
             SELECT RAISE(ABORT, 'managed process launch identity is immutable');
         END;
         CREATE INDEX managed_processes_run_status_idx
             ON managed_processes(run_id, status, updated_at);
         CREATE INDEX managed_processes_session_idx
             ON managed_processes(backend_session_id);
         PRAGMA user_version = 4;",
    )?;
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "single transactional schema migration keeps v4 rebuild and v5 tables atomic"
)]
fn migrate_v5(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         DROP INDEX managed_processes_run_status_idx;
         DROP INDEX managed_processes_session_idx;
         DROP TRIGGER managed_processes_identity_immutable;
         ALTER TABLE managed_processes RENAME TO managed_processes_v4;
         CREATE TABLE managed_processes (
             id TEXT PRIMARY KEY NOT NULL,
             run_id TEXT NOT NULL,
             stage_id TEXT NOT NULL,
             attempt INTEGER NOT NULL CHECK (attempt >= 0),
             invocation INTEGER NOT NULL CHECK (invocation > 0),
             backend_kind TEXT NOT NULL CHECK (length(backend_kind) > 0),
             backend_session_id TEXT NOT NULL UNIQUE,
             status TEXT NOT NULL CHECK (
                 status IN (
                     'preparing', 'starting', 'running', 'interrupting', 'exited',
                     'interrupted', 'missing', 'broken', 'cleaned'
                 )
             ),
             spec_schema_version INTEGER NOT NULL CHECK (spec_schema_version > 0),
             spec_json TEXT NOT NULL,
             command_fingerprint TEXT NOT NULL CHECK (length(command_fingerprint) = 64),
             stdout_offset INTEGER NOT NULL DEFAULT 0 CHECK (stdout_offset >= 0),
             stdout_cursor_revision INTEGER NOT NULL DEFAULT 0
                 CHECK (stdout_cursor_revision >= 0),
             stderr_offset INTEGER NOT NULL DEFAULT 0 CHECK (stderr_offset >= 0),
             stderr_cursor_revision INTEGER NOT NULL DEFAULT 0
                 CHECK (stderr_cursor_revision >= 0),
             exit_code INTEGER,
             term_signal INTEGER,
             runner_error TEXT,
             interrupt_requested INTEGER NOT NULL DEFAULT 0
                 CHECK (interrupt_requested IN (0, 1)),
             last_error TEXT,
             revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             started_at TEXT,
             finished_at TEXT,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             UNIQUE (run_id, stage_id, attempt, invocation),
             CHECK (
                 (exit_code IS NOT NULL) +
                 (term_signal IS NOT NULL) +
                 (runner_error IS NOT NULL) <= 1
             )
         );
         INSERT INTO managed_processes (
             id, run_id, stage_id, attempt, invocation, backend_kind,
             backend_session_id, status, spec_schema_version, spec_json,
             command_fingerprint, stdout_offset, stdout_cursor_revision,
             stderr_offset, stderr_cursor_revision, exit_code, term_signal,
             runner_error, interrupt_requested, last_error, revision, created_at,
             updated_at, started_at, finished_at
         )
         SELECT id, run_id, stage_id, attempt, 1, backend_kind,
                backend_session_id, status, spec_schema_version, spec_json,
                command_fingerprint, stdout_offset, stdout_cursor_revision,
                stderr_offset, stderr_cursor_revision, exit_code, term_signal,
                runner_error, interrupt_requested, last_error, revision, created_at,
                updated_at, started_at, finished_at
         FROM managed_processes_v4;
         DROP TABLE managed_processes_v4;
         CREATE TRIGGER managed_processes_identity_immutable
         BEFORE UPDATE ON managed_processes
         WHEN OLD.id IS NOT NEW.id
           OR OLD.run_id IS NOT NEW.run_id
           OR OLD.stage_id IS NOT NEW.stage_id
           OR OLD.attempt IS NOT NEW.attempt
           OR OLD.invocation IS NOT NEW.invocation
           OR OLD.backend_kind IS NOT NEW.backend_kind
           OR OLD.backend_session_id IS NOT NEW.backend_session_id
           OR OLD.spec_schema_version IS NOT NEW.spec_schema_version
           OR OLD.spec_json IS NOT NEW.spec_json
           OR OLD.command_fingerprint IS NOT NEW.command_fingerprint
           OR OLD.created_at IS NOT NEW.created_at
         BEGIN
             SELECT RAISE(ABORT, 'managed process launch identity is immutable');
         END;
         CREATE INDEX managed_processes_run_status_idx
             ON managed_processes(run_id, status, updated_at);
         CREATE INDEX managed_processes_session_idx
             ON managed_processes(backend_session_id);
         CREATE TABLE provider_sessions (
             id TEXT PRIMARY KEY NOT NULL,
             run_id TEXT NOT NULL,
             stage_id TEXT NOT NULL,
             attempt INTEGER NOT NULL CHECK (attempt > 0),
             provider_id TEXT NOT NULL CHECK (length(provider_id) > 0),
             native_session_id TEXT,
             current_process_id TEXT,
             status TEXT NOT NULL CHECK (
                 status IN ('created', 'starting', 'active', 'needs_user',
                            'completed', 'failed', 'interrupted')
             ),
             protocol_version INTEGER NOT NULL CHECK (protocol_version > 0),
             invocation INTEGER NOT NULL DEFAULT 0 CHECK (invocation >= 0),
             model_id TEXT,
             cli_version TEXT,
             pending_attention_id TEXT,
             pending_process_id TEXT,
             pending_record_start INTEGER CHECK (
                 pending_record_start IS NULL OR pending_record_start >= 0
             ),
             pending_record_end INTEGER CHECK (
                 pending_record_end IS NULL OR pending_record_end >= 0
             ),
             revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             FOREIGN KEY (current_process_id) REFERENCES managed_processes(id) ON DELETE RESTRICT,
             FOREIGN KEY (pending_process_id) REFERENCES managed_processes(id) ON DELETE RESTRICT,
             UNIQUE (run_id, stage_id, attempt),
             UNIQUE (provider_id, native_session_id),
             CHECK (
                 (pending_attention_id IS NULL AND pending_process_id IS NULL
                  AND pending_record_start IS NULL AND pending_record_end IS NULL)
                 OR
                 (pending_attention_id IS NOT NULL AND pending_process_id IS NOT NULL
                  AND pending_record_start IS NOT NULL AND pending_record_end IS NOT NULL
                  AND pending_record_end > pending_record_start)
             )
         );
         CREATE INDEX provider_sessions_run_status_idx
             ON provider_sessions(run_id, status, updated_at);
         CREATE TRIGGER provider_sessions_identity_immutable
         BEFORE UPDATE ON provider_sessions
         WHEN OLD.id IS NOT NEW.id
           OR OLD.run_id IS NOT NEW.run_id
           OR OLD.stage_id IS NOT NEW.stage_id
           OR OLD.attempt IS NOT NEW.attempt
           OR OLD.provider_id IS NOT NEW.provider_id
           OR OLD.protocol_version IS NOT NEW.protocol_version
           OR OLD.created_at IS NOT NEW.created_at
         BEGIN
             SELECT RAISE(ABORT, 'provider session identity is immutable');
         END;
         CREATE TABLE artifacts (
             id TEXT PRIMARY KEY NOT NULL,
             run_id TEXT NOT NULL,
             stage_id TEXT NOT NULL,
             attempt INTEGER NOT NULL CHECK (attempt > 0),
             kind TEXT NOT NULL,
             status TEXT NOT NULL CHECK (status IN ('complete', 'skipped', 'failed')),
             role TEXT NOT NULL,
             provider_id TEXT,
             model_id TEXT,
             path TEXT NOT NULL UNIQUE,
             content_hash TEXT NOT NULL CHECK (length(content_hash) = 64),
             content_size INTEGER NOT NULL CHECK (content_size >= 0),
             base_commit TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             UNIQUE (run_id, stage_id, attempt, kind)
         );
         CREATE INDEX artifacts_run_stage_idx
             ON artifacts(run_id, stage_id, attempt);
         CREATE TRIGGER artifacts_no_update
         BEFORE UPDATE ON artifacts
         BEGIN
             SELECT RAISE(ABORT, 'artifacts are immutable');
         END;
         CREATE TRIGGER artifacts_no_delete
         BEFORE DELETE ON artifacts
         BEGIN
             SELECT RAISE(ABORT, 'artifacts are immutable');
         END;
         PRAGMA user_version = 5;",
    )?;
    Ok(())
}

/// Operator-facing visibility: a hidden run stays fully intact and
/// queryable, it is only left out of the default Runs list. Not a run
/// lifecycle state, so it lives beside the snapshot rather than in it.
fn migrate_v6(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         ALTER TABLE runs ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1));
         PRAGMA user_version = 6;",
    )?;
    Ok(())
}

/// One row per image the image-generation tool produced for a run: the
/// per-run bound is counted from these rows, and they are the local answer
/// to who asked for which bytes, when, from which backend. Insert-only like
/// artifacts; the PNG itself is an ordinary worktree file.
fn migrate_v7(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         CREATE TABLE image_generations (
             id TEXT PRIMARY KEY NOT NULL,
             run_id TEXT NOT NULL,
             stage_id TEXT NOT NULL,
             attempt INTEGER NOT NULL CHECK (attempt > 0),
             ordinal INTEGER NOT NULL CHECK (ordinal > 0),
             backend TEXT NOT NULL,
             model TEXT NOT NULL,
             output_path TEXT NOT NULL,
             output_sha256 TEXT NOT NULL CHECK (length(output_sha256) = 64),
             output_size INTEGER NOT NULL CHECK (output_size > 0),
             prompt_sha256 TEXT NOT NULL CHECK (length(prompt_sha256) = 64),
             response_id TEXT,
             requested_at TEXT NOT NULL,
             completed_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             UNIQUE (run_id, ordinal)
         );
         CREATE INDEX image_generations_run_idx ON image_generations(run_id, ordinal);
         CREATE TRIGGER image_generations_no_update
         BEFORE UPDATE ON image_generations
         BEGIN
             SELECT RAISE(ABORT, 'image generations are immutable');
         END;
         CREATE TRIGGER image_generations_no_delete
         BEFORE DELETE ON image_generations
         BEGIN
             SELECT RAISE(ABORT, 'image generations are immutable');
         END;
         PRAGMA user_version = 7;",
    )?;
    Ok(())
}

/// Archiving, and the purge it leads to.
///
/// `hidden` becomes `archived`: the same list metadata under the name the
/// operator acts on — a run is archived, and an archived run is the only one
/// that can be deleted for good.
///
/// The delete guards go with it. Artifacts, run inputs, and image generations
/// are immutable, and they stay immutable: nothing may edit one, and nothing
/// may delete one on its own. What the guards blocked as a side effect was
/// deleting a whole run, which is not an edit of the record but the end of
/// it — so the `no_delete` triggers give way to the update guards alone, and
/// a purge deletes a run's rows in one transaction or none of them. Losing
/// the run is the point; losing a single artifact under a live run is still
/// impossible, because nothing but the purge deletes rows at all.
fn migrate_v8(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         ALTER TABLE runs RENAME COLUMN hidden TO archived;
         DROP TRIGGER artifacts_no_delete;
         DROP TRIGGER run_inputs_no_delete;
         DROP TRIGGER image_generations_no_delete;
         PRAGMA user_version = 8;",
    )?;
    Ok(())
}

/// Automatic approval, per run.
///
/// Answering the same permission request by hand all day is the cost of a
/// safe default, not a safety property in itself: the operator who trusts a
/// run wants it to keep going. `auto_approve` records that trust on the run
/// it was given for, so it survives a restart and never leaks to the next
/// run. Existing runs default to off — nobody is opted in by upgrading.
fn migrate_v9(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         ALTER TABLE runs ADD COLUMN auto_approve INTEGER NOT NULL DEFAULT 0;
         PRAGMA user_version = 9;",
    )?;
    Ok(())
}

/// Missions: the plan above the run.
///
/// `missions` holds the validated aggregate snapshot under compare-and-swap
/// exactly as `runs` does; `mission_inputs` is the immutable intent;
/// `mission_events` is the mission's own semantic history; `mission_runs`
/// indexes which package a run serves so a run can be traced upward without
/// decoding every mission. A run a mission remembers cannot be purged
/// (`ON DELETE RESTRICT`): the mission's history is the reason the run
/// existed.
fn migrate_v10(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         CREATE TABLE missions (
             id TEXT PRIMARY KEY,
             status TEXT NOT NULL,
             snapshot_schema_version INTEGER NOT NULL,
             snapshot_json TEXT NOT NULL,
             revision INTEGER NOT NULL DEFAULT 0,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE TABLE mission_inputs (
             mission_id TEXT PRIMARY KEY,
             schema_version INTEGER NOT NULL,
             title TEXT NOT NULL,
             goal TEXT NOT NULL,
             source_repo_path TEXT NOT NULL,
             base_commit TEXT NOT NULL,
             created_at TEXT NOT NULL,
             FOREIGN KEY (mission_id) REFERENCES missions(id) ON DELETE RESTRICT
         );
         CREATE TRIGGER mission_inputs_no_update
         BEFORE UPDATE ON mission_inputs
         BEGIN
             SELECT RAISE(ABORT, 'mission inputs are immutable');
         END;
         CREATE TABLE mission_events (
             mission_id TEXT NOT NULL,
             sequence INTEGER NOT NULL CHECK (sequence > 0),
             event_id TEXT NOT NULL UNIQUE,
             event_type TEXT NOT NULL,
             payload_json TEXT NOT NULL,
             occurred_at TEXT NOT NULL,
             recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
             PRIMARY KEY (mission_id, sequence),
             FOREIGN KEY (mission_id) REFERENCES missions(id) ON DELETE RESTRICT
         );
         CREATE TABLE mission_runs (
             run_id TEXT PRIMARY KEY,
             mission_id TEXT NOT NULL,
             package_id TEXT NOT NULL,
             created_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             FOREIGN KEY (mission_id) REFERENCES missions(id) ON DELETE RESTRICT
         );
         CREATE TRIGGER mission_runs_immutable
         BEFORE UPDATE ON mission_runs
         BEGIN
             SELECT RAISE(ABORT, 'mission run bindings are immutable');
         END;
         CREATE INDEX missions_status_updated_idx ON missions(status, updated_at DESC);
         CREATE INDEX mission_runs_mission_idx ON mission_runs(mission_id, package_id);
         PRAGMA user_version = 10;",
    )?;
    Ok(())
}

/// Handoffs: what a package's worker was told, as evidence.
///
/// The child run's immutable input already holds the rendered text; this
/// row binds it to the mission state it was rendered from — the contract's
/// hash, the dependencies and decisions it named — so a later reader can
/// tell whether the package the run served is still the package the plan
/// holds. Insert-only, one per run; a run attached by hand has none.
fn migrate_v11(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         CREATE TABLE mission_handoffs (
             run_id TEXT PRIMARY KEY,
             mission_id TEXT NOT NULL,
             package_id TEXT NOT NULL,
             contract_sha256 TEXT NOT NULL,
             task_sha256 TEXT NOT NULL,
             task_size INTEGER NOT NULL,
             dependencies_json TEXT NOT NULL,
             decision_ids_json TEXT NOT NULL,
             created_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES mission_runs(run_id) ON DELETE RESTRICT,
             FOREIGN KEY (mission_id) REFERENCES missions(id) ON DELETE RESTRICT
         );
         CREATE TRIGGER mission_handoffs_no_update
         BEFORE UPDATE ON mission_handoffs
         BEGIN
             SELECT RAISE(ABORT, 'mission handoffs are immutable');
         END;
         CREATE INDEX mission_handoffs_mission_idx ON mission_handoffs(mission_id, package_id);
         PRAGMA user_version = 11;",
    )?;
    Ok(())
}

/// v12: the lead session of a mission. One run per row, insert-only; the
/// newest row is the mission's current lead. A lead run cannot be deleted
/// while the row stands, for the same reason as a package run.
fn migrate_v12(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "
         CREATE TABLE mission_leads (
             run_id TEXT PRIMARY KEY,
             mission_id TEXT NOT NULL,
             created_at TEXT NOT NULL,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT,
             FOREIGN KEY (mission_id) REFERENCES missions(id) ON DELETE RESTRICT
         );
         CREATE TRIGGER mission_leads_immutable
         BEFORE UPDATE ON mission_leads
         BEGIN
             SELECT RAISE(ABORT, 'mission lead bindings are immutable');
         END;
         CREATE INDEX mission_leads_mission_idx ON mission_leads(mission_id, created_at);
         PRAGMA user_version = 12;",
    )?;
    Ok(())
}

/// Drive policy is separate from Mission lifecycle. Approval receipts prevent
/// a lead answer from being applied twice, including decision-only answers.
fn migrate_v13(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE mission_drives (
             mission_id TEXT PRIMARY KEY NOT NULL,
             max_parallel INTEGER NOT NULL CHECK (max_parallel BETWEEN 1 AND 16),
             policy_json TEXT NOT NULL,
             pause_reason TEXT,
             updated_at TEXT NOT NULL,
             FOREIGN KEY (mission_id) REFERENCES missions(id) ON DELETE RESTRICT
         );
         CREATE TABLE mission_lead_applications (
             mission_id TEXT NOT NULL,
             run_id TEXT NOT NULL,
             stage_id TEXT NOT NULL,
             answer_sha256 TEXT NOT NULL CHECK (length(answer_sha256) = 64),
             applied_at TEXT NOT NULL,
             PRIMARY KEY (mission_id, run_id, stage_id),
             FOREIGN KEY (mission_id) REFERENCES missions(id) ON DELETE RESTRICT,
             FOREIGN KEY (run_id) REFERENCES runs(id) ON DELETE RESTRICT
         );
         CREATE TRIGGER mission_lead_applications_immutable
         BEFORE UPDATE ON mission_lead_applications
         BEGIN
             SELECT RAISE(ABORT, 'lead approval receipts are immutable');
         END;
         PRAGMA user_version = 13;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, EventId, EventMetadata, Role, Run, RunId, StageDefinition, StageId,
        StageKind, WorkflowDefinition, WorkflowKind,
    };
    use crate::store::{ResolvedConfigSnapshot, RunInput, SqliteStore};
    use crate::workspace::{ApplyStatus, WorkspaceStatus};

    #[test]
    fn v1_database_upgrades_without_changing_existing_run() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        migrate_v1(&connection).unwrap();
        let mut store = SqliteStore { connection };
        let at: DateTime<Utc> = std::time::SystemTime::now().into();
        let config_id = ConfigSnapshotId::new("v1-config").unwrap();
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Fast,
            vec![StageDefinition::new(
                StageId::new("implementation").unwrap(),
                StageKind::Implementation,
                Role::Implementer,
                vec![],
            )],
        )
        .unwrap();
        let run = Run::new(RunId::from_u128(900), workflow, config_id.clone(), at);
        let config = ResolvedConfigSnapshot::new(config_id, 1, json!({"v": 1}), at).unwrap();
        let event = run.created_event(EventMetadata::new(EventId::from_u128(901), at));
        store.create_run(&run, &config, &[event]).unwrap();

        migrate(&store.connection).unwrap();

        assert_eq!(store.schema_version().unwrap(), DATABASE_SCHEMA_VERSION);
        assert_eq!(store.load_run(run.id()).unwrap().run, run);
        assert_eq!(store.load_events(run.id()).unwrap().len(), 1);
        assert_eq!(
            store
                .load_config_snapshot(run.config_snapshot_id())
                .unwrap(),
            config
        );
        assert!(store.load_workspace(run.id()).unwrap().is_none());
        assert!(store.load_apply_operation(run.id()).unwrap().is_none());
        assert!(store.load_run_input(run.id()).unwrap().is_none());
    }

    #[test]
    fn v2_database_adds_input_table_without_fabricating_legacy_input() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        migrate_v1(&connection).unwrap();
        migrate_v2(&connection).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            2
        );

        migrate(&connection).unwrap();

        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            DATABASE_SCHEMA_VERSION
        );
    }

    /// Upgrading is not consent: a database written before automatic
    /// approval existed comes back with every run still asking.
    #[test]
    fn v9_leaves_every_existing_run_asking_for_permission() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        migrate_v1(&connection).unwrap();
        migrate_v2(&connection).unwrap();
        migrate_v3(&connection).unwrap();
        migrate_v4(&connection).unwrap();
        migrate_v5(&connection).unwrap();
        migrate_v6(&connection).unwrap();
        migrate_v7(&connection).unwrap();
        migrate_v8(&connection).unwrap();
        let mut store = SqliteStore { connection };
        let at: DateTime<Utc> = std::time::SystemTime::now().into();
        let config_id = ConfigSnapshotId::new("v8-config").unwrap();
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Fast,
            vec![StageDefinition::new(
                StageId::new("implementation").unwrap(),
                StageKind::Implementation,
                Role::Implementer,
                vec![],
            )],
        )
        .unwrap();
        let run = Run::new(RunId::from_u128(910), workflow, config_id.clone(), at);
        let config = ResolvedConfigSnapshot::new(config_id, 1, json!({"v": 8}), at).unwrap();
        let event = run.created_event(EventMetadata::new(EventId::from_u128(911), at));
        store.create_run(&run, &config, &[event]).unwrap();

        migrate(&store.connection).unwrap();

        assert_eq!(store.schema_version().unwrap(), DATABASE_SCHEMA_VERSION);
        assert_eq!(store.load_run(run.id()).unwrap().run, run);
        assert!(
            !store.run_auto_approve(run.id()).unwrap(),
            "no run is opted in by upgrading"
        );
    }

    #[test]
    fn v3_database_adds_process_infrastructure_without_changing_existing_rows() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        migrate_v1(&connection).unwrap();
        migrate_v2(&connection).unwrap();
        migrate_v3(&connection).unwrap();
        let mut store = SqliteStore { connection };
        let at: DateTime<Utc> = std::time::SystemTime::now().into();
        let run_id = RunId::from_u128(950);
        let config_id = ConfigSnapshotId::new("v3-config").unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config_id.clone(),
            at,
        );
        let input = RunInput::new(run_id, "v3 input", at).unwrap();
        let config =
            ResolvedConfigSnapshot::new(config_id.clone(), 1, json!({"v": 3}), at).unwrap();
        let event = run.created_event(EventMetadata::new(EventId::from_u128(951), at));
        store
            .create_run_with_input(&run, &input, &config, &[event])
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO run_workspaces (
                     run_id, source_repo_path, git_common_dir, base_commit, worktree_path,
                     branch_name, mode, status, branch_owned, removal_head, last_error,
                     revision, created_at, updated_at
                 ) VALUES (?1, '/tmp/v3-source', '/tmp/v3-source/.git', ?2,
                           '/tmp/v3-worktree', 'senate/run-v3', 'branch', 'preparing',
                           0, NULL, NULL, 5, ?3, ?3)",
                rusqlite::params![run_id.to_string(), "a".repeat(40), at.to_rfc3339()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO run_apply_operations (
                     run_id, status, patch_hash, run_revision, last_error, revision,
                     created_at, updated_at
                 ) VALUES (?1, 'recorded', ?2, 0, NULL, 2, ?3, ?3)",
                rusqlite::params![run_id.to_string(), "b".repeat(64), at.to_rfc3339()],
            )
            .unwrap();

        migrate(&store.connection).unwrap();

        let process_count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM managed_processes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(process_count, 0);
        assert_eq!(store.load_run(run_id).unwrap().run, run);
        assert_eq!(store.load_events(run_id).unwrap().len(), 1);
        assert_eq!(store.load_config_snapshot(&config_id).unwrap(), config);
        assert_eq!(store.load_run_input(run_id).unwrap(), Some(input));
        assert_eq!(
            store.load_workspace(run_id).unwrap().unwrap().status(),
            WorkspaceStatus::Preparing
        );
        assert_eq!(
            store
                .load_apply_operation(run_id)
                .unwrap()
                .unwrap()
                .status(),
            ApplyStatus::Recorded
        );
        assert_eq!(
            store
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            DATABASE_SCHEMA_VERSION
        );
    }
}
