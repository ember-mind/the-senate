# Workspace isolation, apply, discard, pull request

Every run works in its own Git worktree; the source checkout changes only when you explicitly apply, and a run can instead be published as a branch and pull request.

## Sub-features
- jira-credentials: setup commands, Git and GitHub CLI subprocesses omit Jira importer credentials.
- worktree: `~/.senate/worktrees/<sanitized-repo>-<common-dir-hash>/<run-id>` from the committed `HEAD` at creation (the base commit).
- modes: implementation workflows own a `senate/<slug>-<id-tail>` branch — `<slug>` is the issue key from a Linear or GitHub URL or bare `KEY-123` in the task (`dotcom-17972`), else the task's first words; `<id-tail>` is the last 6 characters of the run id, and a run without input falls back to `senate/run-<run-id>`; review workflows use a detached worktree.
- setup: the `[setup]` table of `.senate.toml` (`commands`, `timeout_seconds`, default 3600 s), found worktree-then-source-repository like every other table, runs in the new worktree after validation and before the workspace is marked `Ready` — so no stage ever sees a tree whose gitignored build output is missing. Argv, no shell, stops at the first failure. A failing command raises `SetupFailed` carrying the command, why it failed, and the last 20 lines of each stream; a table that cannot be read raises `SetupConfig`. Preparation fails in both cases and the workspace never reaches `Ready`. Resuming that run runs setup again over the worktree that is already there rather than handing it over — the failure leaves the workspace `Preparing`, not `Broken`, so correcting the command and pressing `r` is the recovery. Repositories with no `[setup]` table are unaffected.
- apply: patch transfer from base commit to a clean source checkout; no staging, no commit.
- rebase: `senate rebase <run-id>` (TUI `b`) replays a completed run's delta on the source checkout's current `HEAD`, for the checkout that moved on while the run was working and now refuses the patch. The delta is committed on the run's own branch first (as publish commits it), then `git rebase --onto <HEAD> <base>`; a conflict aborts, changes nothing, and keeps the old base. The workspace's `base_commit` moves, the spent `failed` apply intent is dropped with it, and a `WorkspaceRebased` event records the move.
- rebase-verification: the move makes an earlier passed verification stale — it ran over the old base — so apply refuses with `verification predates the rebase onto <commit>` until a later cycle's verify stage completes after the rebase. `fix` and `continue` both append one. `pr` is not gated by it, exactly as it is not gated by a failed verification.
- verification-gate: `apply` refuses unless the run's latest Verify stage — the last one in the graph, since every fix or continue cycle appends its own — is `Completed` (`verification did not pass: stage verify is failed`). Older verify stages do not count: a failed `verify` answered by a passed `verify_1` no longer blocks. Checked before the run-status check. `pr` does not share the gate: it writes a branch and a pull request rather than the operator's checkout, so a red check is something CI and the reviewer see, not a reason to strand the delta in a worktree.
- pr: commit the delta on the run's branch, push to `origin`, open a PR through `gh`; source checkout untouched.
- pr-draft: the PR title and description are quoted from the `## Pull request` section of the latest editing stage's artifact (Implementation, Simplification, Fix, FollowUp); the task's first line and text stand in when no stage wrote one.
- discard: record `Discarded`, then remove owned worktree and branch.
- reclaim: applying removes the worktree and keeps the branch, because an applied run can be neither applied again nor fixed. A sweep at TUI startup does the same for `Applied` and `Discarded` runs that still hold one. `Completed` runs are never swept — apply, fix and continue all read their worktree.
- diff-preview: the same delta apply would move, read-only (`d` in the TUI).
- reconciliation: persisted workspace intent is validated against Git evidence before any lifecycle command.

## How to get to it (user POV)
Nothing is required to get a worktree; every run gets one. When a run is `Completed`, choose `apply` to put the changes in your checkout, `pr` to publish without touching it, or `discard` to drop them. In the TUI these are `a`, `P` and `X`, each followed by Enter in a confirmation overlay. `d` previews the diff first.

