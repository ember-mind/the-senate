use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::domain::{RunId, StageId};
use crate::process::{
    BackendSessionId, ExitResult, ManagedProcess, ManagedProcessId, ManagedProcessStatus,
    OutputChunk, OutputCursor, OutputStream, ProcessError, ProcessRevision,
};
use crate::workspace::{ApplyStatus, WorkspaceStatus};

use super::sqlite::{format_timestamp, i64_to_u64, parse_timestamp, u64_to_i64};
use super::{SqliteStore, StoreError};

impl SqliteStore {
    /// Persists one immutable process intent after rechecking execution guards.
    ///
    /// # Errors
    /// Rejects unknown stages, non-ready/mismatched workspaces, active apply,
    /// duplicate identities/attempts, malformed records, or `SQLite` failures.
    #[allow(
        clippy::too_many_lines,
        reason = "single guarded insert keeps intent validation and diagnostics together"
    )]
    pub(crate) fn insert_managed_process_intent(
        &mut self,
        process: &ManagedProcess,
    ) -> Result<ManagedProcess, ProcessError> {
        let loaded = self.load_run(process.run_id())?;
        if loaded.run.stage(process.stage_id()).is_none() {
            return Err(ProcessError::UnknownStage {
                run_id: process.run_id(),
                stage_id: process.stage_id().clone(),
            });
        }
        let workspace = self.load_workspace(process.run_id())?.ok_or(
            StoreError::ExecutionWorkspaceNotReady {
                run_id: process.run_id(),
                status: None,
            },
        )?;
        if workspace.status() != WorkspaceStatus::Ready {
            return Err(StoreError::ExecutionWorkspaceNotReady {
                run_id: process.run_id(),
                status: Some(workspace.status().as_str().to_owned()),
            }
            .into());
        }
        if workspace.worktree_path() != process.spec().working_directory() {
            return Err(ProcessError::WorkspaceMismatch(
                process.spec().working_directory().to_path_buf(),
            ));
        }

        let manifest = process.manifest_json()?;
        let (exit_code, term_signal, runner_error) = exit_columns(process.exit_result());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_execution_guard(&transaction, process.run_id())?;
        let inserted = transaction.execute(
            "INSERT INTO managed_processes (
                 id, run_id, stage_id, attempt, invocation, backend_kind, backend_session_id,
                 status, spec_schema_version, spec_json, command_fingerprint,
                 stdout_offset, stdout_cursor_revision, stderr_offset,
                 stderr_cursor_revision, exit_code, term_signal, runner_error,
                 interrupt_requested, last_error, revision, created_at, updated_at,
                 started_at, finished_at
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23,
                 ?24, ?25
             ) ON CONFLICT DO NOTHING",
            params![
                process.id().to_string(),
                process.run_id().to_string(),
                process.stage_id().as_str(),
                i64::from(process.attempt()),
                i64::from(process.invocation()),
                process.backend_kind(),
                process.backend_session_id().as_str(),
                process.status().as_str(),
                i64::from(process.spec_schema_version()),
                manifest,
                process.command_fingerprint(),
                u64_to_i64(
                    process.cursor(OutputStream::Stdout).offset(),
                    "stdout offset"
                )?,
                u64_to_i64(
                    process.cursor(OutputStream::Stdout).revision(),
                    "stdout cursor revision"
                )?,
                u64_to_i64(
                    process.cursor(OutputStream::Stderr).offset(),
                    "stderr offset"
                )?,
                u64_to_i64(
                    process.cursor(OutputStream::Stderr).revision(),
                    "stderr cursor revision"
                )?,
                exit_code,
                term_signal,
                runner_error,
                process.interrupt_requested(),
                process.last_error(),
                u64_to_i64(process.revision().value(), "process revision")?,
                format_timestamp(process.created_at()),
                format_timestamp(process.updated_at()),
                process.started_at().map(format_timestamp),
                process.finished_at().map(format_timestamp),
            ],
        )?;
        if inserted == 0 {
            if process_exists(&transaction, process.id())? {
                return Err(ProcessError::ProcessConflict(process.id()));
            }
            if attempt_exists(
                &transaction,
                process.run_id(),
                process.stage_id(),
                process.attempt(),
                process.invocation(),
            )? {
                return Err(ProcessError::AttemptConflict {
                    run_id: process.run_id(),
                    stage_id: process.stage_id().clone(),
                    attempt: process.attempt(),
                    invocation: process.invocation(),
                });
            }
            return Err(ProcessError::InvalidStoredProcess(
                "process insert did not create a row",
            ));
        }
        transaction.commit()?;
        self.load_managed_process(process.id())
    }

    /// Loads one managed-process infrastructure record without filesystem effects.
    ///
    /// # Errors
    /// Returns not-found, malformed persisted state, or `SQLite` failures.
    pub fn load_managed_process(
        &self,
        process_id: ManagedProcessId,
    ) -> Result<ManagedProcess, ProcessError> {
        load_process_from(&self.connection, process_id)?
            .ok_or(ProcessError::ProcessNotFound(process_id))
    }

    /// Finds one immutable stage attempt.
    ///
    /// # Errors
    /// Returns malformed persisted state or `SQLite` failures.
    pub fn load_managed_process_for_attempt(
        &self,
        run_id: RunId,
        stage_id: &StageId,
        attempt: u32,
    ) -> Result<Option<ManagedProcess>, ProcessError> {
        let id = self
            .connection
            .query_row(
                "SELECT id FROM managed_processes
                 WHERE run_id = ?1 AND stage_id = ?2 AND attempt = ?3
                 ORDER BY invocation DESC LIMIT 1",
                params![run_id.to_string(), stage_id.as_str(), i64::from(attempt)],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        id.map(|id| {
            let id = id
                .parse()
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid process ID"))?;
            self.load_managed_process(id)
        })
        .transpose()
    }

    /// Lists one run's managed attempts in creation order.
    ///
    /// # Errors
    /// Returns malformed persisted state or `SQLite` failures.
    pub fn list_managed_processes(
        &self,
        run_id: RunId,
    ) -> Result<Vec<ManagedProcess>, ProcessError> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM managed_processes
             WHERE run_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut processes = Vec::new();
        for id in rows {
            let id = id?
                .parse()
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid process ID"))?;
            processes.push(self.load_managed_process(id)?);
        }
        Ok(processes)
    }

    pub(crate) fn update_managed_process(
        &mut self,
        process: &ManagedProcess,
        expected_revision: ProcessRevision,
        require_execution_guard: bool,
    ) -> Result<ManagedProcess, ProcessError> {
        let next_revision = expected_revision
            .value()
            .checked_add(1)
            .ok_or(StoreError::IntegerRange("next process revision"))?;
        let (exit_code, term_signal, runner_error) = exit_columns(process.exit_result());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if require_execution_guard {
            ensure_execution_guard(&transaction, process.run_id())?;
        }
        let changed = transaction.execute(
            "UPDATE managed_processes
                 SET status = ?1, exit_code = ?2, term_signal = ?3, runner_error = ?4,
                 interrupt_requested = ?5, last_error = ?6, revision = ?7, updated_at = ?8,
                 started_at = ?9, finished_at = ?10
             WHERE id = ?11 AND revision = ?12",
            params![
                process.status().as_str(),
                exit_code,
                term_signal,
                runner_error,
                process.interrupt_requested(),
                process.last_error(),
                u64_to_i64(next_revision, "next process revision")?,
                format_timestamp(process.updated_at()),
                process.started_at().map(format_timestamp),
                process.finished_at().map(format_timestamp),
                process.id().to_string(),
                u64_to_i64(expected_revision.value(), "expected process revision")?,
            ],
        )?;
        if changed == 0 {
            return Err(ProcessError::ConcurrentModification {
                process_id: process.id(),
                expected: expected_revision.value(),
            });
        }
        transaction.commit()?;
        self.load_managed_process(process.id())
    }

    pub(crate) fn acknowledge_process_output(
        &mut self,
        chunk: &OutputChunk,
        acknowledged_end: u64,
    ) -> Result<OutputCursor, ProcessError> {
        acknowledge_process_output_row(&self.connection, chunk, acknowledged_end)
    }
}

