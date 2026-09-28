# Run lifecycle

Start one task as a run, watch it move through its stages, and get it moving again when it stops for you, for a failure, or for a crash.

## Sub-features
- start: `fast`/`standard`/`deep`/`review` create a run, prepare its workspace, and drive it to the first quiescent state.
- start-preconditions: a dirty source repository is refused; so is a task naming a pull request `gh` cannot read. Both run before anything durable exists.
- statuses: Created -> Preparing -> Ready -> Running -> {NeedsUser, Paused, Interrupted, Completed, Failed} -> {Applied, Discarded}. In Standard/Deep a failed verify stage still yields `Completed` (its edge into the decision is optional) and apply is gated separately; in Fast it is a leaf and the run is `Failed`. See verification.md.
- ready-boundary: `Ready` is a persisted atomic boundary; configuration and workspace succeeded but nothing executed yet.
- resume-vs-recover: `Paused` (user asked) takes Resume; `Interrupted` (process or runtime lost) takes Recover. One command, `resume`, does both.
- stop: interrupts live managed processes, then commits a run-level `Interrupted`; nothing is discarded.
- retry: returns one `Failed` stage to `Pending`, together with every downstream stage its failure skipped, only while every other downstream stage is still `Pending` or `Ready`. `--provider` (TUI: the `t` chooser) sends that one stage to another provider first; see routing.md `retry-override`.
- attention: `NeedsUser` holds one or more attention requests (permission, decision, question) that `resolve` answers.
- inspect: `runs` lists runs; `status` prints routing, per-stage evidence, attention and usage.
- diagnosis: every stage that stopped or has not started says why. A failed stage prints `reason: <provider text>`; a Pending/Ready stage prints `waiting on: <ids>` (plus `(degraded: <ids>)` for satisfied optional edges) or `blocked by: <id> (failed|skipped)`. The run-level reason is the blocking stage's, and the TUI's activity strip says the same in prose.

## How to get to it (user POV)
From a Git checkout, run one workflow command with the task text. The command prints committed events and the run's details, then exits when the run is quiescent (completed, needs you, paused, interrupted, failed, or waiting for a provider). Use `senate runs` to find the run id and `senate status <run-id>` to inspect it. In the TUI the same actions are `r` (resume/recover), `s` (stop), `t` (retry the selected failed stage, after choosing which provider runs it) and `u` (resolve attention) on the run detail screen.

