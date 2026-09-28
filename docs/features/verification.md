# Verification

Run the target repository's own verification commands inside the run's worktree after the last stage that edits it, record every command and exit code, and let the change be applied to the operator's checkout only once the latest verification passed. No agent is involved.

## Sub-features
- verify-stage: `StageKind::Verify` / `Role::Verifier`, stage id `verify` in Fast, Standard and Deep (`verify_<n>` in a fix cycle, `followup_verify_<n>` in a continue cycle); absent from Review, which edits nothing.
- placement: after the last editing stage (Implementation in Fast, Simplification in Standard/Deep, the Fix or FollowUp in a cycle); beside the two reviews, not after them. The Decision depends on it *optionally*: it waits for the verdict and receives the artifact, but a failed verification still reaches the lead and the run still completes.
- verify-provider: provider id `verify`, a synchronous command runner; every `Role::Verifier` stage routes to it implicitly, never through a routing snapshot or profile.
- commands-source, in order: the `[verify]` table of `<worktree>/.senate.toml`; else the `[verify]` table of `<source repo>/.senate.toml` (the checkout the worktree was cut from, so an untracked file there configures a repository The Senate cannot commit to); else auto-detection (`Cargo.toml` -> `cargo test`, `package.json` -> `npm test`, `pyproject.toml` or `pytest.ini` -> `pytest`, `go.mod` -> `go test ./...`, first match wins); else nothing.
- detection-declined: a matched `package.json` is read before its command is trusted. A workspaces root (`workspaces` as a non-empty array, or an object with a non-empty `packages`) and a manifest with no non-blank `scripts.test` both yield no commands, `CommandSource::Declined`, and a `## Source` line naming the file, the reason, and the `[verify]` table to add. Only `package.json` is second-guessed: `cargo test` and `go test ./...` are the right command at a workspace root. An unreadable or unparseable `package.json` falls through to the guess rather than failing the stage.
- base-commit-placeholder: `{base_commit}` in any configured command is replaced with the run's base commit before it runs (`yarn test-client --changedSince={base_commit}`). Substitution happens before the argv split, which is safe because a commit ID has no whitespace. A command that asks for it on a run with no recorded base commit fails the stage rather than passing the literal text to the tool.
- repeat-detection: a failed verification whose bottom line matches an earlier `Verify` artifact of the same run at the same base commit gains a `## Not the first time` section naming that stage. Two different trees failing identically points at the repository rather than at either change; it is worded as a hint, not a verdict, and never appears on the first failure or on a pass.
- artifact: `~/.senate/runs/<run-id>/artifacts/verify.md` — `# Verification`, `## Bottom line`, `## Source`, then one `### $ <command>` section per command with `exit:` and fenced stdout/stderr; the TUI quotes the bottom line and renders the rest. `## Source` names the checkout a `[verify]` table came from: `` `.senate.toml` `[verify]` table (worktree) `` or `(source repository)`.
- outcome: every exit zero -> stage Completed (`passed — N commands`); first non-zero exit -> stage Failed with the bottom line as reason, later commands skipped; nothing configured or detected -> Completed (`nothing checked — no commands configured or detected`); unreadable `[verify]` table -> Failed with the parse error.
- apply-gate: `apply` refuses unless the run's *latest* verify stage (highest cycle) is Completed — `verification did not pass: stage verify is failed` — checked before the run-status check. An older failed verify followed by a passed `verify_n` does not block. `pr` is **not** gated: a branch and a pull request are where an unverified change is meant to be graded, and a `[verify]` table often fails for reasons the change never caused. The TUI's publish confirmation names the failed stage instead. A `pr` that replays the change onto a pull request that moved does move the base, so it makes the checks stale the way `rebase` does and `apply` refuses until they run again.
- fix-loop (Standard/Deep): a failed verification completes the run with the decision reading the failure, so `fix` (or continue) stays available: `fix_n` -> `verify_n` -> `decision_n`, and the apply gate opens once `verify_n` passes. `retry` is not available on a `Completed` run.
- fast-failure: in Fast the verify stage is a leaf, so its failure fails the run; `retry <run-id> verify` re-runs the same commands, and `fix` is unavailable (no decision).
- retry-unskip: retrying a failed stage returns the stages its failure skipped (verify among them) to pending with it, so a Fast implementation can still be retried.

## How to get to it (user POV)
Nothing to enable: every Fast, Standard and Deep run verifies. Put a `[verify]` table in the repository's `.senate.toml` to say exactly what "verified" means there; without one, The Senate runs the one command its build file implies. For a repository you cannot commit to — someone else's, or one where a local tool's config does not belong — put that file untracked in your own checkout (`git update-index --skip-worktree` is not needed; add it to `.git/info/exclude`): every run cut from that checkout reads it. Read the result in `senate status <run-id>` (the `verify` stage line) or open the artifact with `o` on the stage in the TUI. What a failure means depends on the workflow. Standard/Deep: the run still completes with the decision having read the failure; `apply` refuses until a later verification passes while `pr` stays open, and `fix` is the path (each fix cycle re-verifies) — `retry` does not apply to a completed run. Fast: the run fails, `retry <run-id> verify` re-runs the same commands, and `fix` is unavailable because Fast has no decision. In both, `discard` is always open.

