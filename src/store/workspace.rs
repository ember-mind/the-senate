use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::domain::{DomainEvent, Run, RunId};
use crate::workspace::{
    ApplyStatus, RunApplyOperation, RunWorkspace, WorkspaceMode, WorkspaceRevision, WorkspaceStatus,
};

use super::sqlite::{
    commit_run_update_transaction, format_timestamp, i64_to_u64, parse_timestamp, u64_to_i64,
};
use super::{CommitResult, RunRevision, SqliteStore, StoreError};

impl SqliteStore {
    /// Loads physical Git workspace state without touching filesystem.
    ///
    /// # Errors
    /// Returns typed `SQLite` or validation errors for malformed persisted state.
    pub fn load_workspace(&self, run_id: RunId) -> Result<Option<RunWorkspace>, StoreError> {
        load_workspace_from(&self.connection, run_id)
    }

    /// Every distinct source-checkout path a run has ever been started from,
    /// most recently used first.
    ///
    /// The natural place to look for a local checkout of a repository a task
    /// names but the chosen checkout does not hold: every path here is a
    /// checkout the operator has already pointed The Senate at, so it is
    /// reachable filesystem, not a guess. Ordered by the most recent
    /// `run_workspaces.created_at` for that path, because the checkout the
    /// operator used most recently is the one most likely to still be the
    /// right one.
    ///
    /// # Errors
    /// Returns typed `SQLite` errors.
    pub(crate) fn distinct_source_repository_paths(&self) -> Result<Vec<PathBuf>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT source_repo_path, MAX(created_at) AS last_used
             FROM run_workspaces
             GROUP BY source_repo_path
             ORDER BY last_used DESC",
        )?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(paths.into_iter().map(PathBuf::from).collect())
    }

    pub(crate) fn begin_workspace_preparation(
        &mut self,
        workspace: &RunWorkspace,
        run: &Run,
        expected_run_revision: RunRevision,
        event: &DomainEvent,
    ) -> Result<CommitResult, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT INTO run_workspaces (
                 run_id, source_repo_path, git_common_dir, base_commit, worktree_path,
                 branch_name, mode, status, branch_owned, removal_head, last_error,
                 revision, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?13)
             ON CONFLICT(run_id) DO NOTHING",
            workspace_params(workspace)?,
        )?;
        if inserted == 0 {
            return Err(StoreError::WorkspaceAlreadyExists(workspace.run_id()));
        }
        let result = commit_run_update_transaction(
            &transaction,
            run,
            expected_run_revision,
            std::slice::from_ref(event),
            false,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub(crate) fn finalize_workspace_preparation(
        &mut self,
        workspace: &RunWorkspace,
        expected_workspace_revision: WorkspaceRevision,
        run: &Run,
        expected_run_revision: RunRevision,
        event: &DomainEvent,
    ) -> Result<CommitResult, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        update_workspace_in(&transaction, workspace, expected_workspace_revision)?;
        let result = commit_run_update_transaction(
            &transaction,
            run,
            expected_run_revision,
            std::slice::from_ref(event),
            false,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub(crate) fn update_workspace(
        &mut self,
        workspace: &RunWorkspace,
        expected_revision: WorkspaceRevision,
    ) -> Result<WorkspaceRevision, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision = update_workspace_in(&transaction, workspace, expected_revision)?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Records a detached workspace adopting a branch of its own.
    ///
    /// Mode and branch name are otherwise creation-time identity, and
    /// [`Self::update_workspace`] refuses to move them on purpose — a run does
    /// not get to change what kind of workspace it has while it is using it.
    /// Adoption is the single exception, so it carries its own statement
    /// rather than widening the general update for every other caller. The
    /// two columns move together because the table's own CHECK will not have
    /// them any other way.
    ///
    /// # Errors
    /// Returns typed `SQLite` errors, or concurrent modification when another
    /// writer has moved the workspace on.
    pub(crate) fn adopt_workspace_branch(
        &mut self,
        workspace: &RunWorkspace,
        expected_revision: WorkspaceRevision,
    ) -> Result<WorkspaceRevision, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_revision = expected_revision
            .value()
            .checked_add(1)
            .ok_or(StoreError::IntegerRange("next workspace revision"))?;
        let changed = transaction.execute(
            "UPDATE run_workspaces
             SET branch_name = ?1, mode = ?2, branch_owned = ?3,
                 revision = ?4, updated_at = ?5
             WHERE run_id = ?6 AND revision = ?7",
            params![
                workspace.branch_name(),
                workspace.mode().as_str(),
                workspace.branch_owned(),
                u64_to_i64(next_revision, "next workspace revision")?,
                format_timestamp(workspace.updated_at()),
                workspace.run_id().to_string(),
                u64_to_i64(expected_revision.value(), "expected workspace revision")?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::WorkspaceConcurrentModification {
                run_id: workspace.run_id(),
                expected: expected_revision.value(),
            });
        }
        transaction.commit()?;
        Ok(WorkspaceRevision::new(next_revision))
    }

    /// Records one workspace standing on a new base commit, dropping the
    /// spent apply intent that was written against the old one.
    ///
    /// The base is creation-time identity everywhere else — [`update_workspace`]
    /// refuses to move it on purpose, the way it refuses to move the branch —
    /// so rebase carries its own statement rather than widening the general
    /// update for callers that must never touch it.
    ///
    /// The delete is part of the same transaction because the two facts are
    /// one fact: an apply intent pins the patch hash of the base it was
    /// written against, and a rebase makes that patch unreachable. Leaving it
    /// behind would answer the next apply with a hash mismatch it could never
    /// resolve. Only a `failed` intent is ever cleared; a `prepared` or
    /// `applied_to_source` one means changes may already be in the checkout,
    /// and the caller refuses the rebase before reaching here.
    ///
    /// [`update_workspace`]: Self::update_workspace
    ///
    /// # Errors
    /// Returns typed `SQLite` errors, or concurrent modification when another
    /// writer has moved the workspace or the run on.
    pub(crate) fn rebase_workspace_base(
        &mut self,
        workspace: &RunWorkspace,
        expected_revision: WorkspaceRevision,
        run: &Run,
        expected_run_revision: RunRevision,
        event: &DomainEvent,
    ) -> Result<CommitResult, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_revision = expected_revision
            .value()
            .checked_add(1)
            .ok_or(StoreError::IntegerRange("next workspace revision"))?;
        let changed = transaction.execute(
            "UPDATE run_workspaces
             SET base_commit = ?1, revision = ?2, updated_at = ?3
             WHERE run_id = ?4 AND revision = ?5",
            params![
                workspace.base_commit(),
                u64_to_i64(next_revision, "next workspace revision")?,
                format_timestamp(workspace.updated_at()),
                workspace.run_id().to_string(),
                u64_to_i64(expected_revision.value(), "expected workspace revision")?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::WorkspaceConcurrentModification {
                run_id: workspace.run_id(),
                expected: expected_revision.value(),
            });
        }
        transaction.execute(
            "DELETE FROM run_apply_operations WHERE run_id = ?1 AND status = 'failed'",
            [workspace.run_id().to_string()],
        )?;
        let result = commit_run_update_transaction(
            &transaction,
            run,
            expected_run_revision,
            std::slice::from_ref(event),
            false,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    /// Loads persisted recoverable apply intent for one run.
    ///
    /// # Errors
    /// Returns typed `SQLite` or validation errors for malformed persisted state.
    pub fn load_apply_operation(
        &self,
        run_id: RunId,
    ) -> Result<Option<RunApplyOperation>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT status, patch_hash, run_revision, last_error, revision,
                        created_at, updated_at
                 FROM run_apply_operations WHERE run_id = ?1",
                [run_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(status, patch_hash, run_revision, last_error, revision, created_at, updated_at)| {
                RunApplyOperation::from_stored(
                    run_id,
                    ApplyStatus::from_str(&status).map_err(workspace_model_error)?,
                    patch_hash,
                    i64_to_u64(run_revision, "apply run revision")?,
                    last_error,
                    i64_to_u64(revision, "apply revision")?,
                    parse_timestamp(&created_at)?,
                    parse_timestamp(&updated_at)?,
                )
                .map_err(workspace_model_error)
            },
        )
        .transpose()
    }

    pub(crate) fn insert_apply_operation(
        &mut self,
        run_id: RunId,
        patch_hash: &str,
        run_revision: RunRevision,
        now: DateTime<Utc>,
    ) -> Result<RunApplyOperation, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT INTO run_apply_operations (
                 run_id, status, patch_hash, run_revision, last_error, revision,
                 created_at, updated_at
             )
             SELECT ?1, 'prepared', ?2, ?3, NULL, 0, ?4, ?4
             FROM runs JOIN run_workspaces ON run_workspaces.run_id = runs.id
             WHERE runs.id = ?1 AND runs.revision = ?3
               AND runs.status = 'completed' AND run_workspaces.status = 'ready'
             ON CONFLICT(run_id) DO NOTHING",
            params![
                run_id.to_string(),
                patch_hash,
                u64_to_i64(run_revision.value(), "apply run revision")?,
                format_timestamp(&now),
            ],
        )?;
        if inserted == 0 {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM run_apply_operations WHERE run_id = ?1",
                    [run_id.to_string()],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                return Err(StoreError::ApplyOperationAlreadyExists(run_id));
            }
            return Err(StoreError::ConcurrentModification {
                run_id,
                expected: run_revision.value(),
            });
        }
        transaction.commit()?;
        self.load_apply_operation(run_id)?
            .ok_or(StoreError::ApplyOperationNotFound(run_id))
    }

    pub(crate) fn update_apply_operation(
        &mut self,
        operation: &RunApplyOperation,
        status: ApplyStatus,
        last_error: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<RunApplyOperation, StoreError> {
        let now = now.max(*operation.updated_at());
        let next_revision = operation
            .revision()
            .checked_add(1)
            .ok_or(StoreError::IntegerRange("next apply revision"))?;
        let changed = self.connection.execute(
            "UPDATE run_apply_operations
             SET status = ?1, last_error = ?2, revision = ?3, updated_at = ?4
             WHERE run_id = ?5 AND revision = ?6",
            params![
                status.as_str(),
                last_error,
                u64_to_i64(next_revision, "next apply revision")?,
                format_timestamp(&now),
                operation.run_id().to_string(),
                u64_to_i64(operation.revision(), "expected apply revision")?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::ApplyOperationConcurrentModification {
                run_id: operation.run_id(),
                expected: operation.revision(),
            });
        }
        self.load_apply_operation(operation.run_id())?
            .ok_or(StoreError::ApplyOperationNotFound(operation.run_id()))
    }

    pub(crate) fn finalize_apply_operation(
        &mut self,
        operation: &RunApplyOperation,
        run: &Run,
        expected_run_revision: RunRevision,
        event: &DomainEvent,
        now: DateTime<Utc>,
    ) -> Result<CommitResult, StoreError> {
        let now = now.max(*operation.updated_at());
        let next_apply_revision = operation
            .revision()
            .checked_add(1)
            .ok_or(StoreError::IntegerRange("next apply revision"))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE run_apply_operations
             SET status = 'recorded', last_error = NULL, revision = ?1, updated_at = ?2
             WHERE run_id = ?3 AND revision = ?4
               AND status IN ('prepared', 'applied_to_source')",
            params![
                u64_to_i64(next_apply_revision, "next apply revision")?,
                format_timestamp(&now),
                operation.run_id().to_string(),
                u64_to_i64(operation.revision(), "expected apply revision")?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::ApplyOperationConcurrentModification {
                run_id: operation.run_id(),
                expected: operation.revision(),
            });
        }
        let result = commit_run_update_transaction(
            &transaction,
            run,
            expected_run_revision,
            std::slice::from_ref(event),
            true,
        )?;
        transaction.commit()?;
        Ok(result)
    }
}

