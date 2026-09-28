use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::domain::{
    DomainEventKind, EventId, EventMetadata, Run, RunId, RunStatus, RunTransition, StageKind,
    StageStatus,
};
use crate::git::{
    GitRepository, PUBLISH_TARGET_REF, apply_patch, branch_exists, branch_tip, check_patch,
    commit_all_in_worktree, count_commits_between, create_branch_in_worktree, create_worktree,
    delete_owned_branch, delete_ref, detach_worktree, fetch_branch, generate_patch,
    generate_patch_preview, inspect_worktree, is_ancestor, push_branch, push_commit_to_branch,
    rebase_worktree_onto, remote_url, remove_worktree, source_is_clean, tree_is_clean,
};
use crate::store::{RunInput, RunRevision, SqliteStore, worktree_root};

use super::branch_name;
use super::github::{GhClient, PullRequestRef};
use super::pull_request::PullRequestDraft;
use super::setup;
use super::{
    ApplyStatus, RunApplyOperation, RunWorkspace, WorkspaceError, WorkspaceMode, WorkspaceStatus,
};

/// The head of a pull request `origin` serves: its branch and the commit that
/// branch pointed at when it was fetched.
struct PullRequestTip {
    branch: String,
    commit: String,
}

/// What one publish actually did: the branch and commit that reached the
/// remote, and what became of the pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishReceipt {
    pub branch: String,
    pub commit: String,
    pub pull_request: PullRequestStatus,
    /// What the publish had to do beyond pushing the run's commit as it
    /// stood: replay it on a pull request that moved, or fall back to the
    /// run's own branch. A run that reviewed a pull request and then fixed it
    /// expects its commits to land on that pull request, so neither a rewrite
    /// nor a fallback is ever left unexplained.
    pub note: Option<String>,
}

/// What one rebase moved: the base the run's delta stood on, the base it
/// stands on now, and how many commits the checkout had gained in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseReceipt {
    pub from_base: String,
    pub to_base: String,
    pub commits_gained: u64,
}

/// Where a publish's commit is meant to land.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PublishTarget {
    /// The head branch of the pull request the run was asked to fix, whose
    /// commits the run is built on.
    PullRequestBranch { branch: String, url: String },
    /// The head branch of the pull request the run was asked to fix, which
    /// gained commits while the run worked. The run's delta is replayed on
    /// its tip before it is pushed, because that pull request is still where
    /// the work belongs.
    RebaseOntoPullRequest(MovedPullRequest),
    /// The branch the run owns, with the reason the named pull request could
    /// not be updated when there was one.
    OwnBranch(Option<String>),
}

/// A pull request the run was asked to fix that gained commits while it
/// worked: its head branch, its URL, and the tip the run's delta has to be
/// replayed on to reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MovedPullRequest {
    branch: String,
    url: String,
    tip: String,
}

/// The pull-request half of a publish, which is allowed to fall short without
/// failing the publish: once the branch is pushed, the work is safe, and a
/// missing or unauthenticated `gh` only costs the convenience.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullRequestStatus {
    Created(String),
    AlreadyExists(String),
    Unavailable(String),
}

/// What a removal does with the branch the worktree was checked out on.
///
/// The two callers want opposite things. Discarding a run throws the work away,
/// so the branch goes with it. Reclaiming the worktree of a run that has
/// already landed keeps the work and wants only the disk back — and after an
/// apply the branch holds the only commits of that work, because apply leaves
/// the change unstaged in the source checkout. Deleting it there would turn
/// "give me the space back" into data loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchDisposition {
    /// Delete the branch along with the worktree.
    Delete,
    /// Leave the branch standing; take only the worktree.
    Keep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconciliationOutcome {
    Ready(RunWorkspace),
    Removed(RunWorkspace),
    Unchanged(RunWorkspace),
    Broken(RunWorkspace),
}

pub struct WorkspaceManager {
    root: PathBuf,
    git: crate::git::Git,
    #[cfg(test)]
    fault: Option<FaultPoint>,
}