## Driving it
```bash
senate fast "<task>" [--repo <path>] [--provider claude|codex|fake | --profile recommended] [--effort native|low|medium|high]
senate standard "<task>" [same flags]
senate deep "<task>" [same flags]
senate review "<task>" [same flags]
senate runs
senate status <run-id>              # failed stages add `reason:`; pending ones add `waiting on:` / `blocked by:`
senate resume <run-id>
senate stop <run-id>
senate retry <run-id> <stage-id>
senate retry <run-id> <stage-id> --provider claude [--model <id>]   # retry this stage on another provider
senate resolve <run-id> <attention-id>                    # approve a permission request
senate resolve <run-id> <attention-id> --response "<answer>"   # answer a question
senate auto-approve <run-id>            # stop asking about this run's permission requests
senate auto-approve <run-id> --off      # go back to being asked
```
TUI keys on the run detail screen: `r` resume/recover, `s` stop, `t` retry selected failed stage (↑/↓ pick Configured provider / Claude / Codex, plus a fallback Codex model when Codex refused the stage's model; Enter retries), `u` open attention overlay (↑/↓ pick request, type a response, Enter resolves), `A` arm or disarm automatic approval.

## Approving without being asked
Answering the same permission request all day is a cost, not a safety property, so a run can be armed to approve them itself: `A` on the run detail screen, or `senate auto-approve <run-id>`. The flag lives on that run — it survives a restart, reaches no other run, and no existing run is armed by upgrading. While it is armed the header carries an `AUTO-APPROVE` chip and the footer offers `A` to withdraw it.

Armed is not unconditional. A question still stops the run, because no approval stands in for an answer, and so does a request that cannot be expressed as an exact permission rule, because approving it would not grant anything. Approval also stops after `MAX_AUTO_APPROVALS` requests in one settling: a stage that keeps asking is one something is wrong with, and the operator gets the run back instead of a loop. Arming a run that is already waiting also resumes it — that is what "stop asking me" means.

## Archiving and deleting
A run you are done with is archived (`h` in the Runs list, `senate archive <run-id>`): it leaves the default list and stays otherwise untouched, and `--undo` brings it back. An archived run — and only an archived one — can then be deleted for good (`D` in the Runs list, `senate delete <run-id> --yes`): its worktree and branch, its artifacts and logs under `~/.senate/runs/<run-id>`, and every row The Senate keeps about it. Nothing it already applied or published is touched. There is no undo, and a run still running cannot be deleted.

## Where it lives
- `src/cli/mod.rs` — clap definitions (`RunArgs`, `Command::{Runs,Status,Resume,Stop,Retry,Resolve}`).
- `src/cli/commands.rs` — `start`, `parse_effort`, `print_report`, `print_details`; `QuiescentState` hints printed after each report.
- `src/workspace/github.rs` — `PullRequestRef::parse`, `GhClient::pull_request_reach`, `PullRequestReach`: the start precondition's probe.
- `src/app/run_service.rs` — `start_run`, `resume_run`, `stop_run`, `retry_stage`, `resolve_attention_with_response`, `inspect_run`, `list_runs`; `settle` and `MAX_AUTO_APPROVALS` for automatic approval; `ABANDONED_AFTER` 30 s observe pass.
- `src/domain/run.rs` — `Run` aggregate, `RunTransition`, `ensure_retry_safe` (`RetryWouldInvalidate`), `skipped_descendants`.
- `src/engine/scheduler.rs` — `retry_stage` returns the skipped descendants to `Pending` in the same commit, and commits the route override (when given) in that commit too.
- `src/domain/stage.rs` — stage state machine.
- `src/domain/attention.rs` — attention request lifecycle.
- `src/app/query.rs` — `RunDetails`, `StageSummary`, `AttentionSummary` DTOs behind `status`; `failure_reason`, `blocking`, `StageWaitingSummary`, `StageDependencyRef`, `BlockedDependencyRef`.
- `src/cli/commands.rs` — `waiting_line`, `dependency_ids`, `blocked_ids`, `outcome_word`: the one extra indented line per stage.
- `src/tui/render.rs` — `status_sentences`, `failed_stage_sentence`, `failed_stage_reason`, `activity_message`, `waiting_message`, `blocked_message`: the same diagnosis as prose in the activity strip.
- `tests/cli.rs` — restart survival, default profile, read-only `runs`/`status`.
- `tests/codex_cli.rs` — `stop_interrupts_a_live_run_while_its_driver_is_still_attached`, detach + resume consuming retained output, retry creating a new native thread.

## Gotchas
- Blocked quiescent states (`needs_user`, paused, interrupted, failed) exit 0; only operational errors exit 1 and clap errors exit 2. Do not treat exit 0 as "completed".
- The pull request in a task need not belong to the run's repository — reviewing a remote one is a supported run — so the precondition asks whether it can be *read*, never whether it matches the checkout. A pull request that cannot be read fails the start with `PullRequestUnreachable`; run `01M1K8KAJ1HMS47H7WR8YMN2PW` had no route to `github.a8c.com`, and all five stages wrote "I was blocked" artifacts under `"subtype":"success"`. A `dial tcp … i/o timeout` in the detail is a network failure, not an auth one: `gh` ignores `~/.ssh/config`, so a host behind a SOCKS proxy needs `HTTPS_PROXY` set (or a `gh` wrapper that sets it), not a re-login.
- A missing `gh` never blocks a start. Absence of `gh` is not evidence of absent access: an MCP provider or a pasted diff may still carry the pull request, so the probe reports `Unknown` and the run proceeds.
- The probe inherits `gh`'s own timeout (~30 s against an unroutable host). That is the cost ceiling of the precondition, paid once instead of by every stage.
- `resume` never bypasses attention and never retries a failed stage; use `resolve` or `retry` explicitly.
- Retry is rejected once any downstream stage, direct or transitive, has started or reached an outcome other than `Skipped`; the error is `RetryWouldInvalidate`. Skipped stages return to `Pending` with the retried one, so a Fast implementation whose failure skipped `verify` can still be retried.
- `stop` is refused unless the run is `Running` or `NeedsUser`; an `Interrupted` run reports its existing state instead of a second interruption.
- `stop` cannot interrupt a verify stage mid-way: its commands are not managed processes, the synchronous poll runs them to the end, and the driver's commit then loses to the stop; `resume` reconciles the state afterwards.
- `stop` is retried on lost-revision races because another Senate process is usually still driving the run; the classifier is `AppError::is_concurrent_modification`, which asks each wrapper. `#[error(transparent)]` makes `.source()` skip the wrapper level, so walking the source chain silently missed the store error and 7 to 20 percent of stops failed under load. Add explicit `is_retryable`-style methods to wrappers; never rely on `.source()` through transparent errors.
- A run reading `Running` whose processes all ended and nothing touched for 30 s is settled by a read (`runs`, `status`, TUI refresh) through `ResumeAction::Observe`; a read never resumes provider work.
- Pre-M5 runs are inspectable but cannot resume when immutable input or execution config is absent (`<legacy input unavailable>` in `status`).
- Routes are resolved once at creation; provider loss after creation fails the stage with configured-provider-unavailable, never reroutes.
- The run-level failure reason is the *blocking* stage's, not the first failed one in workflow order. A failed optional dependency that nothing required (a review beside `synthesis`) never becomes the run's reason; `StageSummary::blocking` marks the one that does.
- `blocked by` states each dependency's own outcome: a dependency that was skipped is reported as skipped, never as failed.
- `waiting on` is printed only for a `Pending` or `Ready` stage. A stage whose dependencies are all satisfied prints nothing extra, even in the moment before the scheduler marks it Ready.
