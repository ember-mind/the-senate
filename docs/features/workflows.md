# Workflows

Choose how much process a task gets: one implementation stage, a full architecture-to-decision pipeline, or a read-only review, and then extend a finished run with fix or continue cycles.

## Sub-features
- fast: Implementation -> Verify.
- standard: Architecture -> Implementation -> Simplification -> Code Quality Review + Specification Review + Verify -> Decision.
- deep: Research before Architecture, then the Standard graph.
- review: Research -> Code Quality Review + Specification Review (optional edges) -> Synthesis -> Decision; detached, read-only workspace; no Verify.
- verify: the repository's own checks run after the last editing stage and beside the reviews; the Decision waits for them through an optional edge, so a failed check reaches the lead and the run still completes (see verification.md).
- fix: append a Fix stage, its Verify (`verify_<n>`) and a fresh Decision to a `Completed` run that reached a decision.
- continue (TUI only): append a FollowUp stage, its Verify (`followup_verify_<n>`) and a Decision with a free-text instruction (`c`), or with the decision's own `## Follow-ups` section (`w`).
- reviewer-specialization: Code Quality Review judges HOW; Specification Review judges WHAT (Missing/Wrong/Unrequested).
- architecture-plan: the Architect artifact carries `## Plan` (numbered, one step per file), `## Verification` (exact commands), `## Out of scope` and `## Assumptions` (facts checkable in one place). The Implementer confirms the assumptions before editing and records any that fail under `## Plan deviations`; this is what lets the implementer run at a lower effort than the planner (see observability-and-effort.md).
- change-handoff: review stages get a bounded diff and changed-file inventory of the worktree vs base commit in their first prompt.
- bottom-line: every stage prompt asks for a `## Bottom line` section; the TUI quotes it verbatim.
- pull-request: every editing stage prompt (Implementation, Simplification, Fix, FollowUp) asks for a closing `## Pull request` section — title line, then a description with Proposed changes / Why / Testing; `pr` quotes it (see workspace.md).

## How to get to it (user POV)
Pick the workflow by the subcommand name. Fix is offered on any completed run that reached a decision; The Senate never reads the verdict to decide whether a fix is warranted. Continue and follow-ups exist only in the TUI. In the TUI composer (`n`), the Workflow field cycles Fast/Standard/Deep/Review with ←/→.

## Driving it
```bash
senate fast "<task>"
senate standard "<task>"
senate deep "<task>"
senate review "<task>"
senate fix <run-id>
```
TUI run detail: `f` next cycle — on a finished run a picker of Fix / Continue / Follow-ups (↑/↓ + Enter, or the cycle's own key; Follow-ups only when the decision wrote that section; skipped when only one applies), on a working run books a fix (press again to cancel the booking); `c` continue with a typed instruction (Enter submits, Esc cancels), `w` work on the decision's Follow-ups (↑/↓ toggles "in this run" / "as a new run", Enter confirms).

## Where it lives
- `src/domain/workflow.rs` — `WorkflowKind`, `StageKind` (incl. `Fix`, `FollowUp`, `Verify`), built-in DAGs, `fix_cycle_stages`, `continue_cycle_stages`, `next_follow_up_stage_id`, `requires_writable_workspace`, `without_verification`.
- `src/domain/role.rs` — roles (Researcher, Architect, Implementer, Simplifier, CodeQualityReviewer, SpecReviewer, EngineeringLead, Verifier, legacy Reviewer).
- `src/engine/scheduler.rs` — graph-driven advancement; one eligible stage at a time.
- `src/app/run_service.rs` — `request_fix`, `request_continue`.
- `src/providers/stage_prompt.rs` — provider-neutral role contracts, `BOTTOM_LINE`, `PULL_REQUEST`.
- `src/domain/workflow.rs` `StageKind::edits_workspace` — the one predicate for which stage kinds edit the worktree (writable workspace, Claude read-only guard, Codex edit framing, PR contract).
- `src/providers/change_handoff.rs` — bounded review-stage diff (1 MiB, 200 files).
- `src/providers/continue_instruction.rs` — run-private file for the continue instruction.
- `src/tui/follow_ups.rs`, `src/tui/bottom_line.rs`, `src/providers/section.rs` — Markdown section extraction.
- `tests/codex_cli.rs` — `a_rejected_run_is_fixed_in_place_and_the_source_is_untouched_until_apply`, stage sandboxes per kind.

## Gotchas
- There is no `continue` CLI command; continue and follow-ups are TUI-only (`c`, `w`).
- Fix and continue require `RunStatus::Completed` and a Decision stage in the graph; `fast` runs have no decision, so `f`/`c`/`w` are refused with an explanation.
- Fix cycles never re-run the reviews; start a `review` run over the result if you want them back. They do re-run verification (`verify_<n>`), and the fresh decision only optionally depends on it.
- A failed Verify stage does not fail the run: the decision runs with the failure in evidence, the run completes, and `fix`/continue stay available. Only `apply` is refused until the latest verification passes; `pr` publishes regardless.
- A run created before fix-cycle routing existed cannot execute a fix; `request_fix` checks this before committing anything.
- Existing persisted runs keep their original stored graph, including legacy generic Review stages; only new runs use the current definitions.
- The change handoff is derived evidence, not an artifact; oversized diffs are marked INCOMPLETE, never silently cut. Resume prompts do not re-inject it.
- Review workflows use a detached worktree; `apply` and `pr` reject them.