## Driving it
```bash
senate fast "<task>"            # Implementation -> Verify
senate standard "<task>"        # ... -> Simplification -> Verify (beside the reviews) -> Decision
senate status <run-id>          # verify stage line; artifact bottom line
senate retry <run-id> verify    # Fast only: the run failed, re-run the same checks
senate fix <run-id>             # Standard/Deep: fix_1 -> verify_1 -> decision_1; the gate opens when verify_1 passes
senate apply <run-id>           # refused while the latest verification did not pass
senate pr <run-id>              # publishes anyway: the branch is where a red check gets read
```
`<repo>/.senate.toml`:
```toml
[verify]
commands = ["cargo fmt --check", "cargo clippy --all-targets", "cargo test"]
timeout_seconds = 1800
```

## Where it lives
- `src/providers/verify/mod.rs` — `VerifyProvider`, two polls per attempt (start, then run everything), idempotent re-poll from the recorded artifact.
- `src/providers/verify/config.rs` — `.senate.toml` `[verify]` reader, detection rules, `DEFAULT_TIMEOUT` (1800 s).
- `src/providers/repo_config.rs` — which checkout `.senate.toml` is read from (`locate`, `ConfigOrigin`), shared with the `[permissions]` and `[setup]` readers.
- `src/workspace/setup.rs` — the `[setup]` table, run during workspace preparation (see workspace.md).
- `src/providers/verify/runner.rs` — direct-argv runner, output draining, `try_wait` loop with kill on timeout.
- `src/providers/verify/artifact.rs` — Markdown rendering, tail truncation, write-once persistence.
- `src/domain/workflow.rs` — `StageKind::Verify`, `verify` in `fast_stages`/`standard_stages`/`deep_stages`, `fix_cycle_stages`, `continue_cycle_stages`, `without_verification` (evals).
- `src/domain/role.rs` — `Role::Verifier`; `src/domain/artifact.rs` — `ArtifactKind::Verify`.
- `src/app/routing.rs` — `VERIFY_PROVIDER_ID`, implicit `RoutingPlan::route(Role::Verifier)` and `ResourcePlan::effort(Role::Verifier)`.
- `src/app/provider_factory.rs` — `RuntimeProvider::Verify`, `"verify"` arm of `runtime_for`.
- `src/workspace/manager.rs` — `ensure_verification_passed` in `apply` alone; `src/workspace/error.rs` — `VerificationNotPassed`.
- `src/domain/run.rs` — `skipped_descendants`, retry safety over transitive descendants; `src/engine/scheduler.rs` — `retry_stage` un-skips.
- `src/app/run_service.rs` — `a_standard_run_runs_the_repositorys_verify_commands_and_can_be_applied`, `a_failing_verify_command_fails_the_run_and_apply_names_verification`.

## Gotchas
- Synchronous: the whole command sequence runs inside one provider poll, so a long test suite blocks the driving process (CLI command or TUI worker) for its duration. The default limit is 1800 s per command.
- `stop` does not stop a running verification: the commands are not managed processes, so the poll runs the suite to its end; the driver then fails its commit with a concurrency error and the run's state is reconciled on the next `resume`. Nothing is lost, but the suite finishes first.
- A timeout kills the whole process group of the command (test runners fork workers that would otherwise keep the output pipes open); it is reported as `timed out after N s`.
- Commands are argv, not shell: the string is split on whitespace and the first word is the program. No pipes, globs, redirections, `&&` or environment expansion; write a script in the repository and name it instead.
- The first failure stops the sequence; later commands are listed as skipped in the artifact and never run.
- Nothing detected means nothing checked: the stage completes with `nothing checked — no commands configured or detected` and the run can be applied. A repository with no recognised build file verifies nothing unless it says otherwise in `.senate.toml`.
- A declined `package.json` reads the same on the bottom line and is equally appliable — the explanation is in `## Source`, not the verdict. That is deliberate: refusing to guess must not leave an unconfigured monorepo unable to apply. If a run there looks like it checked nothing, it did, and the artifact says which table to add.
- Output is captured as a 64 KiB tail per stream while the command runs (memory stays bounded whatever the suite prints), and the artifact then keeps the last 200 lines; `[… N bytes not captured before this tail]` and `[… N lines omitted]` markers say so. Re-run the command for the full output.
- `.senate.toml` is read from the worktree first, so a change the run makes to it is what gets verified, and from the source repository only when the worktree has no such file. Unknown keys inside `[verify]` fail the stage; other tables in the file are ignored.
- One file answers, never two merged. A worktree `.senate.toml` carrying only `[permissions]` means the source repository's `[verify]` table is not consulted — the worktree's file is the repository's current word on every table, including the ones it leaves out. Put both tables in the same file.
- In Standard/Deep a failed verification does not fail the run and does not skip the decision (the edge into the decision is optional). What it blocks is `apply`; `pr` still publishes, and the way forward is `fix`, `pr` or `discard`. `retry <run-id> verify` is refused there (the run is `Completed`); it works only in Fast, where the failed verify is a leaf and the run is `Failed`.
- Only the latest verify stage counts for the gate; `verify` may stay failed forever once `verify_1` passed. The old artifact remains as history.
- A program that is not installed (`npm`, `pytest`) is a failed stage, not an infrastructure error; the bottom line says `could not start`.
- The verifier is never in `status`'s Routing table and never in a snapshot; a run sealed before the stage existed still loads and can still grow a fix cycle.