impl WorkspaceManager {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            git: crate::git::Git::default(),
            #[cfg(test)]
            fault: None,
        }
    }

    #[cfg(test)]
    fn with_fault(root: impl Into<PathBuf>, fault: FaultPoint) -> Self {
        Self {
            root: root.into(),
            git: crate::git::Git::default(),
            fault: Some(fault),
        }
    }

    /// Creates default manager rooted at The Senate data directory.
    ///
    /// # Errors
    /// Returns data-path resolution error when no override or home exists.
    pub fn from_environment() -> Result<Self, WorkspaceError> {
        Ok(Self::new(worktree_root()?))
    }

    /// Persists preparation intent, creates isolated worktree, validates it,
    /// then atomically marks workspace and logical run ready.
    ///
    /// # Errors
    /// Returns typed lifecycle, persistence, repository, ownership, or Git errors.
    pub fn prepare_run_workspace(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        repository_path: impl AsRef<Path>,
    ) -> Result<RunWorkspace, WorkspaceError> {
        self.prepare_run_workspace_with(store, run_id, repository_path, None)
    }

    /// [`Self::prepare_run_workspace`], starting from the pull request the
    /// task names when `gh` can resolve it.
    ///
    /// A run asked about a pull request is working on that pull request's
    /// change, so its worktree has to hold that change. Started from the
    /// operator's checkout instead, the stages never see the code under
    /// review — an editing stage rewrites it from scratch — and publish can
    /// never fast-forward the pull request's branch, so it opens a second
    /// pull request for the same work. The same conditions publish checks
    /// apply here (see [`Self::pull_request_tip`]); when any fails, the run
    /// starts from the checkout's HEAD exactly as before.
    ///
    /// # Errors
    /// Returns typed lifecycle, persistence, repository, ownership, or Git errors.
    pub fn prepare_run_workspace_with(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        repository_path: impl AsRef<Path>,
        gh: Option<&GhClient>,
    ) -> Result<RunWorkspace, WorkspaceError> {
        if store.load_workspace(run_id)?.is_some() {
            return Err(WorkspaceError::WorkspaceAlreadyExists(run_id));
        }
        let loaded = store.load_run(run_id)?;
        if loaded.run.status() != RunStatus::Created {
            return Err(invalid_run_status(&loaded.run, "workspace preparation"));
        }
        let repository = GitRepository::discover(repository_path)?;
        let mode = if loaded.run.workflow().requires_writable_workspace() {
            WorkspaceMode::Branch
        } else {
            WorkspaceMode::Detached
        };
        let input = store.load_run_input(run_id)?;
        let branch = (mode == WorkspaceMode::Branch)
            .then(|| branch_name::branch_name(run_id, input.as_ref().map(RunInput::task)));
        if let Some(branch) = branch.as_deref()
            && branch_exists(&self.git, &repository, branch)?
        {
            return Err(WorkspaceError::BranchConflict(branch.to_owned()));
        }
        let path = self.workspace_path(&repository, run_id)?;
        if path.exists() {
            return Err(WorkspaceError::WorkspacePathConflict(path));
        }
        let base_ref = format!("refs/senate/pull-request-base/{run_id}");
        let pull_request_base = match (
            gh,
            input.as_ref().and_then(|i| PullRequestRef::parse(i.task())),
        ) {
            // Any failure to resolve — including a fetch that errors outright —
            // is a reason to start from the checkout, never to refuse the run:
            // before a run could start from a pull request, nothing here
            // touched the network at all.
            (Some(gh), Some(reference)) => self
                .pull_request_tip(gh, repository.source_path(), &reference, &base_ref)
                .ok()
                .and_then(Result::ok)
                .map(|tip| tip.commit),
            _ => None,
        };
        let base_commit = pull_request_base
            .clone()
            .unwrap_or_else(|| repository.head_commit().to_owned());

        let intent_time = next_time(&loaded.run);
        let mut run = loaded.run;
        let intent = RunWorkspace::preparing(
            run_id,
            repository.source_path().to_path_buf(),
            repository.git_common_dir().to_path_buf(),
            base_commit,
            path,
            branch,
            mode,
            intent_time,
        )
        .and_then(|workspace| {
            let begin_event =
                run.transition(RunTransition::BeginPreparation, metadata(intent_time))?;
            let begin = store.begin_workspace_preparation(
                &workspace,
                &run,
                loaded.revision,
                &begin_event,
            )?;
            Ok((workspace, begin))
        });
        let (mut workspace, begin) = match intent {
            Ok(intent) => intent,
            Err(error) => {
                // No intent was persisted, so nothing will ever come back for
                // the commit the scratch ref holds. Once intent is persisted
                // the ref stays until the worktree exists, because a crash
                // before then is recovered by recreating the worktree from it.
                if pull_request_base.is_some() {
                    let _ = delete_ref(&self.git, repository.source_path(), &base_ref);
                }
                return Err(error);
            }
        };
        self.fault(FaultPoint::WorkspaceIntent)?;

        self.create_intended_worktree(&repository, &workspace)?;
        if pull_request_base.is_some() {
            // The worktree's HEAD holds the commit now; the scratch ref that
            // kept it reachable until then is only clutter. Best effort — a
            // leftover ref under The Senate's namespace harms nothing.
            let _ = delete_ref(&self.git, repository.source_path(), &base_ref);
        }
        self.fault(FaultPoint::WorktreeCreated)?;
        self.validate_workspace(&workspace, true)?;
        // Before the workspace is ready, so no stage ever sees a tree the
        // repository considers half-built. Synchronous like verification and
        // for the same reason: a cold build interrupted halfway would need
        // its own supervision, and this one has to finish before anything
        // downstream is worth starting.
        setup::run_for(workspace.worktree_path(), workspace.source_repo_path())?;

        let ready_time = next_time(&run);
        workspace.mark_ready(ready_time);
        let ready_event = run.transition(RunTransition::FinishPreparation, metadata(ready_time))?;
        store.finalize_workspace_preparation(
            &workspace,
            workspace.revision(),
            &run,
            begin.revision(),
            &ready_event,
        )?;
        store
            .load_workspace(run_id)?
            .ok_or(WorkspaceError::WorkspaceMissing(run_id))
    }

    /// Reconciles persisted intent against current Git/filesystem state.
    ///
    /// # Errors
    /// Returns persistence errors or unrecoverable Git command failures. Unsafe
    /// mismatches are persisted as `Broken` and returned as an outcome.
    pub fn reconcile(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<ReconciliationOutcome, WorkspaceError> {
        let workspace = store
            .load_workspace(run_id)?
            .ok_or(WorkspaceError::WorkspaceMissing(run_id))?;
        match workspace.status() {
            WorkspaceStatus::Preparing => self.reconcile_preparing(store, workspace),
            WorkspaceStatus::Ready => match self.observe(&workspace) {
                Ok(()) => Ok(ReconciliationOutcome::Unchanged(workspace)),
                Err(reason) => Self::break_workspace(store, workspace, reason),
            },
            WorkspaceStatus::Removing => self.finish_removal(store, workspace),
            WorkspaceStatus::Removed => Ok(ReconciliationOutcome::Unchanged(workspace)),
            WorkspaceStatus::Broken => self.reconcile_broken(store, workspace),
        }
    }

    /// Re-observes a workspace that persisted state calls usable, healing what
    /// is safely healable before reporting anything wrong.
    ///
    /// Returns the reason the workspace is unusable, in the words that reach
    /// the operator through `last_error`.
    fn observe(&self, workspace: &RunWorkspace) -> Result<(), String> {
        if let Err(error) = self.validate_source(workspace) {
            return Err(error.to_string());
        }
        if !workspace.worktree_path().exists() {
            return Err("ready worktree is missing".to_owned());
        }
        if let Err(error) = self.restore_detached_head(workspace) {
            return Err(error.to_string());
        }
        self.validate_workspace(workspace, false)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Returns a detached worktree to its base commit after an agent moved HEAD.
    ///
    /// An agent told to review a pull request will reach for `gh pr checkout`,
    /// and inside a worktree The Senate owns exclusively that is reasonable work,
    /// not a loss of ownership — which the worktree's path and Git common
    /// directory still prove, and which this does not touch. Treating the moved
    /// HEAD as a mismatch broke the run permanently on the next reconcile, with
    /// no route back that did not edit the store by hand.
    ///
    /// Uncommitted work is the one case this refuses to resolve alone.
    /// Re-detaching would discard changes no event ever recorded, so a dirty
    /// tree under a moved HEAD stays broken for an operator to judge.
    fn restore_detached_head(&self, workspace: &RunWorkspace) -> Result<(), WorkspaceError> {
        if workspace.mode() != WorkspaceMode::Detached {
            return Ok(());
        }
        let identity = inspect_worktree(&self.git, workspace.worktree_path())?;
        if identity.branch.is_none() && identity.head_commit == workspace.base_commit() {
            return Ok(());
        }
        if !tree_is_clean(&self.git, workspace.worktree_path())? {
            return Err(WorkspaceError::WorkspaceOwnershipMismatch {
                run_id: workspace.run_id(),
                reason: format!(
                    "detached worktree carries uncommitted work at {}, away from base {}",
                    identity.head_commit,
                    workspace.base_commit()
                ),
            });
        }
        detach_worktree(
            &self.git,
            workspace.worktree_path(),
            workspace.base_commit(),
        )?;
        Ok(())
    }

    /// Re-observes a broken workspace instead of treating the verdict as final.
    ///
    /// What breaks a workspace is usually a condition outside it — a source
    /// checkout that moved, a worktree not yet on disk, an agent that walked
    /// HEAD off its base — and once the condition is gone the run is
    /// recoverable. Broken is what was observed, not a property the workspace
    /// acquired, so it is observed again; a workspace that still fails stays
    /// broken and reports the current reason rather than the original one.
    /// A run whose preparation never finished is put back on the preparing
    /// path instead, because there the missing worktree is built rather than
    /// reported.
    fn reconcile_broken(
        &self,
        store: &mut SqliteStore,
        mut workspace: RunWorkspace,
    ) -> Result<ReconciliationOutcome, WorkspaceError> {
        // A run still `Preparing` broke before it was ever handed a worktree,
        // and observing is not what it needs: preparation is the only path
        // that builds a worktree and the only path that ends the run's
        // preparation, so observation could only report the same missing
        // worktree forever while the run sat in `Preparing` with no way out
        // that did not edit the store by hand. Preparing re-checks every
        // condition that breaks a workspace, so nothing unsafe is healed by
        // going back to it.
        if store.load_run(workspace.run_id())?.run.status() == RunStatus::Preparing {
            return self.reconcile_preparing(store, workspace);
        }
        if let Err(reason) = self.observe(&workspace) {
            if workspace.last_error() != Some(reason.as_str()) {
                Self::persist_broken(store, &mut workspace, reason)?;
            }
            return Ok(ReconciliationOutcome::Broken(workspace));
        }
        let prior_revision = workspace.revision();
        workspace.mark_ready(now());
        store.update_workspace(&workspace, prior_revision)?;
        Ok(ReconciliationOutcome::Ready(workspace))
    }

    /// Gives a review's workspace a branch, so a fix cycle can reach the
    /// operator's checkout.
    ///
    /// A review is prepared detached: it produces findings, not changes, and
    /// apply refuses anything but a branch The Senate owns. Sending that run back
    /// to fix what it found is the moment the run starts producing changes, so
    /// it is the moment the workspace earns a branch — created at the
    /// worktree's current HEAD, which is the tree the fix will edit.
    ///
    /// Idempotent by construction: a run already on a branch, including one on
    /// its second fix cycle, is returned unchanged.
    ///
    /// # Errors
    /// Returns ownership, branch-conflict, Git, or persistence errors. A
    /// workspace that is not Ready is refused without being modified.
    pub fn adopt_branch_for_fix(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<RunWorkspace, WorkspaceError> {
        let mut workspace = Self::ready_workspace(store, run_id)?;
        if workspace.mode() == WorkspaceMode::Branch {
            return Ok(workspace);
        }
        let repository = self.validate_source(&workspace)?;
        self.validate_workspace(&workspace, false)?;
        let input = store.load_run_input(run_id)?;
        let branch = branch_name::branch_name(run_id, input.as_ref().map(RunInput::task));
        if branch_exists(&self.git, &repository, &branch)? {
            return Err(WorkspaceError::BranchConflict(branch));
        }
        create_branch_in_worktree(&self.git, workspace.worktree_path(), &branch)?;
        let prior_revision = workspace.revision();
        workspace.adopt_branch(branch, now());
        store.adopt_workspace_branch(&workspace, prior_revision)?;
        store
            .load_workspace(run_id)?
            .ok_or(WorkspaceError::WorkspaceMissing(run_id))
    }

    /// Applies exact workspace delta to clean source checkout without staging or committing.
    ///
    /// # Errors
    /// Rejects non-completed/review runs, dirty sources, changed patch identity,
    /// failed preflight, duplicate apply, and ambiguous recovery state.
    pub fn apply(&self, store: &mut SqliteStore, run_id: RunId) -> Result<(), WorkspaceError> {
        let loaded = store.load_run(run_id)?;
        if loaded.run.status() == RunStatus::Applied {
            return Err(WorkspaceError::ApplyAlreadyPerformed(run_id));
        }
        ensure_verification_passed(&loaded.run)?;
        ensure_verification_follows_rebase(store, &loaded.run)?;

        if loaded.run.status() != RunStatus::Completed {
            return Err(invalid_run_status(&loaded.run, "apply"));
        }
        let workspace = Self::ready_workspace(store, run_id)?;
        if workspace.mode() != WorkspaceMode::Branch {
            return Err(WorkspaceError::ReviewWorkspaceNotApplicable);
        }
        self.validate_workspace(&workspace, false)?;
        let repository = self.validate_source(&workspace)?;
        let patch = generate_patch(
            &self.git,
            workspace.worktree_path(),
            workspace.base_commit(),
        )?;
        if patch.is_empty() {
            return Err(WorkspaceError::EmptyPatch);
        }
        let patch_hash = hash_bytes(&patch);

        if let Some(operation) = store.load_apply_operation(run_id)? {
            return self.reconcile_apply(
                store,
                loaded.run,
                loaded.revision,
                &repository,
                &patch,
                &patch_hash,
                operation,
            );
        }
        if !source_is_clean(&self.git, &repository)? {
            return Err(WorkspaceError::SourceCheckoutDirty(
                repository.source_path().to_path_buf(),
            ));
        }
        let operation =
            store.insert_apply_operation(run_id, &patch_hash, loaded.revision, now())?;
        if !check_patch(&self.git, &repository, &patch, false)? {
            let _failed = store.update_apply_operation(
                &operation,
                ApplyStatus::Failed,
                Some("git apply --check failed"),
                now(),
            )?;
            let (reason, rebasable) = self.patch_refusal_reason(&repository, &workspace);
            return Err(WorkspaceError::PatchCheckFailed { reason, rebasable });
        }
        if !source_is_clean(&self.git, &repository)? {
            store.update_apply_operation(
                &operation,
                ApplyStatus::Failed,
                Some("source changed after apply intent"),
                now(),
            )?;
            return Err(WorkspaceError::SourceCheckoutDirty(
                repository.source_path().to_path_buf(),
            ));
        }
        if let Err(error) = apply_patch(&self.git, &repository, &patch) {
            store.update_apply_operation(
                &operation,
                ApplyStatus::Failed,
                Some(&error.to_string()),
                now(),
            )?;
            return Err(error.into());
        }
        self.fault(FaultPoint::GitApplied)?;
        let operation =
            store.update_apply_operation(&operation, ApplyStatus::AppliedToSource, None, now())?;
        Self::finalize_apply(store, loaded.run, loaded.revision, &operation)
    }

    /// Moves a completed run's delta onto the source checkout's current
    /// `HEAD`, so a change written against a base the checkout has since left
    /// behind can still be applied.
    ///
    /// Apply transfers an exact patch and refuses anything less, which is the
    /// right answer for a checkout that changed underneath a run — The Senate
    /// will not guess how the two edits combine. This is the operator saying
    /// how: take the run's commits and replay them on what the checkout has
    /// now, and let Git refuse if the two really do disagree about the same
    /// lines. Nothing is merged silently; a conflict aborts and the run keeps
    /// the base it had.
    ///
    /// The delta is committed on the run's branch first, exactly as publish
    /// commits it, because commits are what a rebase moves. That commit
    /// survives a conflicting rebase — the branch keeps the work, only the
    /// base stays put.
    ///
    /// Verification is what this costs. The checks ran over the old base, so
    /// the event this records makes them stale and the apply gate refuses
    /// until a later cycle's verification passes on the new one.
    ///
    /// # Errors
    /// Rejects non-completed/review runs, workspaces whose base the checkout
    /// no longer contains, runs already based on `HEAD`, live apply intents,
    /// and conflicting rebases.
    pub fn rebase(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<RebaseReceipt, WorkspaceError> {
        let loaded = store.load_run(run_id)?;
        if loaded.run.status() != RunStatus::Completed {
            return Err(invalid_run_status(&loaded.run, "rebase"));
        }
        let mut workspace = Self::ready_workspace(store, run_id)?;
        if workspace.mode() != WorkspaceMode::Branch {
            return Err(WorkspaceError::ReviewWorkspaceNotApplicable);
        }
        self.validate_workspace(&workspace, false)?;
        let repository = self.validate_source(&workspace)?;
        // A `Prepared` or `AppliedToSource` intent means the patch written
        // against the old base may already be in the checkout, half-way or
        // whole. Moving the base under it would leave nothing able to tell
        // which. Only a `Failed` intent is spent, and the store drops it with
        // the same transaction that moves the base.
        if let Some(operation) = store.load_apply_operation(run_id)?
            && operation.status() != ApplyStatus::Failed
        {
            return Err(WorkspaceError::RebaseBlockedByApply(run_id));
        }
        let base = workspace.base_commit().to_owned();
        let head = repository.head_commit().to_owned();
        if head == base {
            return Err(WorkspaceError::RebaseAlreadyCurrent { run_id, base });
        }
        if !is_ancestor(&self.git, repository.source_path(), &base, &head)? {
            return Err(WorkspaceError::RebaseBaseNotAncestor { base, head });
        }
        let gained = count_commits_between(&self.git, repository.source_path(), &base, &head)?;
        let worktree = workspace.worktree_path();
        if !tree_is_clean(&self.git, worktree)? {
            let message = format!("The Senate run {run_id}");
            commit_all_in_worktree(&self.git, worktree, &message)?;
        }
        rebase_worktree_onto(&self.git, worktree, &base, &head).map_err(|error| {
            WorkspaceError::RebaseConflict {
                run_id,
                base: base.clone(),
                head: head.clone(),
                reason: error.to_string(),
            }
        })?;
        let prior_revision = workspace.revision();
        workspace.rebase_onto(head.clone(), now());
        let mut run = loaded.run;
        let at = next_time(&run);
        let event = run.workspace_rebased(metadata(at), base.clone(), head.clone())?;
        store.rebase_workspace_base(&workspace, prior_revision, &run, loaded.revision, &event)?;
        Ok(RebaseReceipt {
            from_base: base,
            to_base: head,
            commits_gained: gained,
        })
    }

    /// Publishes a completed run as a remote branch and pull request, never
    /// touching the operator's checkout.
    ///
    /// The counterpart to apply for a source the operator does not want
    /// written to: the run's delta is committed on the branch the run already
    /// owns, the branch is pushed to `origin`, and a pull request is opened
    /// through the GitHub CLI. Runs stay `Completed` — publish is transport,
    /// not disposition, so apply, fix, and discard all remain available, and
    /// publishing again after a fix cycle updates the same branch and pull
    /// request.
    ///
    /// When the run's task names a pull request it can reach and fast-forward
    /// — the shape of a review and the fix cycle after it — the commit lands
    /// on that pull request's own head branch instead, so fixing a pull
    /// request updates it rather than opening a second one for the same work.
    /// See [`Self::publish_target`] for what has to hold first.
    ///
    /// Push-first by design: pull-request failures (no `gh`, not
    /// authenticated) are reported inside the receipt, because by then the
    /// work is already safe on the remote.
    ///
    /// Verification does not gate this the way it gates apply. Apply writes
    /// the operator's own checkout, where a failed check means a tree nobody
    /// asked for; publish writes a branch and a pull request, which is exactly
    /// where unfinished work is meant to be graded — by CI, by a reviewer,
    /// and against a `[verify]` table that is often failing for reasons the
    /// change never caused (a worktree whose setup did not run, a repository
    /// red on its own trunk). Refusing there stranded the delta in a worktree
    /// with no way out but a manual push. The confirmation says verification
    /// failed, so publishing past it is a choice rather than an accident.
    ///
    /// # Errors
    /// Rejects non-completed runs, detached/review workspaces, workspaces with
    /// nothing to publish, and repositories without an `origin` remote.
    /// Returns Git errors from committing or pushing.
    pub fn publish(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        draft: Option<&PullRequestDraft>,
    ) -> Result<PublishReceipt, WorkspaceError> {
        self.publish_with(store, run_id, draft, &GhClient::default())
    }

    /// `draft` is the pull request the latest editing stage wrote for its
    /// change, when it wrote one; the task text stands in for whatever the
    /// draft lacks, so a run that predates the contract publishes as before.
    fn publish_with(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        draft: Option<&PullRequestDraft>,
        gh: &GhClient,
    ) -> Result<PublishReceipt, WorkspaceError> {
        let loaded = store.load_run(run_id)?;
        if loaded.run.status() != RunStatus::Completed {
            return Err(invalid_run_status(&loaded.run, "publish"));
        }
        let mut workspace = Self::ready_workspace(store, run_id)?;
        if workspace.mode() != WorkspaceMode::Branch {
            return Err(WorkspaceError::ReviewWorkspaceNotApplicable);
        }
        self.validate_workspace(&workspace, false)?;
        self.validate_source(&workspace)?;
        let branch = workspace
            .branch_name()
            .ok_or(WorkspaceError::MissingBranch(workspace.mode()))?
            .to_owned();
        // Owned, because replaying onto a pull request that moved writes the
        // new base back through `workspace` while the path is still needed.
        let worktree_path = workspace.worktree_path().to_path_buf();
        let worktree = worktree_path.as_path();
        // The same gate cleanup honors: a run stranded mid-apply-recovery has
        // an operation whose outcome is not yet known, and nothing else may
        // move until apply resolves it.
        if store
            .load_apply_operation(run_id)?
            .is_some_and(|operation| {
                matches!(
                    operation.status(),
                    ApplyStatus::Prepared | ApplyStatus::AppliedToSource
                )
            })
        {
            return Err(WorkspaceError::ApplyInProgress(run_id));
        }
        // Every pure refusal comes before the commit, so a refused publish
        // leaves the worktree exactly as it found it.
        if remote_url(&self.git, worktree, "origin")?.is_none() {
            return Err(WorkspaceError::NoRemote(
                workspace.source_repo_path().to_path_buf(),
            ));
        }

        let task = store
            .load_run_input(run_id)?
            .map(|input| input.task().to_owned());
        let title = draft.map_or_else(
            || publish_title(task.as_deref(), run_id),
            |draft| bounded_title(&draft.title),
        );
        let mut commit = if tree_is_clean(&self.git, worktree)? {
            inspect_worktree(&self.git, worktree)?.head_commit
        } else {
            let message = format!("{title}\n\nThe Senate run {run_id}");
            commit_all_in_worktree(&self.git, worktree, &message)?
        };
        if commit == workspace.base_commit() {
            return Err(WorkspaceError::NothingToPublish);
        }
        let (branch, pull_request, note) =
            match self.publish_target(gh, worktree, task.as_deref(), &commit)? {
                PublishTarget::PullRequestBranch {
                    branch: head_branch,
                    url,
                } => {
                    push_commit_to_branch(&self.git, worktree, "origin", &commit, &head_branch)?;
                    (head_branch, PullRequestStatus::AlreadyExists(url), None)
                }
                PublishTarget::RebaseOntoPullRequest(moved) => self.land_on_moved_pull_request(
                    store,
                    run_id,
                    &mut workspace,
                    &branch,
                    &mut commit,
                    moved,
                )?,
                PublishTarget::OwnBranch(note) => {
                    push_branch(&self.git, worktree, "origin", &branch)?;
                    let pull_request = open_pull_request(
                        gh,
                        worktree,
                        &branch,
                        &title,
                        draft,
                        task.as_deref(),
                        run_id,
                    );
                    (branch, pull_request, note)
                }
            };
        // Reloaded: a replay onto a moved pull request has already committed
        // its rebase against the run loaded above.
        let loaded = store.load_run(run_id)?;
        let mut run = loaded.run;
        let url = match &pull_request {
            PullRequestStatus::Created(url) | PullRequestStatus::AlreadyExists(url) => {
                Some(url.clone())
            }
            PullRequestStatus::Unavailable(_) => None,
        };
        let event = run.published(
            metadata(next_time(&run)),
            branch.clone(),
            commit.clone(),
            url,
        )?;
        store.commit_run_update(&run, loaded.revision, &[event])?;
        Ok(PublishReceipt {
            branch,
            commit,
            pull_request,
            note,
        })
    }

    /// Lands this publish on a pull request that moved: replays the run's
    /// delta on the new tip and pushes it there, or — when the two changes
    /// really do disagree — leaves the pull request alone and keeps the work
    /// on the run's own branch.
    ///
    /// The fallback opens no pull request of its own. Everywhere else a
    /// fallback does, because the run's work needs somewhere to be read; here
    /// the place to read it already exists and is named in the run's task, so
    /// a second one would be the duplicate this whole path exists to avoid.
    /// The note says where the work is and what to do about it instead.
    ///
    /// `commit` is rewritten when the replay succeeds, because the commit
    /// that reached the remote is then the replayed one.
    ///
    /// # Errors
    /// Returns persistence and Git errors, including a failed push.
    fn land_on_moved_pull_request(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        workspace: &mut RunWorkspace,
        own_branch: &str,
        commit: &mut String,
        moved: MovedPullRequest,
    ) -> Result<(String, PullRequestStatus, Option<String>), WorkspaceError> {
        let MovedPullRequest { branch, url, tip } = moved;
        let worktree = workspace.worktree_path().to_path_buf();
        match self.replay_onto_pull_request(store, run_id, workspace, &tip)? {
            Ok(replayed) => {
                push_commit_to_branch(&self.git, &worktree, "origin", &replayed, &branch)?;
                *commit = replayed;
                let note = format!(
                    "{url} had moved ahead of this run, so the change was replayed on {branch} before the push. Verification ran on the old base, so it has to run again before apply."
                );
                Ok((branch, PullRequestStatus::AlreadyExists(url), Some(note)))
            }
            Err(conflict) => {
                push_branch(&self.git, &worktree, "origin", own_branch)?;
                let reason =
                    format!("replaying this run's change on {branch} hit a conflict: {conflict}");
                let note = format!(
                    "{url} moved ahead of this run and the replay conflicted, so the work is on {own_branch} and no second pull request was opened for it. Resolve the conflict, then publish again."
                );
                Ok((
                    own_branch.to_owned(),
                    PullRequestStatus::Unavailable(reason),
                    Some(note),
                ))
            }
        }
    }

    /// Replays the run's delta on `tip`, the current head of the pull request
    /// the run was asked to fix, and records the base it now stands on.
    ///
    /// A review and the fix cycle after it are built on the pull request as
    /// it stood when the run started. The pull request is free to gain
    /// commits in the meantime, and the change still belongs on it, so the
    /// delta is replayed — `git rebase --onto <tip> <base>`, the same move
    /// [`Self::rebase`] makes onto a checkout that walked on. A conflict
    /// aborts, changes nothing, and comes back as the reason the publish
    /// falls back to the run's own branch.
    ///
    /// The new base is written through the same transaction [`Self::rebase`]
    /// uses, because the workspace's recorded base is what apply diffs
    /// against: leaving it on a commit the branch no longer descends from
    /// would make a later apply carry the pull request's own commits as if
    /// this run had written them. Verification pays for it the same way,
    /// going stale against the new base.
    ///
    /// # Errors
    /// Returns persistence and Git errors. A conflicting replay is `Ok(Err)`,
    /// not an error: the publish still has somewhere to go.
    fn replay_onto_pull_request(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        workspace: &mut RunWorkspace,
        tip: &str,
    ) -> Result<Result<String, String>, WorkspaceError> {
        let base = workspace.base_commit().to_owned();
        let worktree = workspace.worktree_path().to_path_buf();
        let replayed = match rebase_worktree_onto(&self.git, &worktree, &base, tip) {
            Ok(head) => head,
            Err(conflict) => return Ok(Err(conflict.to_string())),
        };
        let loaded = store.load_run(run_id)?;
        let prior_revision = workspace.revision();
        workspace.rebase_onto(tip.to_owned(), now());
        let mut run = loaded.run;
        let at = next_time(&run);
        let event = run.workspace_rebased(metadata(at), base, tip.to_owned())?;
        store.rebase_workspace_base(workspace, prior_revision, &run, loaded.revision, &event)?;
        Ok(Ok(replayed))
    }

    /// Decides whether this publish belongs on a pull request the run was
    /// asked to fix, rather than on a branch of its own.
    ///
    /// A run whose task names a pull request — the shape of every review, and
    /// of the fix cycle that follows one — is working on that pull request's
    /// change. Pushing its commits to a fresh branch opens a second pull
    /// request for the same work, which is never what the operator asked for.
    ///
    /// Before The Senate writes to a branch it does not own, the pull request
    /// has to resolve (see [`Self::pull_request_tip`]). A tip that is already
    /// an ancestor of the commit being published is pushed onto directly. A
    /// tip that is not — the pull request gained commits while the run worked
    /// — is replayed onto instead (see [`Self::replay_onto_pull_request`]),
    /// because the second pull request that a plain fallback would open is
    /// the one outcome the operator never asked for. A pull request that does
    /// not resolve at all is an explanation rather than an error, because
    /// publishing to the run's own branch remains a correct outcome there.
    /// Nothing here force-pushes and nothing rewrites a branch that moved:
    /// the replay moves the run's own commits, and the push that follows
    /// still fast-forwards.
    fn publish_target(
        &self,
        gh: &GhClient,
        worktree: &Path,
        task: Option<&str>,
        commit: &str,
    ) -> Result<PublishTarget, WorkspaceError> {
        let Some(reference) = task.and_then(PullRequestRef::parse) else {
            return Ok(PublishTarget::OwnBranch(None));
        };
        let tip = match self.pull_request_tip(gh, worktree, &reference, PUBLISH_TARGET_REF)? {
            Ok(tip) => tip,
            Err(reason) => return Ok(PublishTarget::OwnBranch(Some(reason))),
        };
        let url = reference.url();
        if !is_ancestor(&self.git, worktree, &tip.commit, commit)? {
            return Ok(PublishTarget::RebaseOntoPullRequest(MovedPullRequest {
                branch: tip.branch,
                url,
                tip: tip.commit,
            }));
        }
        Ok(PublishTarget::PullRequestBranch {
            branch: tip.branch,
            url,
        })
    }

    /// Resolves the pull request `reference` names to its head branch on
    /// `origin` and fetches that branch's tip onto `target_ref`.
    ///
    /// Three things have to hold, and each failure comes back as an
    /// explanation rather than an error, because building on the run's own
    /// base remains a correct outcome: the head branch must live in the
    /// repository `origin` points at, not in a fork; that pull request must be
    /// the open one `origin` serves for the branch, which rules out a task that
    /// merely mentions somebody else's; and the branch must still exist on the
    /// remote.
    fn pull_request_tip(
        &self,
        gh: &GhClient,
        cwd: &Path,
        reference: &PullRequestRef,
        target_ref: &str,
    ) -> Result<Result<PullRequestTip, String>, WorkspaceError> {
        let url = reference.url();
        let head = match gh.pull_request_head(cwd, reference) {
            Ok(head) => head,
            Err(unavailable) => {
                return Ok(Err(format!(
                    "{url} could not be read, so this run kept its own branch: {}",
                    unavailable.0
                )));
            }
        };
        if head.cross_repository {
            return Ok(Err(format!(
                "{url} reads from a fork, which The Senate cannot push to."
            )));
        }
        let open = match gh.existing_pull_request(cwd, &head.branch) {
            Ok(open) => open,
            Err(unavailable) => {
                return Ok(Err(format!(
                    "{url} could not be matched to this repository: {}",
                    unavailable.0
                )));
            }
        };
        if !open.is_some_and(|open| open.eq_ignore_ascii_case(&url)) {
            return Ok(Err(format!(
                "{url} is not an open pull request of this repository's origin."
            )));
        }
        let Some(commit) = fetch_branch(&self.git, cwd, "origin", &head.branch, target_ref)? else {
            return Ok(Err(format!(
                "{url} has no branch {} on origin any more.",
                head.branch
            )));
        };
        Ok(Ok(PullRequestTip {
            branch: head.branch,
            commit,
        }))
    }

    /// Builds a bounded read-only preview from same temporary-index delta used by apply.
    ///
    /// # Errors
    /// Rejects missing/non-ready workspaces, ownership failures, invalid limits, or Git failures.
    /// Branch and detached workspaces are both inspectable; only branch workspaces remain
    /// applicable. No apply intent or canonical state is changed.
    pub(crate) fn preview_patch(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        max_bytes: usize,
    ) -> Result<crate::git::PatchPreview, WorkspaceError> {
        if max_bytes == 0 {
            return Err(crate::git::GitError::InvalidOutput(
                "diff preview byte limit must be positive".to_owned(),
            )
            .into());
        }
        let workspace = Self::ready_workspace(store, run_id)?;
        self.validate_workspace(&workspace, false)?;
        Ok(generate_patch_preview(
            &self.git,
            workspace.worktree_path(),
            workspace.base_commit(),
            max_bytes,
        )?)
    }

    /// Records logical discard first, then removes owned workspace resources.
    ///
    /// # Errors
    /// Returns lifecycle, persistence, ownership, or Git cleanup errors.
    pub fn discard(&self, store: &mut SqliteStore, run_id: RunId) -> Result<(), WorkspaceError> {
        let loaded = store.load_run(run_id)?;
        if loaded.run.status() != RunStatus::Discarded {
            let mut run = loaded.run;
            let at = next_time(&run);
            let event = run.transition(RunTransition::Discard, metadata(at))?;
            store.commit_run_update(&run, loaded.revision, &[event])?;
        }
        if store.load_workspace(run_id)?.is_some() {
            self.cleanup(store, run_id)?;
        }
        Ok(())
    }

    /// Removes owned Git resources without changing logical run status.
    ///
    /// # Errors
    /// Only completed/applied/discarded runs are eligible. Ownership mismatches
    /// become a persisted broken workspace instead of deleting foreign data.
    pub fn cleanup(&self, store: &mut SqliteStore, run_id: RunId) -> Result<(), WorkspaceError> {
        self.remove_workspace(store, run_id, BranchDisposition::Delete)
    }

    /// Takes back the worktree of a run that no longer needs one, leaving the
    /// branch that carries its work.
    ///
    /// # Errors
    /// The same as [`Self::cleanup`]: ineligible run status, an apply under
    /// way, or an ownership mismatch that becomes a persisted broken workspace.
    pub fn release_worktree(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
    ) -> Result<(), WorkspaceError> {
        self.remove_workspace(store, run_id, BranchDisposition::Keep)
    }

    fn remove_workspace(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        branch: BranchDisposition,
    ) -> Result<(), WorkspaceError> {
        let loaded = store.load_run(run_id)?;
        if !matches!(
            loaded.run.status(),
            RunStatus::Completed | RunStatus::Applied | RunStatus::Discarded
        ) {
            return Err(invalid_run_status(&loaded.run, "workspace cleanup"));
        }
        if store
            .load_apply_operation(run_id)?
            .is_some_and(|operation| {
                matches!(
                    operation.status(),
                    ApplyStatus::Prepared | ApplyStatus::AppliedToSource
                )
            })
        {
            return Err(WorkspaceError::ApplyInProgress(run_id));
        }
        let mut workspace = store
            .load_workspace(run_id)?
            .ok_or(WorkspaceError::WorkspaceMissing(run_id))?;
        if workspace.status() == WorkspaceStatus::Removed {
            return Ok(());
        }
        if workspace.status() == WorkspaceStatus::Removing {
            return match self.finish_removal(store, workspace)? {
                ReconciliationOutcome::Removed(_) | ReconciliationOutcome::Unchanged(_) => Ok(()),
                ReconciliationOutcome::Broken(workspace) => Err(WorkspaceError::WorkspaceBroken {
                    run_id,
                    reason: workspace.last_error().unwrap_or("unknown error").to_owned(),
                }),
                ReconciliationOutcome::Ready(_) => Err(WorkspaceError::WorkspaceBroken {
                    run_id,
                    reason: "removal unexpectedly returned a ready workspace".to_owned(),
                }),
            };
        }
        let repository = match self.validate_source(&workspace) {
            Ok(repository) => repository,
            Err(error) => {
                Self::persist_broken(store, &mut workspace, error.to_string())?;
                return Err(error);
            }
        };
        let removal_head = if workspace.worktree_path().exists() {
            let identity = match self.validate_workspace(&workspace, false) {
                Ok(identity) => identity,
                Err(error) => {
                    Self::persist_broken(store, &mut workspace, error.to_string())?;
                    return Err(error);
                }
            };
            workspace.confirm_branch_ownership();
            identity.head_commit
        } else if workspace.branch_owned() {
            let branch = workspace
                .branch_name()
                .ok_or(WorkspaceError::MissingBranch(workspace.mode()))?;
            match branch_tip(&self.git, &repository, branch)? {
                None => workspace.base_commit().to_owned(),
                Some(tip) if tip == workspace.base_commit() => tip,
                Some(_) => {
                    let error = WorkspaceError::WorkspaceOwnershipMismatch {
                        run_id,
                        reason: "worktree is absent and advanced branch tip cannot be proven owned"
                            .to_owned(),
                    };
                    Self::persist_broken(store, &mut workspace, error.to_string())?;
                    return Err(error);
                }
            }
        } else {
            workspace.base_commit().to_owned()
        };
        let prior_revision = workspace.revision();
        workspace.mark_removing(removal_head, now());
        // Recorded as part of the removal intent rather than carried in the
        // call, because removal resumes from the store: a crash between here
        // and the deletion leaves the workspace row as the only thing that
        // still knows the branch was meant to survive.
        if branch == BranchDisposition::Keep {
            workspace.release_branch_ownership();
        }
        store.update_workspace(&workspace, prior_revision)?;
        self.fault(FaultPoint::RemovalIntent)?;
        let workspace = store
            .load_workspace(run_id)?
            .ok_or(WorkspaceError::WorkspaceMissing(run_id))?;
        match self.finish_removal(store, workspace)? {
            ReconciliationOutcome::Removed(_) | ReconciliationOutcome::Unchanged(_) => Ok(()),
            ReconciliationOutcome::Broken(workspace) => Err(WorkspaceError::WorkspaceBroken {
                run_id,
                reason: workspace.last_error().unwrap_or("unknown error").to_owned(),
            }),
            ReconciliationOutcome::Ready(_) => Err(WorkspaceError::WorkspaceBroken {
                run_id,
                reason: "removal did not reach terminal state".to_owned(),
            }),
        }
    }

    fn reconcile_preparing(
        &self,
        store: &mut SqliteStore,
        mut workspace: RunWorkspace,
    ) -> Result<ReconciliationOutcome, WorkspaceError> {
        let repository = match self.validate_source(&workspace) {
            Ok(repository) => repository,
            Err(error) => {
                return Self::break_workspace(store, workspace, error.to_string());
            }
        };
        if workspace.worktree_path().exists() {
            if let Err(error) = self.validate_workspace(&workspace, true) {
                return Self::break_workspace(store, workspace, error.to_string());
            }
        } else {
            if let Some(branch) = workspace.branch_name()
                && branch_exists(&self.git, &repository, branch)?
            {
                return Self::break_workspace(
                    store,
                    workspace,
                    "intended branch exists without intended worktree",
                );
            }
            if let Err(error) = self.create_intended_worktree(&repository, &workspace) {
                return Self::break_workspace(store, workspace, error.to_string());
            }
            if let Err(error) = self.validate_workspace(&workspace, true) {
                return Self::break_workspace(store, workspace, error.to_string());
            }
        }

        // A workspace is `Preparing` on this path precisely because the first
        // attempt did not finish, and setup failing is one of the ways it does
        // not finish. Marking ready now, because the checkout itself
        // validates, would hand over the half-built tree `[setup]` exists to
        // prevent, and the run would look like it had recovered. Setup
        // commands are meant to be idempotent, so running them over a worktree
        // that got part of the way is the same work.
        //
        // A failure here leaves the workspace `Preparing` rather than breaking
        // it: the usual cause is a `[setup]` command the user can correct, and
        // the next resume should be free to try it again.
        setup::run_for(workspace.worktree_path(), workspace.source_repo_path())?;

        let loaded = store.load_run(workspace.run_id())?;
        if loaded.run.status() != RunStatus::Preparing {
            return Self::break_workspace(
                store,
                workspace,
                format!(
                    "run status is {:?}, expected Preparing",
                    loaded.run.status()
                ),
            );
        }
        let mut run = loaded.run;
        let at = next_time(&run);
        let event = run.transition(RunTransition::FinishPreparation, metadata(at))?;
        let prior_revision = workspace.revision();
        workspace.mark_ready(at);
        store.finalize_workspace_preparation(
            &workspace,
            prior_revision,
            &run,
            loaded.revision,
            &event,
        )?;
        let workspace = store
            .load_workspace(workspace.run_id())?
            .ok_or(WorkspaceError::WorkspaceMissing(workspace.run_id()))?;
        Ok(ReconciliationOutcome::Ready(workspace))
    }

    fn finish_removal(
        &self,
        store: &mut SqliteStore,
        mut workspace: RunWorkspace,
    ) -> Result<ReconciliationOutcome, WorkspaceError> {
        let repository = match self.validate_source(&workspace) {
            Ok(repository) => repository,
            Err(error) => return Self::break_workspace(store, workspace, error.to_string()),
        };
        if workspace.worktree_path().exists() {
            if let Err(error) = self.validate_workspace(&workspace, false) {
                return Self::break_workspace(store, workspace, error.to_string());
            }
            if let Err(error) = remove_worktree(&self.git, &repository, workspace.worktree_path()) {
                return Self::break_workspace(store, workspace, error.to_string());
            }
        }
        if workspace.worktree_path().exists() {
            return Self::break_workspace(store, workspace, "worktree remains after removal");
        }
        if workspace.branch_owned() {
            let branch = workspace
                .branch_name()
                .ok_or(WorkspaceError::MissingBranch(workspace.mode()))?;
            let expected_tip =
                workspace
                    .removal_head()
                    .ok_or(WorkspaceError::InvalidStoredWorkspace(
                        "removal head is missing",
                    ))?;
            if let Err(error) = delete_owned_branch(&self.git, &repository, branch, expected_tip) {
                return Self::break_workspace(store, workspace, error.to_string());
            }
        }
        let prior_revision = workspace.revision();
        workspace.mark_removed(now());
        store.update_workspace(&workspace, prior_revision)?;
        let workspace = store
            .load_workspace(workspace.run_id())?
            .ok_or(WorkspaceError::WorkspaceMissing(workspace.run_id()))?;
        Ok(ReconciliationOutcome::Removed(workspace))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "recovery requires persisted run, workspace patch, repository, and operation identities"
    )]
    fn reconcile_apply(
        &self,
        store: &mut SqliteStore,
        run: Run,
        run_revision: RunRevision,
        repository: &GitRepository,
        patch: &[u8],
        patch_hash: &str,
        mut operation: RunApplyOperation,
    ) -> Result<(), WorkspaceError> {
        if operation.status() == ApplyStatus::Recorded {
            return Err(WorkspaceError::ApplyAlreadyPerformed(run.id()));
        }
        if operation.patch_hash() != patch_hash {
            return Err(WorkspaceError::PatchHashMismatch);
        }
        if operation.run_revision() != run_revision.value() {
            return Err(WorkspaceError::WorkspaceBroken {
                run_id: run.id(),
                reason: "run revision changed after apply intent".to_owned(),
            });
        }
        let forward = check_patch(&self.git, repository, patch, false)?;
        let reverse = check_patch(&self.git, repository, patch, true)?;
        match operation.status() {
            ApplyStatus::Prepared if forward && !reverse => {
                if !source_is_clean(&self.git, repository)? {
                    return Err(WorkspaceError::SourceCheckoutDirty(
                        repository.source_path().to_path_buf(),
                    ));
                }
                if let Err(error) = apply_patch(&self.git, repository, patch) {
                    store.update_apply_operation(
                        &operation,
                        ApplyStatus::Failed,
                        Some(&error.to_string()),
                        now(),
                    )?;
                    return Err(error.into());
                }
                self.fault(FaultPoint::GitApplied)?;
                operation = store.update_apply_operation(
                    &operation,
                    ApplyStatus::AppliedToSource,
                    None,
                    now(),
                )?;
            }
            ApplyStatus::Prepared | ApplyStatus::AppliedToSource if !forward && reverse => {
                if operation.status() == ApplyStatus::Prepared {
                    operation = store.update_apply_operation(
                        &operation,
                        ApplyStatus::AppliedToSource,
                        None,
                        now(),
                    )?;
                }
            }
            ApplyStatus::Failed if forward && !reverse => {
                if !source_is_clean(&self.git, repository)? {
                    return Err(WorkspaceError::SourceCheckoutDirty(
                        repository.source_path().to_path_buf(),
                    ));
                }
                operation =
                    store.update_apply_operation(&operation, ApplyStatus::Prepared, None, now())?;
                if let Err(error) = apply_patch(&self.git, repository, patch) {
                    store.update_apply_operation(
                        &operation,
                        ApplyStatus::Failed,
                        Some(&error.to_string()),
                        now(),
                    )?;
                    return Err(error.into());
                }
                self.fault(FaultPoint::GitApplied)?;
                operation = store.update_apply_operation(
                    &operation,
                    ApplyStatus::AppliedToSource,
                    None,
                    now(),
                )?;
            }
            _ => return Err(WorkspaceError::AmbiguousApplyState),
        }
        Self::finalize_apply(store, run, run_revision, &operation)
    }

    fn finalize_apply(
        store: &mut SqliteStore,
        mut run: Run,
        run_revision: RunRevision,
        operation: &RunApplyOperation,
    ) -> Result<(), WorkspaceError> {
        let at = next_time(&run);
        let event = run.transition(RunTransition::Apply, metadata(at))?;
        store.finalize_apply_operation(operation, &run, run_revision, &event, at)?;
        Ok(())
    }

    /// Why `git apply --check` refused, in the terms the operator can act on.
    ///
    /// Best effort by construction: this runs while an apply is already
    /// failing, and a Git call that fails here must not replace the real
    /// refusal with a worse one. Every probe that cannot answer is simply
    /// left out of the sentence. The flag says whether a rebase answers it:
    /// only a checkout that moved forward past the base can be met that way.
    fn patch_refusal_reason(
        &self,
        repository: &GitRepository,
        workspace: &RunWorkspace,
    ) -> (String, bool) {
        let base = workspace.base_commit();
        let head = repository.head_commit();
        if head == base {
            return (
                "the checkout is still on this run's base, so the change conflicts with what \
                 is already in it"
                    .to_owned(),
                false,
            );
        }
        let path = repository.source_path();
        match is_ancestor(&self.git, path, base, head) {
            Ok(true) => {
                let moved = count_commits_between(&self.git, path, base, head).map_or_else(
                    |_| "moved past".to_owned(),
                    |count| format!("moved {count} commit(s) past"),
                );
                (
                    format!(
                        "the checkout {moved} this run's base {}, and the two edits touch the \
                         same lines — `senate rebase {}` replays the change on the checkout's \
                         HEAD",
                        short(base),
                        workspace.run_id()
                    ),
                    true,
                )
            }
            Ok(false) => (
                format!(
                    "the checkout's HEAD {} does not contain this run's base {}",
                    short(head),
                    short(base)
                ),
                false,
            ),
            Err(_) => (
                "the checkout no longer matches the base this change was written against"
                    .to_owned(),
                false,
            ),
        }
    }

    fn ready_workspace(store: &SqliteStore, run_id: RunId) -> Result<RunWorkspace, WorkspaceError> {
        let workspace = store
            .load_workspace(run_id)?
            .ok_or(WorkspaceError::WorkspaceMissing(run_id))?;
        if workspace.status() != WorkspaceStatus::Ready {
            return Err(WorkspaceError::InvalidWorkspaceStatus {
                run_id,
                status: workspace.status(),
                expected: "Ready",
            });
        }
        Ok(workspace)
    }

    fn create_intended_worktree(
        &self,
        repository: &GitRepository,
        workspace: &RunWorkspace,
    ) -> Result<(), WorkspaceError> {
        let parent = workspace.worktree_path().parent().ok_or_else(|| {
            WorkspaceError::WorkspacePathConflict(workspace.worktree_path().to_path_buf())
        })?;
        std::fs::create_dir_all(parent)?;
        create_worktree(
            &self.git,
            repository,
            workspace.worktree_path(),
            workspace.base_commit(),
            workspace.branch_name(),
        )?;
        Ok(())
    }

    fn validate_source(&self, workspace: &RunWorkspace) -> Result<GitRepository, WorkspaceError> {
        let repository = GitRepository::discover(workspace.source_repo_path())?;
        if repository.source_path() != workspace.source_repo_path()
            || repository.git_common_dir() != workspace.git_common_dir()
        {
            return Err(WorkspaceError::WorkspaceOwnershipMismatch {
                run_id: workspace.run_id(),
                reason: "source repository identity changed".to_owned(),
            });
        }
        let expected_path = self.workspace_path(&repository, workspace.run_id())?;
        if expected_path != workspace.worktree_path() {
            return Err(WorkspaceError::WorkspaceOwnershipMismatch {
                run_id: workspace.run_id(),
                reason: "persisted worktree path is not deterministic for repository and run"
                    .to_owned(),
            });
        }
        Ok(repository)
    }

    fn validate_workspace(
        &self,
        workspace: &RunWorkspace,
        require_base_head: bool,
    ) -> Result<crate::git::WorktreeIdentity, WorkspaceError> {
        let identity = inspect_worktree(&self.git, workspace.worktree_path())?;
        if identity.path != workspace.worktree_path()
            || identity.git_common_dir != workspace.git_common_dir()
        {
            return Err(WorkspaceError::WorkspaceOwnershipMismatch {
                run_id: workspace.run_id(),
                reason: "path or Git common directory differs".to_owned(),
            });
        }
        let expected_branch = workspace.branch_name();
        if identity.branch.as_deref() != expected_branch {
            return Err(WorkspaceError::WorkspaceOwnershipMismatch {
                run_id: workspace.run_id(),
                reason: format!(
                    "branch differs: expected {expected_branch:?}, found {:?}",
                    identity.branch
                ),
            });
        }
        if require_base_head && identity.head_commit != workspace.base_commit() {
            return Err(WorkspaceError::WorkspaceOwnershipMismatch {
                run_id: workspace.run_id(),
                reason: "initial worktree HEAD differs from persisted base".to_owned(),
            });
        }
        Ok(identity)
    }

    fn workspace_path(
        &self,
        repository: &GitRepository,
        run_id: RunId,
    ) -> Result<PathBuf, WorkspaceError> {
        std::fs::create_dir_all(&self.root)?;
        let root = self.root.canonicalize()?;
        let name = repository
            .source_path()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository");
        let sanitized = sanitize_component(name);
        let common = repository.git_common_dir().to_str().ok_or_else(|| {
            crate::git::GitError::NonUtf8Path(repository.git_common_dir().to_path_buf())
        })?;
        let digest = hash_bytes(common.as_bytes());
        Ok(root
            .join(format!("{sanitized}-{}", &digest[..12]))
            .join(run_id.to_string()))
    }

    fn break_workspace(
        store: &mut SqliteStore,
        mut workspace: RunWorkspace,
        reason: impl Into<String>,
    ) -> Result<ReconciliationOutcome, WorkspaceError> {
        Self::persist_broken(store, &mut workspace, reason)?;
        let workspace = store
            .load_workspace(workspace.run_id())?
            .ok_or(WorkspaceError::WorkspaceMissing(workspace.run_id()))?;
        Ok(ReconciliationOutcome::Broken(workspace))
    }

    fn persist_broken(
        store: &mut SqliteStore,
        workspace: &mut RunWorkspace,
        reason: impl Into<String>,
    ) -> Result<(), WorkspaceError> {
        let prior_revision = workspace.revision();
        workspace.mark_broken(reason, now());
        store.update_workspace(workspace, prior_revision)?;
        Ok(())
    }

    #[cfg(test)]
    fn fault(&self, point: FaultPoint) -> Result<(), WorkspaceError> {
        if self.fault == Some(point) {
            return Err(WorkspaceError::InjectedCrash(point.name()));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[allow(
        clippy::unnecessary_wraps,
        reason = "production and test fault hooks intentionally share call contract"
    )]
    fn fault(&self, _point: FaultPoint) -> Result<(), WorkspaceError> {
        let _ = &self.root;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultPoint {
    WorkspaceIntent,
    WorktreeCreated,
    RemovalIntent,
    GitApplied,
}

#[cfg(test)]
impl FaultPoint {
    const fn name(self) -> &'static str {
        match self {
            Self::WorkspaceIntent => "after workspace intent",
            Self::WorktreeCreated => "after worktree creation",
            Self::RemovalIntent => "after removal intent",
            Self::GitApplied => "after Git apply",
        }
    }
}

/// One line naming the work when no editing stage drafted a title: the
/// task's first line, for the commit subject and pull-request title.
///
/// A task is written to an agent, not to a reviewer, so its first line is
/// often a link and little else — `Work on <issue url>` names nothing in a
/// list of pull requests. Links are dropped, and when what remains is too
/// thin to be a title the issue behind the link supplies one.
/// The pull request for a branch the run owns: the one already open for it,
/// or a new one. Never an error — by the time this runs the branch is on the
/// remote, so a `gh` that is missing or unauthenticated costs the convenience
/// and nothing else.
fn open_pull_request(
    gh: &GhClient,
    worktree: &Path,
    branch: &str,
    title: &str,
    draft: Option<&PullRequestDraft>,
    task: Option<&str>,
    run_id: RunId,
) -> PullRequestStatus {
    match gh.existing_pull_request(worktree, branch) {
        Ok(Some(url)) => PullRequestStatus::AlreadyExists(url),
        Ok(None) => {
            let body = draft
                .filter(|draft| !draft.body.is_empty())
                .map_or_else(|| publish_body(task, run_id), |draft| draft.body.clone());
            match gh.create_pull_request(worktree, branch, title, &body) {
                Ok(url) => PullRequestStatus::Created(url),
                Err(unavailable) => PullRequestStatus::Unavailable(unavailable.0),
            }
        }
        Err(unavailable) => PullRequestStatus::Unavailable(unavailable.0),
    }
}

fn publish_title(task: Option<&str>, run_id: RunId) -> String {
    let Some(first_line) = task
        .map(str::trim)
        .and_then(|task| task.lines().next())
        .map(str::trim)
        .filter(|line| !line.is_empty())
    else {
        return format!("The Senate run {run_id}");
    };
    let words = without_urls(first_line);
    if words.split_whitespace().count() >= 3 {
        return bounded_title(&words);
    }
    if let Some(headline) = issue_headline(first_line) {
        return bounded_title(&headline);
    }
    if words.is_empty() {
        format!("The Senate run {run_id}")
    } else {
        bounded_title(&words)
    }
}

/// The line without its URL tokens, with the punctuation a dropped link left
/// hanging trimmed off each end.
fn without_urls(line: &str) -> String {
    let kept: Vec<&str> = line
        .split_whitespace()
        .filter(|word| !word.contains("://"))
        .collect();
    kept.join(" ")
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '-' | ',' | '.' | ';'))
        .to_owned()
}

/// A title read out of the first issue link in the line: `DOTCOM-17972:
/// stepper transfer waits` from a Linear URL, `wp-calypso#114037` from a
/// GitHub one. `None` when the line links to no issue.
fn issue_headline(line: &str) -> Option<String> {
    for word in line.split_whitespace() {
        let word = word.trim_end_matches(['.', ',', ';', ')', '>']);
        if let Some(rest) = word.split_once("linear.app/").map(|(_, rest)| rest) {
            let mut parts = rest.split('/');
            let _org = parts.next();
            if parts.next() != Some("issue") {
                continue;
            }
            let Some(key) = parts.next().filter(|key| !key.is_empty()) else {
                continue;
            };
            let key = key.to_ascii_uppercase();
            return Some(match parts.next().filter(|slug| !slug.is_empty()) {
                Some(slug) => format!("{key}: {}", slug.replace('-', " ")),
                None => key,
            });
        }
        if let Some(rest) = word.split_once("github.com/").map(|(_, rest)| rest) {
            let parts: Vec<&str> = rest.split('/').collect();
            if let [_owner, repo, kind, number, ..] = parts.as_slice()
                && matches!(*kind, "issues" | "pull")
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
            {
                return Some(format!("{repo}#{number}"));
            }
        }
    }
    None
}

/// A title cut to the length Git and GitHub show in full.
fn bounded_title(line: &str) -> String {
    const LIMIT: usize = 72;
    if line.chars().count() <= LIMIT {
        line.to_owned()
    } else {
        let mut title: String = line.chars().take(LIMIT - 1).collect();
        title.push('…');
        title
    }
}

/// The description when no editing stage drafted one: the task verbatim, led
/// by a `Fixes` line when the task links an issue so the pull request still
/// closes it, and closed by the run footer.
fn publish_body(task: Option<&str>, run_id: RunId) -> String {
    let footer = format!("Opened by The Senate from run {run_id}.");
    let Some(task) = task.map(str::trim).filter(|task| !task.is_empty()) else {
        return footer;
    };
    match issue_url(task) {
        Some(url) => format!("Fixes {url}\n\n{task}\n\n---\n{footer}"),
        None => format!("{task}\n\n---\n{footer}"),
    }
}

/// One commit hash cut to the length a person reads, for messages only.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The first issue link in the task, as written.
fn issue_url(task: &str) -> Option<&str> {
    task.split_whitespace()
        .map(|word| word.trim_end_matches(['.', ',', ';', ')', '>']))
        .find(|word| {
            word.contains("://")
                && (word.contains("linear.app/") || word.contains("github.com/"))
                && issue_headline(word).is_some()
        })
}

/// Refuses a run whose latest verification ran before its workspace was
/// rebased.
///
/// A passed verification is a statement about a tree: these checks ran over
/// that base plus this delta. A rebase replaces the base, so the statement
/// stops covering what apply would move — the delta may be identical and
/// still break against commits the checkout gained. The two facts are both
/// in the event log with a total order over them, so the gate is a
/// comparison rather than a stored flag: the run is blocked when its most
/// recent verify stage completed earlier in the log than the most recent
/// rebase, or never completed at all.
///
/// Clearing it means running the checks again on the new base, which is what
/// a fix or continue cycle's own verify stage does.
fn ensure_verification_follows_rebase(
    store: &SqliteStore,
    run: &Run,
) -> Result<(), WorkspaceError> {
    let Some(latest) = run
        .stages()
        .iter()
        .rev()
        .find(|stage| stage.kind() == StageKind::Verify)
    else {
        return Ok(());
    };
    let events = store.load_events(run.id())?;
    let Some((rebased_at, base)) =
        events
            .iter()
            .rev()
            .find_map(|sequenced| match sequenced.event.kind() {
                DomainEventKind::WorkspaceRebased { to_base, .. } => {
                    Some((sequenced.sequence, to_base.clone()))
                }
                _ => None,
            })
    else {
        return Ok(());
    };
    let verified_at = events
        .iter()
        .rev()
        .find(|sequenced| {
            sequenced.event.stage_id() == Some(latest.id())
                && matches!(sequenced.event.kind(), DomainEventKind::StageCompleted)
        })
        .map(|sequenced| sequenced.sequence);
    if verified_at.is_none_or(|verified_at| verified_at < rebased_at) {
        return Err(WorkspaceError::VerificationPrecedesRebase {
            stage_id: latest.id().clone(),
            base,
        });
    }
    Ok(())
}

/// Refuses to move a run's changes anywhere unless its latest verification
/// passed.
///
/// The rule: take the run's most recent verify stage — the last one in
/// definition order, since every fix or continue cycle appends its own —
/// and refuse if it failed, or if the run is `Completed` and it is not
/// `Completed`. Older verify stages do not count: a fix cycle whose
/// `verify_n` passed has answered the failure that came before it, and a
/// verification that went red on the first attempt is exactly what a fix
/// cycle exists to turn green.
///
/// Asked before the run-status check, so a failure is refused by name rather
/// than by whatever status it caused. The decision only optionally depends
/// on verification, so a failed check completes the run, reaches the lead
/// as evidence, and leaves fix and continue available; this gate is what
/// keeps such a run from being applied or published in the meantime.
/// Apply's gate. Publish deliberately does not use it — see [`WorkspaceManager::publish`].
fn ensure_verification_passed(run: &Run) -> Result<(), WorkspaceError> {
    let Some(latest) = run
        .stages()
        .iter()
        .rev()
        .find(|stage| stage.kind() == StageKind::Verify)
    else {
        return Ok(());
    };
    let blocked = latest.status() == StageStatus::Failed
        || (run.status() == RunStatus::Completed && latest.status() != StageStatus::Completed);
    if blocked {
        return Err(WorkspaceError::VerificationNotPassed {
            stage_id: latest.id().clone(),
            status: format!("{:?}", latest.status()).to_lowercase(),
        });
    }
    Ok(())
}

fn invalid_run_status(run: &Run, operation: &'static str) -> WorkspaceError {
    WorkspaceError::InvalidRunStatus {
        run_id: run.id(),
        status: run.status(),
        operation,
    }
}

fn metadata(at: DateTime<Utc>) -> EventMetadata {
    EventMetadata::new(EventId::new(), at)
}

fn next_time(run: &Run) -> DateTime<Utc> {
    now().max(*run.updated_at())
}

fn hash_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(64);
    for byte in digest {
        hash.push(char::from(HEX[usize::from(byte >> 4)]));
        hash.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    hash
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

fn sanitize_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim_matches(['-', '.']);
    let sanitized = sanitized.chars().take(64).collect::<String>();
    if sanitized.is_empty() {
        "repository".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::process::Command;

    use chrono::Duration;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, Role, StageDefinition, StageId, StageKind, StageTransition,
        WorkflowDefinition, WorkflowKind,
    };
    use crate::store::ResolvedConfigSnapshot;

    struct Fixture {
        temp: TempDir,
        source: PathBuf,
        root: PathBuf,
        store: SqliteStore,
        run_id: RunId,
    }

    impl Fixture {
        fn new(kind: WorkflowKind) -> Self {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source repo with spaces");
            init_repository(&source);
            let root = temp.path().join("managed worktrees");
            let mut store = SqliteStore::open_in_memory().unwrap();
            let run_id = create_run(&mut store, kind, 1);
            Self {
                temp,
                source,
                root,
                store,
                run_id,
            }
        }

        /// A run carrying task text, which is where the pull request a review
        /// was pointed at is written down.
        fn with_task(kind: WorkflowKind, task: &str) -> Self {
            let mut fixture = Self::new(kind);
            fixture.run_id = create_run_with_task(&mut fixture.store, kind, 2, task);
            fixture
        }

        fn manager(&self) -> WorkspaceManager {
            WorkspaceManager::new(&self.root)
        }

        fn prepare(&mut self) -> RunWorkspace {
            self.manager()
                .prepare_run_workspace(&mut self.store, self.run_id, &self.source)
                .unwrap()
        }

        fn complete(&mut self) {
            complete_run(&mut self.store, self.run_id);
        }

        /// A run whose workflow actually carries a verify stage, which the
        /// single-stage fixtures do not — the gates that read verification
        /// have nothing to read without one.
        fn with_verification() -> Self {
            let mut fixture = Self::new(WorkflowKind::Standard);
            fixture.run_id = create_verified_run(&mut fixture.store, 3);
            fixture
        }

        /// Attempts preparation and hands back the error, for the cases
        /// where refusing to prepare is the behaviour under test.
        fn prepare_err(&mut self) -> WorkspaceError {
            self.manager()
                .prepare_run_workspace(&mut self.store, self.run_id, &self.source)
                .expect_err("preparation should have failed")
        }
    }

    /// Everything the operator can see of their own checkout: every file in
    /// the working tree with its bytes, the branch they are standing on, and
    /// the commit it points at. `.git` is walked past deliberately — a run
    /// legitimately writes worktree metadata and a branch ref in there, and
    /// neither is something the operator is looking at.
    fn source_snapshot(source: &Path) -> (Vec<(PathBuf, Vec<u8>)>, String, String) {
        fn walk(directory: &Path, base: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in fs::read_dir(directory).unwrap().flatten() {
                let path = entry.path();
                if path.file_name() == Some(OsStr::new(".git")) {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, base, out);
                } else {
                    out.push((
                        path.strip_prefix(base).unwrap().to_path_buf(),
                        fs::read(&path).unwrap(),
                    ));
                }
            }
        }
        let mut files = Vec::new();
        walk(source, source, &mut files);
        files.sort();
        (
            files,
            git_text(source, ["rev-parse", "HEAD"]),
            git_text(source, ["rev-parse", "--abbrev-ref", "HEAD"]),
        )
    }

    /// The invariant that entitles the word "isolated": a run owns its
    /// worktree and touches nothing the operator is working in. Asserted
    /// across the whole lifecycle rather than at creation, because the
    /// hazard is a long-running run beside someone editing the same
    /// checkout — the operator is left mid-edit here on purpose, with both
    /// an untracked file and a modified tracked one.
    #[test]
    fn setup_commands_run_before_the_workspace_is_handed_over() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        // Untracked in the source checkout, so it cannot reach the worktree
        // through git — which is the point: the build output a worktree
        // lacks is configured from the checkout the user owns.
        fs::write(
            fixture.source.join(".senate.toml"),
            "[setup]\ncommands = [\"touch built-artifact\"]\n",
        )
        .unwrap();

        let workspace = fixture.prepare();

        assert!(
            workspace.worktree_path().join("built-artifact").is_file(),
            "setup must have run in the worktree before it was ready"
        );
        assert_eq!(workspace.status(), WorkspaceStatus::Ready);
    }

    #[test]
    fn a_failing_setup_command_refuses_the_workspace_instead_of_handing_over_a_half_built_tree() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fs::write(
            fixture.source.join(".senate.toml"),
            "[setup]\ncommands = [\"ls /no-such-senate-setup-path\"]\n",
        )
        .unwrap();

        let error = fixture.prepare_err();

        assert!(
            matches!(&error, WorkspaceError::SetupFailed { command, .. }
                if command == "ls /no-such-senate-setup-path"),
            "{error}"
        );
        // The run never reaches Ready, so no stage is dispatched into a tree
        // the repository itself says is not usable.
        assert_ne!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .map(|workspace| workspace.status()),
            Some(WorkspaceStatus::Ready)
        );
    }

    /// Resuming a run whose setup failed is the operator saying "try again",
    /// not "carry on without it". The worktree left behind validates as a
    /// checkout, so reconciliation would otherwise mark ready the very tree
    /// the failure was about.
    #[test]
    fn resuming_after_a_failed_setup_runs_setup_again_instead_of_handing_the_tree_over() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fs::write(
            fixture.source.join(".senate.toml"),
            "[setup]\ncommands = [\"ls /no-such-senate-setup-path\"]\n",
        )
        .unwrap();
        fixture.prepare_err();

        let error = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .expect_err("reconciliation should have failed with the setup command");

        assert!(
            matches!(&error, WorkspaceError::SetupFailed { command, .. }
                if command == "ls /no-such-senate-setup-path"),
            "{error}"
        );
        // Still Preparing rather than Broken: the operator can fix the
        // command and resume, and rather than Ready: nothing is dispatched
        // into the half-built tree.
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .map(|workspace| workspace.status()),
            Some(WorkspaceStatus::Preparing)
        );
    }

    #[test]
    fn a_corrected_setup_command_prepares_the_existing_worktree_on_the_next_resume() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let config = fixture.source.join(".senate.toml");
        fs::write(
            &config,
            "[setup]\ncommands = [\"ls /no-such-senate-setup-path\"]\n",
        )
        .unwrap();
        fixture.prepare_err();
        fs::write(&config, "[setup]\ncommands = [\"touch built-artifact\"]\n").unwrap();

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();

        let ReconciliationOutcome::Ready(workspace) = outcome else {
            panic!("expected the corrected setup to make the workspace ready: {outcome:?}");
        };
        assert!(
            workspace.worktree_path().join("built-artifact").is_file(),
            "setup must have run in the worktree before it was ready"
        );
        assert_eq!(workspace.status(), WorkspaceStatus::Ready);
    }

    #[test]
    fn a_run_leaves_the_operators_checkout_exactly_as_it_found_it() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fs::write(fixture.source.join("local-only.txt"), "mine\n").unwrap();
        fs::write(fixture.source.join("README.md"), "mine too\n").unwrap();
        let before = source_snapshot(&fixture.source);

        let workspace = fixture.prepare();
        fs::write(
            workspace.worktree_path().join("README.md"),
            "changed by the run\n",
        )
        .unwrap();
        fs::write(workspace.worktree_path().join("added.txt"), "new\n").unwrap();
        git(workspace.worktree_path(), ["add", "."]);
        git(workspace.worktree_path(), ["commit", "-m", "run work"]);
        fixture.complete();
        assert_eq!(
            source_snapshot(&fixture.source),
            before,
            "a run changed the checkout its operator is working in"
        );

        fixture
            .manager()
            .discard(&mut fixture.store, fixture.run_id)
            .unwrap();
        fixture
            .manager()
            .cleanup(&mut fixture.store, fixture.run_id)
            .unwrap();
        assert_eq!(
            source_snapshot(&fixture.source),
            before,
            "cleaning up after a run changed the operator's checkout"
        );
    }

    /// And the one exception, stated so the invariant above is not merely a
    /// test that nothing anywhere ever writes to the source. Apply is the
    /// single path that does, it is invoked deliberately, and it refuses a
    /// checkout that is not clean.
    #[test]
    fn apply_is_the_only_thing_that_writes_to_the_operators_checkout() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let before = source_snapshot(&fixture.source);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(
            workspace.worktree_path().join("README.md"),
            "changed by the run\n",
        )
        .unwrap();

        // Not while the operator has work in progress. Two guards enforce
        // this — one before the apply intent is recorded and one immediately
        // before the write, so a checkout that goes dirty in between is
        // caught too. This asserts the behaviour, not either guard: removing
        // the first leaves the second, and the run still refuses.
        fs::write(fixture.source.join("mid-edit.txt"), "mine\n").unwrap();
        assert!(matches!(
            fixture.manager().apply(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::SourceCheckoutDirty(_))
        ));
        fs::remove_file(fixture.source.join("mid-edit.txt")).unwrap();

        fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .unwrap();
        let after = source_snapshot(&fixture.source);
        assert_ne!(
            after, before,
            "apply is supposed to be the one thing that changes the source"
        );
        assert_eq!(after.1, before.1, "apply does not move the operator's HEAD");
        assert_eq!(
            after.2, before.2,
            "apply does not move the operator's branch"
        );
    }

    #[test]
    fn a_run_with_a_task_owns_a_branch_named_after_its_issue() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        create_run_with_task(
            &mut fixture.store,
            WorkflowKind::Standard,
            2,
            "Fix https://linear.app/a8c/issue/DOTCOM-17972/stepper-transfer-waits",
        );
        fixture.run_id = RunId::from_u128(2);

        let workspace = fixture.prepare();
        let id = fixture.run_id.to_string().to_ascii_lowercase();
        let expected = format!("senate/dotcom-17972-{}", &id[id.len() - 6..]);
        assert_eq!(workspace.branch_name(), Some(expected.as_str()));
        let repository = GitRepository::discover(&fixture.source).unwrap();
        assert!(branch_exists(&fixture.manager().git, &repository, &expected).unwrap());
    }

    #[test]
    fn branch_workspace_is_isolated_and_source_may_be_dirty_at_creation() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fs::write(fixture.source.join("local-only.txt"), "dirty source\n").unwrap();

        let workspace = fixture.prepare();
        let discovered = GitRepository::discover(&fixture.source).unwrap();

        assert_eq!(workspace.mode(), WorkspaceMode::Branch);
        assert_eq!(workspace.status(), WorkspaceStatus::Ready);
        assert!(!workspace.worktree_path().starts_with(&fixture.source));
        assert_eq!(
            workspace.branch_name(),
            Some(format!("senate/run-{}", fixture.run_id).as_str())
        );
        assert!(fixture.source.join("local-only.txt").exists());
        assert_eq!(workspace.source_repo_path(), discovered.source_path());
        assert_eq!(workspace.git_common_dir(), discovered.git_common_dir());
        assert_eq!(workspace.base_commit(), discovered.head_commit());
        assert!(!workspace.worktree_path().join("local-only.txt").exists());
        fs::write(workspace.worktree_path().join("README.md"), "worktree\n").unwrap();
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "base\n"
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Ready
        );
    }

    #[test]
    fn review_workspace_is_detached_and_discard_removes_it() {
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let workspace = fixture.prepare();
        assert_eq!(workspace.mode(), WorkspaceMode::Detached);
        assert_eq!(workspace.branch_name(), None);

        fixture
            .manager()
            .discard(&mut fixture.store, fixture.run_id)
            .unwrap();
        fixture
            .manager()
            .discard(&mut fixture.store, fixture.run_id)
            .unwrap();
        fixture
            .manager()
            .cleanup(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(!workspace.worktree_path().exists());
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            WorkspaceStatus::Removed
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Discarded
        );
    }

    #[test]
    fn multiple_runs_get_distinct_worktrees_and_branches() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let second = create_run(&mut fixture.store, WorkflowKind::Standard, 2);
        let manager = fixture.manager();

        let first_workspace = manager
            .prepare_run_workspace(&mut fixture.store, fixture.run_id, &fixture.source)
            .unwrap();
        let second_workspace = manager
            .prepare_run_workspace(&mut fixture.store, second, &fixture.source)
            .unwrap();

        assert_ne!(
            first_workspace.worktree_path(),
            second_workspace.worktree_path()
        );
        assert_ne!(
            first_workspace.branch_name(),
            second_workspace.branch_name()
        );
        assert!(first_workspace.worktree_path().exists());
        assert!(second_workspace.worktree_path().exists());
    }

    #[test]
    fn apply_transfers_tracked_untracked_deleted_and_binary_without_staging_or_commit() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fs::write(fixture.source.join("delete me.txt"), "remove\n").unwrap();
        git(&fixture.source, ["add", "."]);
        git(&fixture.source, ["commit", "-m", "add deletion fixture"]);
        let source_head = git_text(&fixture.source, ["rev-parse", "HEAD"]);
        let workspace = fixture.prepare();
        fixture.complete();

        fs::write(workspace.worktree_path().join("README.md"), "changed\n").unwrap();
        git(workspace.worktree_path(), ["add", "README.md"]);
        git(
            workspace.worktree_path(),
            ["commit", "-m", "commit run change"],
        );
        fs::write(
            workspace.worktree_path().join("new file ü.txt"),
            "untracked\n",
        )
        .unwrap();
        fs::write(
            workspace.worktree_path().join("tab\tnewline\nü.txt"),
            "odd name\n",
        )
        .unwrap();
        fs::remove_file(workspace.worktree_path().join("delete me.txt")).unwrap();
        fs::write(
            workspace.worktree_path().join("binary.bin"),
            [0_u8, 1, 2, 0, 255, 128],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "base\n"
        );
        assert!(git_text(&fixture.source, ["status", "--porcelain"]).is_empty());

        fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("new file ü.txt")).unwrap(),
            "untracked\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("tab\tnewline\nü.txt")).unwrap(),
            "odd name\n"
        );
        assert!(!fixture.source.join("delete me.txt").exists());
        assert_eq!(
            fs::read(fixture.source.join("binary.bin")).unwrap(),
            [0_u8, 1, 2, 0, 255, 128]
        );
        assert_eq!(
            git_text(&fixture.source, ["rev-parse", "HEAD"]),
            source_head
        );
        assert!(git_status(&fixture.source, ["diff", "--cached", "--quiet"]));
        assert!(git_status(
            workspace.worktree_path(),
            ["diff", "--cached", "--quiet"]
        ));
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Applied
        );
        assert_eq!(
            fixture
                .store
                .load_apply_operation(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            ApplyStatus::Recorded
        );
        assert!(workspace.worktree_path().exists());
        assert!(matches!(
            fixture.manager().apply(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::ApplyAlreadyPerformed(_))
        ));
    }

    #[test]
    fn dirty_source_rejects_apply_without_mutation() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "run change\n").unwrap();
        fs::write(fixture.source.join("local.txt"), "mine\n").unwrap();

        let result = fixture.manager().apply(&mut fixture.store, fixture.run_id);

        assert!(matches!(
            result,
            Err(WorkspaceError::SourceCheckoutDirty(_))
        ));
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "base\n"
        );
        assert!(
            fixture
                .store
                .load_apply_operation(fixture.run_id)
                .unwrap()
                .is_none()
        );
    }

    /// The shape this exists for: the operator's checkout moved while the run
    /// was working, close enough to the run's own edit that the patch no
    /// longer applies, but not on the same lines. Apply is right to refuse —
    /// and rebase is how the operator answers it without redoing the work.
    #[test]
    fn a_rebase_replays_the_change_on_a_checkout_that_moved_and_apply_then_lands_it() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fs::write(fixture.source.join("lines.txt"), "1\n2\n3\n4\n5\n6\n7\n").unwrap();
        git(&fixture.source, ["add", "lines.txt"]);
        git(&fixture.source, ["commit", "-m", "lines"]);
        let workspace = fixture.prepare();
        fixture.complete();
        let base = workspace.base_commit().to_owned();
        fs::write(
            workspace.worktree_path().join("lines.txt"),
            "run\n2\n3\n4\n5\n6\n7\n",
        )
        .unwrap();
        // Four lines below the run's edit: inside the patch's context, so
        // `git apply` refuses, but not a line both sides changed.
        fs::write(
            fixture.source.join("lines.txt"),
            "1\n2\n3\nsource\n5\n6\n7\n",
        )
        .unwrap();
        git(&fixture.source, ["add", "lines.txt"]);
        git(&fixture.source, ["commit", "-m", "advance source"]);
        let head = git_text(&fixture.source, ["rev-parse", "HEAD"]);

        let refusal = fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .expect_err("the moved checkout must refuse the patch");
        assert!(matches!(refusal, WorkspaceError::PatchCheckFailed { .. }));

        let receipt = fixture
            .manager()
            .rebase(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert_eq!(receipt.from_base, base);
        assert_eq!(receipt.to_base, head);
        assert_eq!(receipt.commits_gained, 1);
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .base_commit(),
            head
        );
        // The spent intent is gone with the base it was written against;
        // leaving it would answer the next apply with a hash mismatch.
        assert!(
            fixture
                .store
                .load_apply_operation(fixture.run_id)
                .unwrap()
                .is_none()
        );

        fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert_eq!(
            fs::read_to_string(fixture.source.join("lines.txt")).unwrap(),
            "run\n2\n3\nsource\n5\n6\n7\n",
            "both edits must survive the move"
        );
    }

    /// Nothing is merged on the operator's behalf. When the two edits really
    /// do disagree, the rebase aborts and the run keeps the base it had, so
    /// the next apply refuses for the same honest reason as the first.
    #[test]
    fn a_conflicting_rebase_aborts_and_leaves_the_workspace_on_its_own_base() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        let base = workspace.base_commit().to_owned();
        fs::write(workspace.worktree_path().join("README.md"), "run change\n").unwrap();
        fs::write(fixture.source.join("README.md"), "source advanced\n").unwrap();
        git(&fixture.source, ["add", "README.md"]);
        git(&fixture.source, ["commit", "-m", "advance source"]);

        let error = fixture
            .manager()
            .rebase(&mut fixture.store, fixture.run_id)
            .expect_err("edits on the same line must not be merged silently");

        assert!(matches!(error, WorkspaceError::RebaseConflict { .. }));
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .base_commit(),
            base
        );
        assert_eq!(
            fs::read_to_string(workspace.worktree_path().join("README.md")).unwrap(),
            "run change\n",
            "the run's work must survive the aborted rebase"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "source advanced\n"
        );
    }

    #[test]
    fn a_run_already_based_on_head_is_refused_by_name_rather_than_rebased_onto_itself() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "run change\n").unwrap();

        let error = fixture
            .manager()
            .rebase(&mut fixture.store, fixture.run_id)
            .expect_err("there is no newer HEAD to move onto");

        assert!(matches!(error, WorkspaceError::RebaseAlreadyCurrent { .. }));
    }

    /// A checkout that was rewound or moved to unrelated history does not
    /// contain the run's base, so there is no "since the base" to replay.
    #[test]
    fn a_checkout_that_no_longer_contains_the_base_cannot_be_rebased_onto() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fs::write(fixture.source.join("second.txt"), "second\n").unwrap();
        git(&fixture.source, ["add", "second.txt"]);
        git(&fixture.source, ["commit", "-m", "second"]);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "run change\n").unwrap();
        git(&fixture.source, ["reset", "--hard", "HEAD~1"]);

        let error = fixture
            .manager()
            .rebase(&mut fixture.store, fixture.run_id)
            .expect_err("a rewound checkout has no path from the run's base");

        assert!(matches!(
            error,
            WorkspaceError::RebaseBaseNotAncestor { .. }
        ));
    }

    /// The price of the move: checks that passed over the old base say
    /// nothing about the new one, so apply waits for a fresh verification
    /// rather than trusting a stale green.
    #[test]
    fn apply_refuses_a_verification_that_ran_before_the_rebase() {
        let mut fixture = Fixture::with_verification();
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("run.txt"), "run change\n").unwrap();
        fs::write(fixture.source.join("source.txt"), "source\n").unwrap();
        git(&fixture.source, ["add", "source.txt"]);
        git(&fixture.source, ["commit", "-m", "advance source"]);

        fixture
            .manager()
            .rebase(&mut fixture.store, fixture.run_id)
            .unwrap();
        let error = fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .expect_err("verification predates the move");

        assert!(matches!(
            error,
            WorkspaceError::VerificationPrecedesRebase { .. }
        ));
        assert!(
            !fixture.source.join("run.txt").exists(),
            "nothing may reach the checkout while the gate is closed"
        );
    }

    #[test]
    fn preflight_conflict_records_failure_and_does_not_partially_apply() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "run change\n").unwrap();
        fs::write(fixture.source.join("README.md"), "source advanced\n").unwrap();
        git(&fixture.source, ["add", "README.md"]);
        git(&fixture.source, ["commit", "-m", "advance source"]);

        let result = fixture.manager().apply(&mut fixture.store, fixture.run_id);

        let Err(WorkspaceError::PatchCheckFailed {
            reason,
            rebasable: true,
        }) = result
        else {
            panic!("a source checkout that moved past the base must refuse the patch");
        };
        assert!(
            reason.contains("moved 1 commit(s) past") && reason.contains("senate rebase"),
            "the refusal must say what moved and what moves the run to meet it: {reason}"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "source advanced\n"
        );
        assert_eq!(
            fixture
                .store
                .load_apply_operation(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            ApplyStatus::Failed
        );
    }

    #[test]
    fn creation_crash_windows_reconcile_without_duplicates() {
        for fault in [FaultPoint::WorkspaceIntent, FaultPoint::WorktreeCreated] {
            let mut fixture = Fixture::new(WorkflowKind::Standard);
            let crashing = WorkspaceManager::with_fault(&fixture.root, fault);
            assert!(matches!(
                crashing.prepare_run_workspace(&mut fixture.store, fixture.run_id, &fixture.source),
                Err(WorkspaceError::InjectedCrash(_))
            ));
            assert_eq!(
                fixture
                    .store
                    .load_workspace(fixture.run_id)
                    .unwrap()
                    .unwrap()
                    .status(),
                WorkspaceStatus::Preparing
            );

            let outcome = fixture
                .manager()
                .reconcile(&mut fixture.store, fixture.run_id)
                .unwrap();
            assert!(matches!(outcome, ReconciliationOutcome::Ready(_)));
            assert!(matches!(
                fixture
                    .manager()
                    .reconcile(&mut fixture.store, fixture.run_id)
                    .unwrap(),
                ReconciliationOutcome::Unchanged(_)
            ));
        }
    }

    #[test]
    fn discard_after_creation_crash_removes_proven_branch() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::WorktreeCreated);
        assert!(
            crashing
                .prepare_run_workspace(&mut fixture.store, fixture.run_id, &fixture.source)
                .is_err()
        );
        let workspace = fixture
            .store
            .load_workspace(fixture.run_id)
            .unwrap()
            .unwrap();
        let reference = format!("refs/heads/{}", workspace.branch_name().unwrap());

        fixture
            .manager()
            .discard(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(!workspace.worktree_path().exists());
        assert!(!git_status(
            &fixture.source,
            ["rev-parse", "--verify", "--quiet", &reference]
        ));
    }

    #[test]
    fn apply_crash_after_git_effect_finalizes_once() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "recovered\n").unwrap();
        fs::write(
            workspace.worktree_path().join("recovered-new.txt"),
            "created once\n",
        )
        .unwrap();
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::GitApplied);

        assert!(matches!(
            crashing.apply(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::InjectedCrash(_))
        ));
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "recovered\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("recovered-new.txt")).unwrap(),
            "created once\n"
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Completed
        );
        assert_eq!(
            fixture
                .store
                .load_apply_operation(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            ApplyStatus::Prepared
        );
        assert!(matches!(
            fixture
                .manager()
                .discard(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::Store(
                crate::store::StoreError::RunFrozenForApply(_)
            ))
        ));
        assert!(matches!(
            fixture
                .manager()
                .cleanup(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::ApplyInProgress(_))
        ));

        fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .unwrap();
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "recovered\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("recovered-new.txt")).unwrap(),
            "created once\n"
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Applied
        );
    }

    #[test]
    fn removal_crash_reconciles_and_keeps_logical_completion() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::RemovalIntent);

        assert!(matches!(
            crashing.cleanup(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::InjectedCrash(_))
        ));
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            WorkspaceStatus::Removing
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Completed
        );

        assert!(matches!(
            fixture
                .manager()
                .reconcile(&mut fixture.store, fixture.run_id)
                .unwrap(),
            ReconciliationOutcome::Removed(_)
        ));
        assert!(!workspace.worktree_path().exists());
    }

    #[test]
    fn removal_reconciles_when_git_effect_already_happened() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::RemovalIntent);
        assert!(
            crashing
                .cleanup(&mut fixture.store, fixture.run_id)
                .is_err()
        );
        git_os(
            &fixture.source,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                workspace.worktree_path().as_os_str(),
            ],
        );

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(matches!(outcome, ReconciliationOutcome::Removed(_)));
        assert!(!workspace.worktree_path().exists());
        let reference = format!("refs/heads/{}", workspace.branch_name().unwrap());
        assert!(!git_status(
            &fixture.source,
            ["rev-parse", "--verify", "--quiet", &reference]
        ));
    }

    #[test]
    fn workspace_persists_across_store_reopen() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("persistent source");
        init_repository(&source);
        let root = temp.path().join("persistent worktrees");
        let database = temp.path().join("state").join("senate.db");
        let run_id;
        let expected;
        {
            let mut store = SqliteStore::open(&database).unwrap();
            run_id = create_run(&mut store, WorkflowKind::Standard, 77);
            expected = WorkspaceManager::new(&root)
                .prepare_run_workspace(&mut store, run_id, &source)
                .unwrap();
        }

        let mut reopened = SqliteStore::open(&database).unwrap();
        let restored = reopened.load_workspace(run_id).unwrap().unwrap();

        assert_eq!(restored, expected);
        assert!(matches!(
            WorkspaceManager::new(&root)
                .reconcile(&mut reopened, run_id)
                .unwrap(),
            ReconciliationOutcome::Unchanged(_)
        ));
    }

    #[test]
    fn review_workspace_rejects_apply_even_when_logically_completed() {
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "review edit\n").unwrap();

        let result = fixture.manager().apply(&mut fixture.store, fixture.run_id);

        assert!(matches!(
            result,
            Err(WorkspaceError::ReviewWorkspaceNotApplicable)
        ));
        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "base\n"
        );
    }

    /// A review is detached because it is not meant to produce changes.
    /// Sending it back to fix what it found is the moment that stops being
    /// true, so it is the moment the workspace earns a branch — and, with it,
    /// a route back into the operator's checkout that apply will accept.
    #[test]
    fn a_review_workspace_earns_a_branch_when_asked_to_fix_what_it_found() {
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let workspace = fixture.prepare();
        assert_eq!(workspace.mode(), WorkspaceMode::Detached);
        fixture.complete();

        let adopted = fixture
            .manager()
            .adopt_branch_for_fix(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert_eq!(adopted.mode(), WorkspaceMode::Branch);
        assert!(adopted.branch_owned());
        let branch = adopted.branch_name().unwrap().to_owned();
        assert_eq!(
            git_text(
                workspace.worktree_path(),
                ["rev-parse", "--abbrev-ref", "HEAD"]
            ),
            branch,
            "the worktree stands on the branch the store now claims"
        );
        assert!(
            matches!(
                fixture
                    .manager()
                    .reconcile(&mut fixture.store, fixture.run_id)
                    .unwrap(),
                ReconciliationOutcome::Unchanged(_)
            ),
            "and reconcile recognises the workspace it just became"
        );

        let again = fixture
            .manager()
            .adopt_branch_for_fix(&mut fixture.store, fixture.run_id)
            .unwrap();
        assert_eq!(
            again.branch_name(),
            Some(branch.as_str()),
            "a second fix cycle adopts nothing new"
        );
    }

    /// The point of the branch: what the fix writes can reach the checkout.
    /// Before adoption this same run is refused, which
    /// `review_workspace_rejects_apply_even_when_logically_completed` pins.
    #[test]
    fn a_fixed_review_can_finally_transfer_what_it_changed() {
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let workspace = fixture.prepare();
        fixture.complete();
        fixture
            .manager()
            .adopt_branch_for_fix(&mut fixture.store, fixture.run_id)
            .unwrap();
        fs::write(workspace.worktree_path().join("README.md"), "fixed\n").unwrap();

        fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "fixed\n"
        );
    }

    #[test]
    fn deterministic_paths_reject_collisions_and_existing_branches() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let manager = fixture.manager();
        let repository = GitRepository::discover(&fixture.source).unwrap();
        let first = manager.workspace_path(&repository, fixture.run_id).unwrap();
        let second = manager.workspace_path(&repository, fixture.run_id).unwrap();
        assert_eq!(first, second);
        assert!(first.is_absolute());
        assert!(first.to_string_lossy().contains("source-repo-with-spaces-"));
        fs::create_dir_all(&first).unwrap();
        assert!(matches!(
            manager.prepare_run_workspace(&mut fixture.store, fixture.run_id, &fixture.source),
            Err(WorkspaceError::WorkspacePathConflict(_))
        ));

        let mut branch_fixture = Fixture::new(WorkflowKind::Standard);
        let branch = format!("senate/run-{}", branch_fixture.run_id);
        git(&branch_fixture.source, ["branch", &branch]);
        assert!(matches!(
            branch_fixture.manager().prepare_run_workspace(
                &mut branch_fixture.store,
                branch_fixture.run_id,
                &branch_fixture.source,
            ),
            Err(WorkspaceError::BranchConflict(found)) if found == branch
        ));
        assert!(
            branch_fixture
                .store
                .load_workspace(branch_fixture.run_id)
                .unwrap()
                .is_none()
        );
    }

    /// An agent told to review a pull request reaches for `gh pr checkout`.
    /// That is reasonable work inside a worktree The Senate owns exclusively,
    /// and it used to end the run: the next reconcile read the branch as an
    /// ownership mismatch and condemned a workspace nothing else could revive.
    #[test]
    fn a_review_worktree_that_walked_off_its_base_is_returned_to_it() {
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let workspace = fixture.prepare();
        let base = workspace.base_commit().to_owned();
        git(&fixture.source, ["branch", "pull-request"]);
        git(workspace.worktree_path(), ["checkout", "pull-request"]);

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(matches!(outcome, ReconciliationOutcome::Unchanged(_)));
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            WorkspaceStatus::Ready
        );
        assert_eq!(
            git_text(workspace.worktree_path(), ["rev-parse", "HEAD"]),
            base,
            "the worktree is back on the commit the run was prepared from"
        );
        assert_eq!(
            git_text(
                workspace.worktree_path(),
                ["rev-parse", "--abbrev-ref", "HEAD"]
            ),
            "HEAD",
            "and detached again, the way a review workspace is owned"
        );
    }

    /// Healing stops where evidence begins. Re-detaching a dirty tree would
    /// destroy work no event ever recorded, so that stays an operator's call.
    #[test]
    fn uncommitted_work_under_a_moved_head_is_never_silently_discarded() {
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let workspace = fixture.prepare();
        git(&fixture.source, ["branch", "pull-request"]);
        git(workspace.worktree_path(), ["checkout", "pull-request"]);
        let stray = workspace.worktree_path().join("NOTES.md");
        fs::write(&stray, "work nobody recorded\n").unwrap();

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(matches!(outcome, ReconciliationOutcome::Broken(_)));
        assert_eq!(
            fs::read_to_string(&stray).unwrap(),
            "work nobody recorded\n",
            "the run is refused, and the work survives the refusal"
        );
    }

    /// Broken records what was observed, not something the workspace became.
    /// Once the condition is gone the run is recoverable, and recovering it
    /// must not require editing the store by hand.
    #[test]
    fn a_broken_workspace_is_observed_again_rather_than_condemned() {
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let workspace = fixture.prepare();
        git(&fixture.source, ["branch", "pull-request"]);
        git(workspace.worktree_path(), ["checkout", "pull-request"]);
        let stray = workspace.worktree_path().join("NOTES.md");
        fs::write(&stray, "work nobody recorded\n").unwrap();
        assert!(matches!(
            fixture
                .manager()
                .reconcile(&mut fixture.store, fixture.run_id)
                .unwrap(),
            ReconciliationOutcome::Broken(_)
        ));

        fs::remove_file(&stray).unwrap();

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(matches!(outcome, ReconciliationOutcome::Ready(_)));
        let healed = fixture
            .store
            .load_workspace(fixture.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(healed.status(), WorkspaceStatus::Ready);
        assert!(
            healed.last_error().is_none(),
            "a recovered workspace stops reporting the failure it recovered from"
        );
    }

    /// The one thing observation cannot heal is a worktree that was never
    /// built, and breaking during preparation is exactly when that happens:
    /// the run stayed `Preparing` while every later reconcile reported a
    /// "ready" worktree that had never been ready.
    #[test]
    fn a_workspace_broken_before_its_worktree_existed_is_built_and_handed_over() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::WorkspaceIntent);
        assert!(
            crashing
                .prepare_run_workspace(&mut fixture.store, fixture.run_id, &fixture.source)
                .is_err()
        );
        let intended = fixture
            .store
            .load_workspace(fixture.run_id)
            .unwrap()
            .unwrap();
        // Something foreign at the intended path breaks the workspace while
        // the run is still `Preparing` and the worktree still does not exist.
        init_repository(intended.worktree_path());
        assert!(matches!(
            fixture
                .manager()
                .reconcile(&mut fixture.store, fixture.run_id)
                .unwrap(),
            ReconciliationOutcome::Broken(_)
        ));

        fs::remove_dir_all(intended.worktree_path()).unwrap();

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(matches!(outcome, ReconciliationOutcome::Ready(_)));
        let healed = fixture
            .store
            .load_workspace(fixture.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(healed.status(), WorkspaceStatus::Ready);
        assert!(
            healed.worktree_path().join("README.md").exists(),
            "the worktree the run never got is built, not reported missing"
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Ready,
            "and the run finally leaves preparation"
        );
    }

    #[test]
    fn relocated_source_is_marked_broken_not_guessed() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        let relocated = fixture.source.with_file_name("relocated source");
        fs::rename(&fixture.source, &relocated).unwrap();

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert!(matches!(outcome, ReconciliationOutcome::Broken(_)));
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            WorkspaceStatus::Broken
        );
        assert!(workspace.worktree_path().exists());
    }

    #[test]
    fn foreign_path_and_missing_ready_workspace_become_broken_without_deletion() {
        let mut foreign_fixture = Fixture::new(WorkflowKind::Standard);
        let crashing =
            WorkspaceManager::with_fault(&foreign_fixture.root, FaultPoint::WorkspaceIntent);
        assert!(
            crashing
                .prepare_run_workspace(
                    &mut foreign_fixture.store,
                    foreign_fixture.run_id,
                    &foreign_fixture.source,
                )
                .is_err()
        );
        let intended = foreign_fixture
            .store
            .load_workspace(foreign_fixture.run_id)
            .unwrap()
            .unwrap();
        init_repository(intended.worktree_path());
        let outcome = foreign_fixture
            .manager()
            .reconcile(&mut foreign_fixture.store, foreign_fixture.run_id)
            .unwrap();
        assert!(matches!(outcome, ReconciliationOutcome::Broken(_)));
        assert!(intended.worktree_path().join("README.md").exists());

        let mut missing_fixture = Fixture::new(WorkflowKind::Standard);
        let ready = missing_fixture.prepare();
        git_os(
            &missing_fixture.source,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                ready.worktree_path().as_os_str(),
            ],
        );
        let outcome = missing_fixture
            .manager()
            .reconcile(&mut missing_fixture.store, missing_fixture.run_id)
            .unwrap();
        assert!(matches!(outcome, ReconciliationOutcome::Broken(_)));
    }

    #[test]
    fn moved_owned_branch_is_never_deleted_during_recovery() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        let branch = workspace.branch_name().unwrap().to_owned();
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::RemovalIntent);
        assert!(
            crashing
                .cleanup(&mut fixture.store, fixture.run_id)
                .is_err()
        );

        fs::write(
            workspace.worktree_path().join("README.md"),
            "new branch tip\n",
        )
        .unwrap();
        git(workspace.worktree_path(), ["add", "README.md"]);
        git(
            workspace.worktree_path(),
            ["commit", "-m", "move owned branch"],
        );
        let moved_tip = git_text(workspace.worktree_path(), ["rev-parse", "HEAD"]);

        let outcome = fixture
            .manager()
            .reconcile(&mut fixture.store, fixture.run_id)
            .unwrap();
        assert!(matches!(outcome, ReconciliationOutcome::Broken(_)));
        assert_eq!(
            git_text(
                &fixture.source,
                ["rev-parse", &format!("refs/heads/{branch}")]
            ),
            moved_tip
        );
    }

    #[test]
    fn advanced_branch_without_worktree_is_not_assumed_owned() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "advanced\n").unwrap();
        git(workspace.worktree_path(), ["add", "README.md"]);
        git(
            workspace.worktree_path(),
            ["commit", "-m", "advance branch"],
        );
        let branch = workspace.branch_name().unwrap().to_owned();
        let tip = git_text(workspace.worktree_path(), ["rev-parse", "HEAD"]);
        git_os(
            &fixture.source,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                workspace.worktree_path().as_os_str(),
            ],
        );

        assert!(matches!(
            fixture
                .manager()
                .cleanup(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::WorkspaceOwnershipMismatch { .. })
        ));
        assert_eq!(
            git_text(
                &fixture.source,
                ["rev-parse", &format!("refs/heads/{branch}")]
            ),
            tip
        );
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            WorkspaceStatus::Broken
        );
    }

    #[test]
    fn workspace_revision_rejects_stale_writer() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        let mut first = workspace.clone();
        let mut stale = workspace;
        first.mark_broken("first writer", now());
        fixture
            .store
            .update_workspace(&first, first.revision())
            .unwrap();
        stale.mark_broken("stale writer", now());

        let error = fixture
            .store
            .update_workspace(&stale, stale.revision())
            .unwrap_err();

        assert!(matches!(
            error,
            crate::store::StoreError::WorkspaceConcurrentModification { .. }
        ));
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .last_error(),
            Some("first writer")
        );
    }

    #[test]
    fn corrupt_workspace_record_never_becomes_valid_infrastructure_state() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        fixture.prepare();
        fixture
            .store
            .connection
            .execute(
                "UPDATE run_workspaces SET base_commit = ?1 WHERE run_id = ?2",
                rusqlite::params!["z".repeat(40), fixture.run_id.to_string()],
            )
            .unwrap();

        assert!(matches!(
            fixture.store.load_workspace(fixture.run_id),
            Err(crate::store::StoreError::InvalidWorkspaceRecord(_))
        ));
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Ready
        );
    }

    #[test]
    fn ready_workspace_and_run_event_roll_back_together() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::WorkspaceIntent);
        assert!(
            crashing
                .prepare_run_workspace(&mut fixture.store, fixture.run_id, &fixture.source)
                .is_err()
        );
        let mut workspace = fixture
            .store
            .load_workspace(fixture.run_id)
            .unwrap()
            .unwrap();
        let loaded = fixture.store.load_run(fixture.run_id).unwrap();
        let duplicate_id = fixture.store.load_events(fixture.run_id).unwrap()[0]
            .event
            .id();
        let mut run = loaded.run;
        let at = *run.updated_at() + Duration::milliseconds(1);
        let event = run
            .transition(
                RunTransition::FinishPreparation,
                EventMetadata::new(duplicate_id, at),
            )
            .unwrap();
        workspace.mark_ready(at);

        assert!(
            fixture
                .store
                .finalize_workspace_preparation(
                    &workspace,
                    workspace.revision(),
                    &run,
                    loaded.revision,
                    &event,
                )
                .is_err()
        );
        assert_eq!(
            fixture
                .store
                .load_workspace(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            WorkspaceStatus::Preparing
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Preparing
        );
    }

    #[test]
    fn applied_operation_and_run_event_roll_back_together() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(
            workspace.worktree_path().join("README.md"),
            "applied effect\n",
        )
        .unwrap();
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::GitApplied);
        assert!(crashing.apply(&mut fixture.store, fixture.run_id).is_err());
        let prepared = fixture
            .store
            .load_apply_operation(fixture.run_id)
            .unwrap()
            .unwrap();
        let operation = fixture
            .store
            .update_apply_operation(&prepared, ApplyStatus::AppliedToSource, None, now())
            .unwrap();
        let loaded = fixture.store.load_run(fixture.run_id).unwrap();
        let duplicate_id = fixture.store.load_events(fixture.run_id).unwrap()[0]
            .event
            .id();
        let mut run = loaded.run;
        let at = *run.updated_at() + Duration::milliseconds(1);
        let event = run
            .transition(RunTransition::Apply, EventMetadata::new(duplicate_id, at))
            .unwrap();

        assert!(
            fixture
                .store
                .finalize_apply_operation(&operation, &run, loaded.revision, &event, at,)
                .is_err()
        );
        assert_eq!(
            fixture
                .store
                .load_apply_operation(fixture.run_id)
                .unwrap()
                .unwrap()
                .status(),
            ApplyStatus::AppliedToSource
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Completed
        );
    }

    fn create_run(store: &mut SqliteStore, kind: WorkflowKind, id: u128) -> RunId {
        create_run_with_task_option(store, kind, id, None)
    }

    fn create_run_with_task(
        store: &mut SqliteStore,
        kind: WorkflowKind,
        id: u128,
        task: &str,
    ) -> RunId {
        create_run_with_task_option(store, kind, id, Some(task))
    }

    fn create_run_with_task_option(
        store: &mut SqliteStore,
        kind: WorkflowKind,
        id: u128,
        task: Option<&str>,
    ) -> RunId {
        let run_id = RunId::from_u128(id);
        let (stage_id, stage_kind, role) = if kind == WorkflowKind::Review {
            (
                StageId::new("review").unwrap(),
                StageKind::Review,
                Role::Reviewer,
            )
        } else {
            (
                StageId::new("implementation").unwrap(),
                StageKind::Implementation,
                Role::Implementer,
            )
        };
        let workflow = WorkflowDefinition::new(
            kind,
            vec![StageDefinition::new(stage_id, stage_kind, role, vec![])],
        )
        .unwrap();
        let created_at = now();
        let config_id = ConfigSnapshotId::new(format!("config-{id}")).unwrap();
        let run = Run::new(run_id, workflow, config_id.clone(), created_at);
        let config = ResolvedConfigSnapshot::new(config_id, 1, json!({}), created_at).unwrap();
        let event = run.created_event(metadata(created_at));
        match task {
            Some(task) => {
                let input = RunInput::new(run_id, task, created_at).unwrap();
                store
                    .create_run_with_input(&run, &input, &config, &[event])
                    .unwrap();
            }
            None => {
                store.create_run(&run, &config, &[event]).unwrap();
            }
        }
        run_id
    }

    /// An implementation stage followed by the verify stage that checks it,
    /// which is the smallest workflow the verification gates can see.
    fn create_verified_run(store: &mut SqliteStore, id: u128) -> RunId {
        let run_id = RunId::from_u128(id);
        let implementation = StageId::new("implementation").unwrap();
        let workflow = WorkflowDefinition::new(
            WorkflowKind::Standard,
            vec![
                StageDefinition::new(
                    implementation.clone(),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![],
                ),
                StageDefinition::new(
                    StageId::new("verify").unwrap(),
                    StageKind::Verify,
                    Role::Verifier,
                    vec![crate::domain::Dependency::required(implementation)],
                ),
            ],
        )
        .unwrap();
        let created_at = now();
        let config_id = ConfigSnapshotId::new(format!("config-{id}")).unwrap();
        let run = Run::new(run_id, workflow, config_id.clone(), created_at);
        let config = ResolvedConfigSnapshot::new(config_id, 1, json!({}), created_at).unwrap();
        let event = run.created_event(metadata(created_at));
        store.create_run(&run, &config, &[event]).unwrap();
        run_id
    }

    fn complete_run(store: &mut SqliteStore, run_id: RunId) {
        let loaded = store.load_run(run_id).unwrap();
        let mut run = loaded.run;
        let mut revision = loaded.revision;
        let at = *run.updated_at() + Duration::milliseconds(1);
        let event = run.transition(RunTransition::Start, metadata(at)).unwrap();
        revision = store
            .commit_run_update(&run, revision, &[event])
            .unwrap()
            .revision();
        let stages = run
            .stages()
            .iter()
            .map(|stage| stage.id().clone())
            .collect::<Vec<_>>();
        for stage in stages {
            for transition in [
                StageTransition::MarkReady,
                StageTransition::Start,
                StageTransition::Complete,
            ] {
                let at = *run.updated_at() + Duration::milliseconds(1);
                let event = run
                    .transition_stage(&stage, transition, metadata(at))
                    .unwrap();
                revision = store
                    .commit_run_update(&run, revision, &[event])
                    .unwrap()
                    .revision();
            }
        }
        let at = *run.updated_at() + Duration::milliseconds(1);
        let event = run
            .transition(RunTransition::Complete, metadata(at))
            .unwrap();
        store.commit_run_update(&run, revision, &[event]).unwrap();
    }

    /// A bare repository wired up as the source's `origin`, so push has
    /// somewhere real to go without any network.
    fn add_origin(fixture: &Fixture) -> PathBuf {
        let parent = fixture.source.parent().unwrap().to_path_buf();
        git(&parent, ["init", "--bare", "-b", "main", "origin.git"]);
        let origin = parent.join("origin.git");
        git(
            &fixture.source,
            ["remote", "add", "origin", origin.to_str().unwrap()],
        );
        origin
    }

    /// A `gh` stand-in: `pr list --head <branch>` prints whatever
    /// `list-<branch>` beside the script holds and falls back to
    /// `list-output`, `pr view` prints `view-output`, and `pr create` prints a
    /// fixed URL. A missing file is an unanswered question, which is what a
    /// `gh` that cannot answer looks like. No stub ever reaches a network.
    fn stub_gh(directory: &Path) -> GhClient {
        use std::os::unix::fs::PermissionsExt;
        let script = directory.join("gh");
        fs::write(
            &script,
            "#!/bin/sh\n\
             dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
             case \"$1 $2\" in\n\
             \"pr list\") head=$(printf '%s' \"$4\" | tr / _)\n\
             cat \"$dir/list-$head\" 2>/dev/null || cat \"$dir/list-output\" 2>/dev/null; exit 0 ;;\n\
             \"pr view\") cat \"$dir/view-output\" 2>/dev/null || exit 1; exit 0 ;;\n\
             \"pr create\") printf '%s\\n' \"$@\" > \"$dir/create-args\"; echo \"https://example.invalid/pull/7\"; exit 0 ;;\n\
             *) exit 1 ;;\n\
             esac\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        GhClient::with_executable(script)
    }

    /// The pull request a review names, as the task text carries it.
    const REVIEWED_PULL_REQUEST: &str = "https://github.com/owner/repo/pull/7";

    /// The branch a run owns, which a publish falls back to.
    fn own_branch(fixture: &Fixture) -> String {
        let task = fixture
            .store
            .load_run_input(fixture.run_id)
            .unwrap()
            .map(|input| input.task().to_owned());
        branch_name::branch_name(fixture.run_id, task.as_deref())
    }

    /// Everything a run that was asked to fix `REVIEWED_PULL_REQUEST` needs to
    /// publish onto it: the pull request's branch on origin at the worktree's
    /// base, and a `gh` that reports that branch as the pull request's head.
    fn arrange_reviewed_pull_request(fixture: &Fixture, directory: &Path, cross_repository: bool) {
        git(
            &fixture.source,
            ["push", "origin", "HEAD:refs/heads/pull-request-branch"],
        );
        fs::write(
            directory.join("view-output"),
            format!("pull-request-branch\t{cross_repository}\n"),
        )
        .unwrap();
        fs::write(
            directory.join("list-pull-request-branch"),
            format!("{REVIEWED_PULL_REQUEST}\n"),
        )
        .unwrap();
    }

    #[test]
    fn publish_commits_pushes_and_opens_a_pull_request_without_touching_the_source() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        // The operator is mid-edit: exactly the situation apply refuses and
        // publish exists for.
        fs::write(fixture.source.join("mid-edit.txt"), "mine\n").unwrap();
        let before = source_snapshot(&fixture.source);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "published\n").unwrap();
        fs::write(workspace.worktree_path().join("new.txt"), "added\n").unwrap();

        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        let branch = format!("senate/run-{}", fixture.run_id);
        assert_eq!(receipt.branch, branch);
        assert_eq!(
            receipt.pull_request,
            PullRequestStatus::Created("https://example.invalid/pull/7".to_owned())
        );
        let reference = format!("refs/heads/{branch}");
        assert_eq!(git_text(&origin, ["rev-parse", &reference]), receipt.commit);
        assert_eq!(
            git_text(workspace.worktree_path(), ["log", "-1", "--format=%s"]),
            format!("The Senate run {}", fixture.run_id)
        );
        assert!(git_status(
            workspace.worktree_path(),
            ["diff", "--quiet", "HEAD"]
        ));
        assert_eq!(
            source_snapshot(&fixture.source),
            before,
            "publish wrote to the operator's checkout"
        );
        // The run keeps every disposition: publish is transport.
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Completed
        );
        // And remembers where it went, so the interface can say so.
        let events = fixture.store.load_events(fixture.run_id).unwrap();
        assert!(events.iter().any(|sequenced| matches!(
            sequenced.event.kind(),
            DomainEventKind::RunPublished { branch: pushed, commit, pull_request_url }
                if *pushed == branch
                    && *commit == receipt.commit
                    && pull_request_url.as_deref() == Some("https://example.invalid/pull/7")
        )));
    }

    #[test]
    fn a_second_publish_reuses_the_branch_and_reports_the_open_pull_request() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "one\n").unwrap();
        fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        // The pull request now exists, and the worktree gained more work —
        // the shape of a fix cycle followed by another publish.
        fs::write(
            fixture.temp.path().join("list-output"),
            "https://example.invalid/pull/7\n",
        )
        .unwrap();
        fs::write(workspace.worktree_path().join("README.md"), "two\n").unwrap();
        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert_eq!(
            receipt.pull_request,
            PullRequestStatus::AlreadyExists("https://example.invalid/pull/7".to_owned())
        );
        let reference = format!("refs/heads/senate/run-{}", fixture.run_id);
        assert_eq!(git_text(&origin, ["rev-parse", &reference]), receipt.commit);
    }

    /// The pull request is the editing stage's own words when it wrote them:
    /// the drafted title becomes the commit subject and the pull request
    /// title, and the drafted description reaches gh unchanged.
    #[test]
    fn a_drafted_pull_request_is_quoted_over_the_task_text() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let _origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("new.txt"), "added\n").unwrap();
        let draft = PullRequestDraft {
            title: "Add the file the task asked for".to_owned(),
            body: "Fixes https://issues.invalid/1\n\n## Why\n\nIt was missing.".to_owned(),
        };

        fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, Some(&draft), &gh)
            .unwrap();

        assert_eq!(
            git_text(workspace.worktree_path(), ["log", "-1", "--format=%s"]),
            draft.title
        );
        let args = fs::read_to_string(fixture.temp.path().join("create-args")).unwrap();
        assert!(args.contains(&format!("--title\n{}\n", draft.title)));
        assert!(args.contains(&format!("--body\n{}\n", draft.body)));
        assert!(!args.contains("Opened by The Senate"));
    }

    /// A drafted title with nothing under it still names the work; only the
    /// description falls back to the task text.
    #[test]
    fn a_draft_without_a_description_borrows_the_task_for_the_body_alone() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let _origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("new.txt"), "added\n").unwrap();
        let draft = PullRequestDraft {
            title: "Add the file".to_owned(),
            body: String::new(),
        };

        fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, Some(&draft), &gh)
            .unwrap();

        let args = fs::read_to_string(fixture.temp.path().join("create-args")).unwrap();
        assert!(args.contains("--title\nAdd the file\n"));
        assert!(args.contains(&format!(
            "Opened by The Senate from run {}.",
            fixture.run_id
        )));
    }

    #[test]
    fn a_missing_gh_costs_the_pull_request_but_never_the_push() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let origin = add_origin(&fixture);
        let gh = GhClient::with_executable(fixture.temp.path().join("missing-gh"));
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "pushed\n").unwrap();

        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert!(matches!(
            receipt.pull_request,
            PullRequestStatus::Unavailable(_)
        ));
        let reference = format!("refs/heads/senate/run-{}", fixture.run_id);
        assert_eq!(git_text(&origin, ["rev-parse", &reference]), receipt.commit);
    }

    /// The whole point: fixing a reviewed pull request updates it.
    #[test]
    fn publishing_a_run_that_fixes_a_pull_request_pushes_onto_that_pull_request() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Review {REVIEWED_PULL_REQUEST} and fix what it finds"),
        );
        let origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), false);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "fixed\n").unwrap();

        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert_eq!(receipt.branch, "pull-request-branch");
        assert_eq!(receipt.note, None);
        assert_eq!(
            receipt.pull_request,
            PullRequestStatus::AlreadyExists(REVIEWED_PULL_REQUEST.to_owned())
        );
        assert_eq!(
            git_text(&origin, ["rev-parse", "refs/heads/pull-request-branch"]),
            receipt.commit,
            "the fix did not reach the pull request's own branch"
        );
        // No second pull request, and no branch of its own on origin to open
        // one from: that duplication is what this exists to prevent.
        assert!(
            !fixture.temp.path().join("create-args").exists(),
            "a second pull request was opened for the same change"
        );
        let own = format!("refs/heads/senate/run-{}", fixture.run_id);
        assert!(
            !git_ok(&origin, ["rev-parse", "--verify", "--quiet", &own]),
            "the run's own branch reached origin as well"
        );
    }

    /// The pull request's change is not in the operator's checkout — the usual
    /// case, since nobody checks out a branch before asking for it to be
    /// fixed. The run still starts from that change, so its stages see the
    /// code under review and publishing fast-forwards the pull request.
    #[test]
    fn a_run_on_a_pull_request_starts_from_its_head_and_publishes_onto_it() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Review {REVIEWED_PULL_REQUEST} and fix what it finds"),
        );
        let origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        // The pull request's commit lives on origin only: the checkout stays
        // on the commit before it.
        fs::write(fixture.source.join("change.txt"), "the pull request\n").unwrap();
        git(&fixture.source, ["add", "-A"]);
        git(&fixture.source, ["commit", "-m", "the pull request"]);
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), false);
        let tip = git_text(&fixture.source, ["rev-parse", "HEAD"]);
        git(&fixture.source, ["reset", "--hard", "HEAD~1"]);
        let before = source_snapshot(&fixture.source);

        let workspace = fixture
            .manager()
            .prepare_run_workspace_with(
                &mut fixture.store,
                fixture.run_id,
                &fixture.source,
                Some(&gh),
            )
            .unwrap();

        assert_eq!(workspace.base_commit(), tip);
        assert_eq!(
            fs::read_to_string(workspace.worktree_path().join("change.txt")).unwrap(),
            "the pull request\n",
            "the run cannot see the change it was asked about"
        );
        assert_eq!(source_snapshot(&fixture.source), before);
        let scratch = format!("refs/senate/pull-request-base/{}", fixture.run_id);
        assert!(!git_ok(
            &fixture.source,
            ["rev-parse", "--verify", "--quiet", &scratch]
        ));

        fixture.complete();
        fs::write(workspace.worktree_path().join("change.txt"), "fixed\n").unwrap();
        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert_eq!(receipt.branch, "pull-request-branch");
        assert_eq!(receipt.note, None);
        assert_eq!(
            git_text(&origin, ["rev-parse", "refs/heads/pull-request-branch"]),
            receipt.commit
        );
        assert!(!fixture.temp.path().join("create-args").exists());
    }

    /// A pull request `gh` resolves but whose branch cannot be fetched — an
    /// origin that is down, a network that drops — is not a reason to refuse
    /// the run. It starts from the checkout, as it did before runs could start
    /// from a pull request at all.
    #[test]
    fn a_pull_request_that_cannot_be_fetched_starts_from_the_checkout() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Review {REVIEWED_PULL_REQUEST} and fix what it finds"),
        );
        add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), false);
        let unreachable = fixture.temp.path().join("gone.git");
        git(
            &fixture.source,
            ["remote", "set-url", "origin", unreachable.to_str().unwrap()],
        );
        let head = git_text(&fixture.source, ["rev-parse", "HEAD"]);

        let workspace = fixture
            .manager()
            .prepare_run_workspace_with(
                &mut fixture.store,
                fixture.run_id,
                &fixture.source,
                Some(&gh),
            )
            .unwrap();

        assert_eq!(workspace.base_commit(), head);
    }

    /// A pull request the run cannot build on leaves the start as it always
    /// was: the operator's checkout.
    #[test]
    fn a_run_on_a_fork_pull_request_starts_from_the_checkout() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Review {REVIEWED_PULL_REQUEST} and fix what it finds"),
        );
        add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        fs::write(fixture.source.join("change.txt"), "the pull request\n").unwrap();
        git(&fixture.source, ["add", "-A"]);
        git(&fixture.source, ["commit", "-m", "the pull request"]);
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), true);
        git(&fixture.source, ["reset", "--hard", "HEAD~1"]);
        let head = git_text(&fixture.source, ["rev-parse", "HEAD"]);

        let workspace = fixture
            .manager()
            .prepare_run_workspace_with(
                &mut fixture.store,
                fixture.run_id,
                &fixture.source,
                Some(&gh),
            )
            .unwrap();

        assert_eq!(workspace.base_commit(), head);
        assert!(!workspace.worktree_path().join("change.txt").exists());
    }

    /// A pull request that gained commits while the run worked is still where
    /// that work belongs: the change is replayed on the new tip and pushed
    /// onto it, rather than becoming a second pull request for the same fix.
    #[test]
    fn a_pull_request_branch_that_moved_ahead_is_replayed_onto_and_pushed() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Review {REVIEWED_PULL_REQUEST} and fix what it finds"),
        );
        let origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), false);
        let workspace = fixture.prepare();
        fixture.complete();
        // Somebody pushed to the pull request after this run started.
        fs::write(fixture.source.join("theirs.txt"), "theirs\n").unwrap();
        git(&fixture.source, ["add", "-A"]);
        git(&fixture.source, ["commit", "-m", "theirs"]);
        git(
            &fixture.source,
            ["push", "origin", "HEAD:refs/heads/pull-request-branch"],
        );
        let tip = git_text(&fixture.source, ["rev-parse", "HEAD"]);
        fs::write(workspace.worktree_path().join("README.md"), "fixed\n").unwrap();

        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert_eq!(receipt.branch, "pull-request-branch");
        assert_eq!(
            receipt.pull_request,
            PullRequestStatus::AlreadyExists(REVIEWED_PULL_REQUEST.to_owned())
        );
        assert!(
            !fixture.temp.path().join("create-args").exists(),
            "a second pull request was opened for the change"
        );
        assert_eq!(
            git_text(&origin, ["rev-parse", "refs/heads/pull-request-branch"]),
            receipt.commit
        );
        // The commit that arrived while the run worked is the parent of what
        // was pushed: the replay moved this run's work, and nothing was
        // force-pushed over somebody else's.
        assert_eq!(
            git_text(&origin, ["rev-parse", "refs/heads/pull-request-branch^"]),
            tip
        );
        let note = receipt.note.expect("the replay went unexplained");
        assert!(note.contains(REVIEWED_PULL_REQUEST), "{note}");
        // The recorded base moved with the worktree, so what apply would diff
        // is still this run's delta and not the pull request's own commits.
        assert_eq!(
            WorkspaceManager::ready_workspace(&fixture.store, fixture.run_id)
                .unwrap()
                .base_commit(),
            tip
        );
    }

    /// A replay the two changes really do disagree about aborts, and the work
    /// goes to the run's own branch — without a pull request, because the one
    /// this run was asked to fix is still where the change belongs.
    #[test]
    fn a_conflicting_replay_keeps_the_run_branch_and_opens_no_pull_request() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Review {REVIEWED_PULL_REQUEST} and fix what it finds"),
        );
        let origin = add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), false);
        let workspace = fixture.prepare();
        fixture.complete();
        // Somebody rewrote the same lines this run is about to change.
        fs::write(fixture.source.join("README.md"), "theirs\n").unwrap();
        git(&fixture.source, ["add", "-A"]);
        git(&fixture.source, ["commit", "-m", "theirs"]);
        git(
            &fixture.source,
            ["push", "origin", "HEAD:refs/heads/pull-request-branch"],
        );
        let tip = git_text(&fixture.source, ["rev-parse", "HEAD"]);
        let base = workspace.base_commit().to_owned();
        fs::write(workspace.worktree_path().join("README.md"), "fixed\n").unwrap();

        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert_eq!(receipt.branch, own_branch(&fixture));
        let PullRequestStatus::Unavailable(reason) = &receipt.pull_request else {
            panic!(
                "a second pull request was opened: {:?}",
                receipt.pull_request
            );
        };
        assert!(reason.contains("conflict"), "{reason}");
        assert!(
            !fixture.temp.path().join("create-args").exists(),
            "a second pull request was opened for the change"
        );
        let note = receipt.note.expect("the fallback went unexplained");
        assert!(note.contains(REVIEWED_PULL_REQUEST), "{note}");
        // The work is safe on the run's own branch, the pull request keeps
        // the tip it had, and the aborted replay left the base where it was.
        let reference = format!("refs/heads/{}", own_branch(&fixture));
        assert_eq!(git_text(&origin, ["rev-parse", &reference]), receipt.commit);
        assert_eq!(
            git_text(&origin, ["rev-parse", "refs/heads/pull-request-branch"]),
            tip
        );
        assert_eq!(
            WorkspaceManager::ready_workspace(&fixture.store, fixture.run_id)
                .unwrap()
                .base_commit(),
            base
        );
    }

    /// A fork's branch is not a branch `origin` can be pushed to.
    #[test]
    fn a_pull_request_from_a_fork_keeps_the_runs_own_branch() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Review {REVIEWED_PULL_REQUEST} and fix what it finds"),
        );
        add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), true);
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "fixed\n").unwrap();

        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert_eq!(receipt.branch, own_branch(&fixture));
        let note = receipt.note.expect("the fallback went unexplained");
        assert!(note.contains("fork"), "{note}");
    }

    /// A task that merely mentions a pull request of another repository must
    /// not have its commits pushed at that pull request.
    #[test]
    fn a_pull_request_origin_does_not_serve_is_left_alone() {
        let mut fixture = Fixture::with_task(
            WorkflowKind::Standard,
            &format!("Port the fix from {REVIEWED_PULL_REQUEST} to this repository"),
        );
        add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        arrange_reviewed_pull_request(&fixture, fixture.temp.path(), false);
        // Origin's open pull request for that branch is a different one.
        fs::write(
            fixture.temp.path().join("list-pull-request-branch"),
            "https://github.com/owner/repo/pull/99\n",
        )
        .unwrap();
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "ported\n").unwrap();

        let receipt = fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        assert_eq!(receipt.branch, own_branch(&fixture));
        let note = receipt.note.expect("the fallback went unexplained");
        assert!(note.contains("not an open pull request"), "{note}");
    }

    #[test]
    fn publish_refusals_leave_the_workspace_unchanged() {
        // No origin remote: refused before anything is committed or pushed,
        // leaving the worktree's uncommitted delta exactly as it was.
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let gh = stub_gh(fixture.temp.path());
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "change\n").unwrap();
        assert!(matches!(
            fixture
                .manager()
                .publish_with(&mut fixture.store, fixture.run_id, None, &gh),
            Err(WorkspaceError::NoRemote(_))
        ));
        assert_eq!(
            git_text(workspace.worktree_path(), ["rev-parse", "HEAD"]),
            workspace.base_commit(),
            "a refused publish committed anyway"
        );
        assert!(!git_status(
            workspace.worktree_path(),
            ["diff", "--quiet", "HEAD"]
        ));

        // Nothing to publish: a worktree still at its base.
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let gh = stub_gh(fixture.temp.path());
        add_origin(&fixture);
        fixture.prepare();
        fixture.complete();
        assert!(matches!(
            fixture
                .manager()
                .publish_with(&mut fixture.store, fixture.run_id, None, &gh),
            Err(WorkspaceError::NothingToPublish)
        ));

        // A run that has not completed.
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        let gh = stub_gh(fixture.temp.path());
        add_origin(&fixture);
        fixture.prepare();
        assert!(matches!(
            fixture
                .manager()
                .publish_with(&mut fixture.store, fixture.run_id, None, &gh),
            Err(WorkspaceError::InvalidRunStatus { .. })
        ));

        // A detached review workspace has no branch to publish.
        let mut fixture = Fixture::new(WorkflowKind::Review);
        let gh = stub_gh(fixture.temp.path());
        add_origin(&fixture);
        fixture.prepare();
        fixture.complete();
        assert!(matches!(
            fixture
                .manager()
                .publish_with(&mut fixture.store, fixture.run_id, None, &gh),
            Err(WorkspaceError::ReviewWorkspaceNotApplicable)
        ));
    }

    #[test]
    fn a_published_run_still_cleans_up_completely() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "published\n").unwrap();
        fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        fixture
            .manager()
            .discard(&mut fixture.store, fixture.run_id)
            .unwrap();
        assert!(!workspace.worktree_path().exists());
        let reference = format!("refs/heads/senate/run-{}", fixture.run_id);
        assert!(
            !git_status(
                &fixture.source,
                ["rev-parse", "--verify", "--quiet", &reference]
            ),
            "the local branch outlived its discard"
        );
    }

    /// Publish is transport, not disposition: the same delta must still be
    /// transferable into the operator's checkout afterwards. This leans on
    /// `generate_patch` diffing the base commit against working-tree content —
    /// a publish commit on the branch must not change what apply transfers.
    #[test]
    fn apply_still_transfers_the_same_delta_after_publish() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "published\n").unwrap();
        fs::write(workspace.worktree_path().join("new.txt"), "added\n").unwrap();
        fixture
            .manager()
            .publish_with(&mut fixture.store, fixture.run_id, None, &gh)
            .unwrap();

        fixture
            .manager()
            .apply(&mut fixture.store, fixture.run_id)
            .unwrap();

        assert_eq!(
            fs::read_to_string(fixture.source.join("README.md")).unwrap(),
            "published\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("new.txt")).unwrap(),
            "added\n"
        );
        assert_eq!(
            fixture.store.load_run(fixture.run_id).unwrap().run.status(),
            RunStatus::Applied
        );
    }

    #[test]
    fn a_run_frozen_mid_apply_recovery_cannot_publish() {
        let mut fixture = Fixture::new(WorkflowKind::Standard);
        add_origin(&fixture);
        let gh = stub_gh(fixture.temp.path());
        let workspace = fixture.prepare();
        fixture.complete();
        fs::write(workspace.worktree_path().join("README.md"), "changed\n").unwrap();
        let crashing = WorkspaceManager::with_fault(&fixture.root, FaultPoint::GitApplied);
        assert!(matches!(
            crashing.apply(&mut fixture.store, fixture.run_id),
            Err(WorkspaceError::InjectedCrash(_))
        ));

        assert!(matches!(
            fixture
                .manager()
                .publish_with(&mut fixture.store, fixture.run_id, None, &gh),
            Err(WorkspaceError::ApplyInProgress(_))
        ));
    }

    #[test]
    fn publish_titles_and_bodies_survive_odd_tasks() {
        let run_id = RunId::from_u128(9);
        assert_eq!(
            publish_title(None, run_id),
            format!("The Senate run {run_id}")
        );
        assert_eq!(
            publish_title(Some("  fix the flaky test\nwith details  "), run_id),
            "fix the flaky test"
        );
        let long = "α".repeat(100);
        let title = publish_title(Some(&long), run_id);
        assert_eq!(title.chars().count(), 72);
        assert!(title.ends_with('…'));
        // A drafted title is bounded the same way the task's first line is.
        assert_eq!(bounded_title(&long), title);
        assert_eq!(bounded_title("short"), "short");
        assert_eq!(
            publish_body(None, run_id),
            format!("Opened by The Senate from run {run_id}.")
        );
        assert!(publish_body(Some("task text"), run_id).starts_with("task text\n\n---\n"));
    }

    /// The common task shape: an instruction word and a link. The link says
    /// everything about the work and nothing a pull request list can read,
    /// so the title comes from the issue it points at and the description
    /// opens with the `Fixes` line that closes it.
    #[test]
    fn a_task_that_is_mostly_a_link_titles_the_pull_request_from_its_issue() {
        let run_id = RunId::from_u128(9);
        let linear = "Work on https://linear.app/a8c/issue/DOTCOM-17972/stepper-transfer-waits";
        assert_eq!(
            publish_title(Some(linear), run_id),
            "DOTCOM-17972: stepper transfer waits"
        );
        assert_eq!(
            publish_body(Some(linear), run_id),
            format!(
                "Fixes https://linear.app/a8c/issue/DOTCOM-17972/stepper-transfer-waits\n\n{linear}\n\n---\nOpened by The Senate from run {run_id}."
            )
        );
        assert_eq!(
            publish_title(
                Some("https://github.com/Automattic/wp-calypso/issues/114037"),
                run_id
            ),
            "wp-calypso#114037"
        );
        // A task that says something of its own keeps saying it, without the
        // link it happens to carry.
        assert_eq!(
            publish_title(
                Some("Fix the flaky transfer test https://linear.app/a8c/issue/DOTCOM-1/x"),
                run_id
            ),
            "Fix the flaky transfer test"
        );
        // No issue link, nothing but a link: the run names itself as before.
        assert_eq!(
            publish_title(Some("https://example.invalid/notes"), run_id),
            format!("The Senate run {run_id}")
        );
        assert!(
            !publish_body(Some("See https://example.invalid/notes"), run_id).starts_with("Fixes ")
        );
    }

    fn init_repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, ["init", "-b", "main"]);
        git(path, ["config", "user.name", "The Senate Test"]);
        git(path, ["config", "user.email", "senate@example.invalid"]);
        git(path, ["config", "commit.gpgsign", "false"]);
        git(path, ["config", "core.hooksPath", ".git/senate-no-hooks"]);
        git(path, ["config", "core.autocrlf", "false"]);
        fs::write(path.join("README.md"), "base\n").unwrap();
        git(path, ["add", "README.md"]);
        git(path, ["commit", "-m", "initial"]);
    }

    fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
        git_os(cwd, args.map(OsStr::new));
    }

    fn git_os<const N: usize>(cwd: &Path, args: [&OsStr; N]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Whether a Git command succeeds, for asking a question whose answer is
    /// allowed to be no — "is there such a ref".
    fn git_ok<const N: usize>(cwd: &Path, args: [&str; N]) -> bool {
        Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    }

    fn git_text<const N: usize>(cwd: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn git_status<const N: usize>(cwd: &Path, args: [&str; N]) -> bool {
        Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap()
            .success()
    }
}