pub(crate) fn acknowledge_process_output_row(
    connection: &rusqlite::Connection,
    chunk: &OutputChunk,
    acknowledged_end: u64,
) -> Result<OutputCursor, ProcessError> {
    if acknowledged_end < chunk.start_offset() || acknowledged_end > chunk.end_offset() {
        return Err(ProcessError::InvalidAcknowledgement);
    }
    let next_revision = chunk
        .cursor_revision()
        .checked_add(1)
        .ok_or(StoreError::IntegerRange("next output cursor revision"))?;
    let (offset_column, revision_column) = match chunk.stream() {
        OutputStream::Stdout => ("stdout_offset", "stdout_cursor_revision"),
        OutputStream::Stderr => ("stderr_offset", "stderr_cursor_revision"),
    };
    let sql = format!(
        "UPDATE managed_processes
         SET {offset_column} = ?1, {revision_column} = ?2
         WHERE id = ?3 AND {offset_column} = ?4 AND {revision_column} = ?5"
    );
    let changed = connection.execute(
        &sql,
        params![
            u64_to_i64(acknowledged_end, "acknowledged output offset")?,
            u64_to_i64(next_revision, "next output cursor revision")?,
            chunk.process_id().to_string(),
            u64_to_i64(chunk.start_offset(), "expected output offset")?,
            u64_to_i64(chunk.cursor_revision(), "expected output cursor revision")?,
        ],
    )?;
    if changed == 0 {
        return Err(ProcessError::CursorConcurrentModification {
            process_id: chunk.process_id(),
            stream: chunk.stream(),
            expected: chunk.cursor_revision(),
        });
    }
    Ok(OutputCursor::new(acknowledged_end, next_revision))
}