## Driving it
```bash
senate apply <run-id>
senate rebase <run-id>
senate pr <run-id>
senate discard <run-id>
senate status <run-id>      # Workspace and Base lines
```
TUI run detail: `d` diff preview, `a` then Enter apply, `b` then Enter rebase onto HEAD, `P` then Enter push to PR (a "pushing to PR" card while it runs, then a result card with the PR URL: `o` open, `y` copy), `X` then Enter discard. `b` is not on the action row: an apply refused by a checkout that moved forward (`PatchCheckFailed { rebasable: true }`) opens the rebase confirmation itself.

CLI `pr` prints its progress line to stderr and the receipt (branch, commit, elapsed, PR URL on its own line) to stdout.

## Where it lives
- `src/workspace/manager.rs` — prepare/apply/rebase/publish/discard sagas, `publish`, `publish_target` (push onto a reviewed pull request's own branch), `land_on_moved_pull_request`/`replay_onto_pull_request` (replay onto one that moved), `open_pull_request` (the fallback's pull request), `rebase`, `patch_refusal_reason` (why `git apply --check` said no), `ensure_verification_passed` and `ensure_verification_follows_rebase` (apply only).
- `src/workspace/error.rs` — `WorkspaceError`, incl. `VerificationNotPassed`, `VerificationPrecedesRebase`, `RebaseAlreadyCurrent`, `RebaseBaseNotAncestor`, `RebaseConflict`, `RebaseBlockedByApply`, `SetupConfig`, `SetupFailed`.
- `src/workspace/setup.rs` — the `[setup]` table reader and its runner, called from `prepare_run_workspace` and from `reconcile_preparing` before the workspace is marked ready.
- `src/workspace/model.rs` — `RunWorkspace`, `WorkspaceStatus`, apply operation records.
- `src/workspace/github.rs` — `gh pr list --head` / `gh pr view` / `gh pr create` boundary; `PullRequestRef` parses the pull request out of the task text.
- `src/workspace/pull_request.rs` — `PullRequestDraft`, `extract` of the artifact's `## Pull request` section; `src/app/query.rs` `pull_request_draft` walks the editing artifacts newest first and takes the first that wrote the section. When none did, `publish_title`/`publish_body` in `manager.rs` name the run from the task with its links dropped, falling back to the linked issue.
- `src/git/worktree.rs`, `src/git/patch.rs`, `src/git/remote.rs`, `src/git/repository.rs` — Git commands with direct argv.
- `src/store/workspace.rs` — workspace and apply-intent persistence with CAS; `rebase_workspace_base` moves the base and drops the superseded `failed` intent in one transaction.
- `src/store/path.rs` — `worktree_root`, `SENATE_DATA_DIR`.
- `src/app/run_service.rs` — `apply_run`, `rebase_run`, `publish_run`, `discard_run`, `preview_run_diff`.
- `src/tui/state.rs` `PublishInFlight`/`PublishOutcome`, `src/tui/render.rs` `render_publishing`/`render_published`, `src/tui/desktop.rs` `open_in_browser`/`copy_to_clipboard` (platform `open`/`xdg-open`, `pbcopy`/`wl-copy`/`xclip`).
- `tests/codex_cli.rs` — `native_codex_fixture_runs_through_tmux_preserves_source_then_applies`.