fn load_workspace_from(
    connection: &rusqlite::Connection,
    run_id: RunId,
) -> Result<Option<RunWorkspace>, StoreError> {
    let row = connection
        .query_row(
            "SELECT source_repo_path, git_common_dir, base_commit, worktree_path,
                    branch_name, mode, status, branch_owned, removal_head, last_error,
                    revision, created_at, updated_at
             FROM run_workspaces WHERE run_id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(
            source,
            common,
            base,
            worktree,
            branch,
            mode,
            status,
            branch_owned,
            removal_head,
            last_error,
            revision,
            created_at,
            updated_at,
        )| {
            RunWorkspace::from_stored(
                run_id,
                PathBuf::from(source),
                PathBuf::from(common),
                base,
                PathBuf::from(worktree),
                branch,
                WorkspaceMode::from_str(&mode).map_err(workspace_model_error)?,
                WorkspaceStatus::from_str(&status).map_err(workspace_model_error)?,
                branch_owned,
                removal_head,
                last_error,
                WorkspaceRevision::new(i64_to_u64(revision, "workspace revision")?),
                parse_timestamp(&created_at)?,
                parse_timestamp(&updated_at)?,
            )
            .map_err(workspace_model_error)
        },
    )
    .transpose()
}

fn update_workspace_in(
    transaction: &rusqlite::Transaction<'_>,
    workspace: &RunWorkspace,
    expected_revision: WorkspaceRevision,
) -> Result<WorkspaceRevision, StoreError> {
    let next_revision = expected_revision
        .value()
        .checked_add(1)
        .ok_or(StoreError::IntegerRange("next workspace revision"))?;
    let changed = transaction.execute(
        "UPDATE run_workspaces
         SET status = ?1, branch_owned = ?2, removal_head = ?3, last_error = ?4,
             revision = ?5, updated_at = ?6
         WHERE run_id = ?7 AND revision = ?8",
        params![
            workspace.status().as_str(),
            workspace.branch_owned(),
            workspace.removal_head(),
            workspace.last_error(),
            u64_to_i64(next_revision, "next workspace revision")?,
            format_timestamp(workspace.updated_at()),
            workspace.run_id().to_string(),
            u64_to_i64(expected_revision.value(), "expected workspace revision")?,
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::WorkspaceConcurrentModification {
            run_id: workspace.run_id(),
            expected: expected_revision.value(),
        });
    }
    Ok(WorkspaceRevision::new(next_revision))
}

fn workspace_params(workspace: &RunWorkspace) -> Result<[rusqlite::types::Value; 13], StoreError> {
    Ok([
        workspace.run_id().to_string().into(),
        path_string(workspace.source_repo_path())?.into(),
        path_string(workspace.git_common_dir())?.into(),
        workspace.base_commit().to_owned().into(),
        path_string(workspace.worktree_path())?.into(),
        workspace.branch_name().map(ToOwned::to_owned).into(),
        workspace.mode().as_str().to_owned().into(),
        workspace.status().as_str().to_owned().into(),
        workspace.branch_owned().into(),
        workspace.removal_head().map(ToOwned::to_owned).into(),
        workspace.last_error().map(ToOwned::to_owned).into(),
        format_timestamp(workspace.created_at()).into(),
        format_timestamp(workspace.updated_at()).into(),
    ])
}

fn path_string(path: &Path) -> Result<String, StoreError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StoreError::NonUtf8WorkspacePath(path.to_path_buf()))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err adapter consumes its error argument"
)]
fn workspace_model_error(error: crate::workspace::WorkspaceError) -> StoreError {
    StoreError::InvalidWorkspaceRecord(error.to_string())
}