pub(crate) fn ensure_execution_guard(
    connection: &rusqlite::Connection,
    run_id: RunId,
) -> Result<(), ProcessError> {
    let workspace_status = connection
        .query_row(
            "SELECT status FROM run_workspaces WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if workspace_status.as_deref() != Some(WorkspaceStatus::Ready.as_str()) {
        return Err(StoreError::ExecutionWorkspaceNotReady {
            run_id,
            status: workspace_status,
        }
        .into());
    }
    let apply_status = connection
        .query_row(
            "SELECT status FROM run_apply_operations
             WHERE run_id = ?1 AND status IN ('prepared', 'applied_to_source')",
            [run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if apply_status.is_some_and(|status| {
        matches!(
            ApplyStatus::from_str(&status),
            Ok(ApplyStatus::Prepared | ApplyStatus::AppliedToSource)
        )
    }) {
        return Err(StoreError::RunFrozenForApply(run_id).into());
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one row decoder validates every persisted projection before construction"
)]
fn load_process_from(
    connection: &rusqlite::Connection,
    process_id: ManagedProcessId,
) -> Result<Option<ManagedProcess>, ProcessError> {
    let row = connection
        .query_row(
            "SELECT run_id, stage_id, attempt, backend_kind, backend_session_id,
                    invocation, status, spec_schema_version, spec_json, command_fingerprint,
                    stdout_offset, stdout_cursor_revision, stderr_offset,
                    stderr_cursor_revision, exit_code, term_signal, runner_error,
                    interrupt_requested, last_error, revision, created_at, updated_at,
                    started_at, finished_at
             FROM managed_processes WHERE id = ?1",
            [process_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, Option<i64>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, bool>(17)?,
                    row.get::<_, Option<String>>(18)?,
                    row.get::<_, i64>(19)?,
                    row.get::<_, String>(20)?,
                    row.get::<_, String>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    row.get::<_, Option<String>>(23)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(
            run_id,
            stage_id,
            attempt,
            backend_kind,
            backend_session_id,
            invocation,
            status,
            spec_schema_version,
            spec_json,
            command_fingerprint,
            stdout_offset,
            stdout_cursor_revision,
            stderr_offset,
            stderr_cursor_revision,
            exit_code,
            term_signal,
            runner_error,
            interrupt_requested,
            last_error,
            revision,
            created_at,
            updated_at,
            started_at,
            finished_at,
        )| {
            let spec_schema_version = u32::try_from(spec_schema_version)
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid spec schema version"))?;
            if !matches!(spec_schema_version, 1 | 2) {
                return Err(ProcessError::InvalidStoredProcess(
                    "unsupported spec schema version",
                ));
            }
            let run_id = run_id
                .parse()
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid run ID"))?;
            let stage_id = StageId::new(stage_id)
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid stage ID"))?;
            let attempt = u32::try_from(attempt)
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid attempt"))?;
            let invocation = u32::try_from(invocation)
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid invocation"))?;
            let backend_session_id = BackendSessionId::new(backend_session_id)?;
            let status = ManagedProcessStatus::from_str(&status)?;
            let exit_result = decode_exit_result(exit_code, term_signal, runner_error)?;
            ManagedProcess::from_stored(
                process_id,
                run_id,
                stage_id,
                attempt,
                invocation,
                backend_kind,
                backend_session_id,
                status,
                spec_schema_version,
                &spec_json,
                command_fingerprint,
                OutputCursor::new(
                    i64_to_u64(stdout_offset, "stdout offset")?,
                    i64_to_u64(stdout_cursor_revision, "stdout cursor revision")?,
                ),
                OutputCursor::new(
                    i64_to_u64(stderr_offset, "stderr offset")?,
                    i64_to_u64(stderr_cursor_revision, "stderr cursor revision")?,
                ),
                exit_result,
                interrupt_requested,
                last_error,
                ProcessRevision::new(i64_to_u64(revision, "process revision")?),
                parse_timestamp(&created_at)?,
                parse_timestamp(&updated_at)?,
                started_at.as_deref().map(parse_timestamp).transpose()?,
                finished_at.as_deref().map(parse_timestamp).transpose()?,
            )
        },
    )
    .transpose()
}

fn exit_columns(exit: Option<&ExitResult>) -> (Option<i64>, Option<i64>, Option<&str>) {
    match exit {
        Some(ExitResult::ExitCode { code }) => (Some(i64::from(*code)), None, None),
        Some(ExitResult::Signal { signal }) => (None, Some(i64::from(*signal)), None),
        Some(ExitResult::RunnerError { message }) => (None, None, Some(message)),
        None => (None, None, None),
    }
}

fn decode_exit_result(
    exit_code: Option<i64>,
    term_signal: Option<i64>,
    runner_error: Option<String>,
) -> Result<Option<ExitResult>, ProcessError> {
    match (exit_code, term_signal, runner_error) {
        (Some(code), None, None) => Ok(Some(ExitResult::ExitCode {
            code: i32::try_from(code)
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid exit code"))?,
        })),
        (None, Some(signal), None) => Ok(Some(ExitResult::Signal {
            signal: i32::try_from(signal)
                .map_err(|_| ProcessError::InvalidStoredProcess("invalid signal"))?,
        })),
        (None, None, Some(message)) => Ok(Some(ExitResult::RunnerError { message })),
        (None, None, None) => Ok(None),
        _ => Err(ProcessError::InvalidStoredProcess(
            "multiple exit outcomes stored",
        )),
    }
}

fn process_exists(
    connection: &rusqlite::Connection,
    process_id: ManagedProcessId,
) -> Result<bool, ProcessError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM managed_processes WHERE id = ?1",
            [process_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn attempt_exists(
    connection: &rusqlite::Connection,
    run_id: RunId,
    stage_id: &StageId,
    attempt: u32,
    invocation: u32,
) -> Result<bool, ProcessError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM managed_processes
             WHERE run_id = ?1 AND stage_id = ?2 AND attempt = ?3 AND invocation = ?4",
            params![
                run_id.to_string(),
                stage_id.as_str(),
                i64::from(attempt),
                i64::from(invocation)
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use chrono::{DateTime, Duration, TimeZone, Utc};
    use rusqlite::params;
    use serde_json::json;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, EventId, EventMetadata, Run, WorkflowDefinition, WorkflowKind,
    };
    use crate::process::{ManagedProcessId, ProcessSpec};
    use crate::store::ResolvedConfigSnapshot;

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 14, 12, 0, second)
            .single()
            .unwrap()
    }

    fn fixture() -> (SqliteStore, ManagedProcess) {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let run_id = RunId::from_u128(700);
        let config_id = ConfigSnapshotId::new("m6-store").unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Fast),
            config_id.clone(),
            at(0),
        );
        let config =
            ResolvedConfigSnapshot::new(config_id, 1, json!({"provider": "fixture"}), at(0))
                .unwrap();
        let event = run.created_event(EventMetadata::new(EventId::from_u128(701), at(0)));
        store.create_run(&run, &config, &[event]).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO run_workspaces (
                     run_id, source_repo_path, git_common_dir, base_commit, worktree_path,
                     branch_name, mode, status, branch_owned, removal_head, last_error,
                     revision, created_at, updated_at
                 ) VALUES (?1, '/tmp/source', '/tmp/source/.git', ?2, '/tmp/worktree',
                           'senate/run-test', 'branch', 'ready', 1, NULL, NULL, 0, ?3, ?3)",
                params![run_id.to_string(), "a".repeat(40), format_timestamp(&at(1))],
            )
            .unwrap();
        let process_id = ManagedProcessId::from_u128(702);
        let spec = ProcessSpec::new(
            PathBuf::from("/bin/echo"),
            vec![OsString::from("safe")],
            PathBuf::from("/tmp/worktree"),
            BTreeMap::new(),
            PathBuf::from("/tmp/process/stdout.log"),
            PathBuf::from("/tmp/process/stderr.log"),
        )
        .unwrap();
        let process = ManagedProcess::preparing(
            process_id,
            run_id,
            StageId::new("implementation").unwrap(),
            0,
            1,
            "tmux".to_owned(),
            BackendSessionId::for_process(process_id),
            spec,
            at(2),
        )
        .unwrap();
        (store, process)
    }

    #[test]
    fn process_round_trip_identity_is_immutable_and_lifecycle_is_cas_protected() {
        let (mut store, process) = fixture();
        let stored = store.insert_managed_process_intent(&process).unwrap();
        assert_eq!(stored, process);

        let mut first = stored.clone();
        first
            .transition(ManagedProcessStatus::Starting, at(3), None, None)
            .unwrap();
        let updated = store
            .update_managed_process(&first, stored.revision(), true)
            .unwrap();
        assert_eq!(updated.revision().value(), 1);

        let mut stale = stored;
        stale
            .transition(ManagedProcessStatus::Starting, at(3), None, None)
            .unwrap();
        assert!(matches!(
            store.update_managed_process(&stale, stale.revision(), true),
            Err(ProcessError::ConcurrentModification { .. })
        ));
        assert!(
            store
                .connection
                .execute(
                    "UPDATE managed_processes SET spec_json = '{}' WHERE id = ?1",
                    [process.id().to_string()],
                )
                .is_err()
        );
        assert_eq!(store.load_managed_process(process.id()).unwrap(), updated);
    }

    #[test]
    fn process_intent_rechecks_workspace_and_active_apply_guards() {
        let (mut store, process) = fixture();
        store
            .connection
            .execute(
                "UPDATE run_workspaces SET status = 'broken' WHERE run_id = ?1",
                [process.run_id().to_string()],
            )
            .unwrap();
        assert!(matches!(
            store.insert_managed_process_intent(&process),
            Err(ProcessError::Store(
                StoreError::ExecutionWorkspaceNotReady { .. }
            ))
        ));

        store
            .connection
            .execute(
                "UPDATE run_workspaces SET status = 'ready' WHERE run_id = ?1",
                [process.run_id().to_string()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO run_apply_operations (
                     run_id, status, patch_hash, run_revision, last_error, revision,
                     created_at, updated_at
                 ) VALUES (?1, 'prepared', ?2, 0, NULL, 0, ?3, ?3)",
                params![
                    process.run_id().to_string(),
                    "b".repeat(64),
                    format_timestamp(&(at(2) + Duration::seconds(1)))
                ],
            )
            .unwrap();
        assert!(matches!(
            store.insert_managed_process_intent(&process),
            Err(ProcessError::Store(StoreError::RunFrozenForApply(_)))
        ));
    }

    #[test]
    fn corrupt_terminal_projection_never_loads_as_valid_process() {
        let (mut store, process) = fixture();
        store.insert_managed_process_intent(&process).unwrap();
        store
            .connection
            .execute(
                "UPDATE managed_processes SET status = 'exited' WHERE id = ?1",
                [process.id().to_string()],
            )
            .unwrap();
        assert!(matches!(
            store.load_managed_process(process.id()),
            Err(ProcessError::InvalidStoredProcess(_))
        ));
    }
}