## Gotchas
- A refused apply says why in the same sentence: how far the checkout moved past the run's base, and that `senate rebase <run>` moves the change to meet it.
- Rebase refuses a `prepared` or `applied_to_source` intent (`RebaseBlockedByApply`): part of the old patch may already be in the checkout. It also refuses a run already on `HEAD`, and a checkout that no longer contains the run's base (rewound or moved to unrelated history).
- A conflicting rebase leaves the delta as a commit on the run's branch — the commit is made before the rebase and survives the abort — but the base, the checkout and the apply intent are untouched.
- Apply requires a fully clean source checkout; preparation does not (the worktree starts from committed `HEAD`).
- Apply never stages or commits; an empty diff is a successful no-op (`No workspace changes to apply.`).
- `pr` rejects non-completed runs, detached (review) workspaces, empty deltas and repositories without an `origin` remote. A failed verification is not among them.
- A Standard/Deep run can be `Completed` and still unapplicable: a failed verification completes the run (the decision's edge to it is optional) precisely so it can be fixed in place; `apply`/`pr` name verification until a later cycle's check passes. A Fast run with a failed verify is `Failed` instead; `retry` the verify stage there. PR failure (no `gh`, not authenticated) is reported in the receipt and never undoes the push.
- `pr` never force-pushes; a diverged remote branch is an error by design. `GIT_TERMINAL_PROMPT=0` turns a credential prompt into an error instead of a hang.
- When the run's task names a pull request, `pr` pushes the commit onto that pull request's own head branch instead of opening a second one for the same change — the shape of "review this PR" followed by a fix cycle. `publish_target` in `manager.rs` requires three things to resolve it: `gh pr view` readable, the head not in a fork, and `gh pr list --head <branch>` returning that same pull request (so a task merely mentioning somebody else's PR is left alone). Any of those failing keeps the run's own branch and puts the reason in `PublishReceipt::note`, shown on the result card and printed by the CLI — the fallback is never silent. Nothing is ever force-pushed.
- A resolved pull request whose remote tip is already an ancestor of the commit is pushed onto directly. A tip that is not — the pull request gained commits while the run worked, or the fix cycle's worktree was based on a checkout that was never on that branch — is **replayed onto**: `land_on_moved_pull_request` runs `git rebase --onto <tip> <base>` in the worktree (`replay_onto_pull_request`, the same move `rebase` makes onto a checkout that walked on), pushes the replayed commit onto the PR's branch, and says so in the note. The push still fast-forwards; nothing is force-pushed.
- The replay records the new base through `rebase_workspace_base`, exactly as `rebase` does, because the recorded base is what apply diffs against — left behind, a later apply would carry the pull request's own commits as if the run had written them. Verification goes stale with it, so `apply` refuses until a later cycle's check passes on the new base.
- A conflicting replay aborts and changes nothing: the work is pushed to the run's own branch and **no pull request is opened for it**. Everywhere else a fallback opens one; here the place to read the change already exists and is named in the run's task, so a second one would be the duplicate this path exists to avoid. The card reads `BRANCH PUSHED`, the reason names the conflict, and the note says where the work is.
- publish-record: every successful push commits a `RunPublished { branch, commit, pull_request_url }` run event. `RunDetails::publication` folds the latest one and marks it `outdated` when a `RunFixRequested`, `RunContinueRequested` or `WorkspaceRebased` follows it; the TUI's `P` label reads from it, and `P` on a current publication with a URL opens the PR rather than pushing. Runs published before this event existed read as not pushed.
- After `pr` the run stays `Completed`, so apply, fix and discard remain available; publishing again after a fix updates the same branch and PR. The PR body is only written on creation: a fix's fresh draft changes the commit subject but not an already-open PR's text.
- The drafted title is cut at 72 characters; a corrupt latest editing artifact fails the publish (artifact integrity fails closed) rather than silently publishing from the task.
- Discard commits the logical disposition before cleanup; cleanup is idempotent and retains process files. Branch deletion needs persisted ownership and an unchanged tip, otherwise the workspace turns `Broken` and the branch is kept.
- Reclaiming after apply keeps the branch because apply leaves its change unstaged in the source checkout — the branch holds the only commits of that work. The disposition is persisted with the removal intent (ownership is released before `Removing`), so a crash mid-removal still resumes without deleting the branch.
- `Broken` workspace status (moved repo, foreign path, branch collision) blocks execution; The Senate never guesses or deletes foreign data.
- `Broken` is re-observed on every reconcile, so a run recovers once the condition is gone. A workspace broken while its run was still `Preparing` is put back on the preparing path instead of merely observed: that is the one case where the worktree does not exist yet, and only preparation builds it and ends the run's preparation. Before, such a run stayed `Preparing` forever, reporting `ready worktree is missing` about a worktree that had never been ready; the whole preparation check runs again, so a condition that is still there breaks it again with the current reason.
- Ordinary run commits and cleanup are rejected while an apply intent is `Prepared` or `AppliedToSource`.
