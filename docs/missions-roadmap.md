# Missions: from run orchestrator to engineering command center

This document records the architecture assessment made before the first
mission milestone and the milestone sequence that follows from it. It is a
plan, not a description of what exists; `ARCHITECTURE.md` and
`docs/features/missions.md` say what is built.

## Product direction

Today the human is the clipboard between agents: one model plans, another
implements, a third reviews, and the user copies text between them and holds
the project state in their head. The Senate should own that coordination:

```text
User
  ↓
Mission (goal, plan, decisions, attention)
  ↓
Lead
  ├── Work package A → child run (isolated worktree)
  ├── Work package B → child run
  ├── independent review of A / B     (existing review stages)
  └── controlled integration          (existing apply / pr)
         ↓
      verification                    (existing verify stage)
         ↓
       decision / PR
```

The user talks to the mission and its lead, not to individual agents. The
test for every change: does it reduce the project state the human has to
hold in their head, and does it let them direct development rather than
watch agents work?

## Architecture assessment

### What already exists and maps directly

| Need | Existing primitive | Reuse decision |
|---|---|---|
| Bounded engineering operation with durable state | `Run` aggregate, `RunInput`, config snapshot, SQLite CAS + events | A work package is delivered by an ordinary run. No parallel execution model. |
| Isolated writable checkout per worker | `WorkspaceManager`, `~/.senate/worktrees/<repo>/<run-id>` | Every package run gets its own worktree for free. Missions add no Git state. |
| Independent review of real repository state | CodeQualityReviewer / SpecReviewer stages, change handoff | Reviews stay inside the child run's workflow (Standard/Deep). |
| Deterministic verification | `Verify` stage, `[verify]` in `.senate.toml`, apply gate | Package delivery inherits the run's verification evidence. |
| Explicit integration | `apply` (patch transfer, clean checkout), `pr`, `rebase` | Package integration is recorded on top of these; nothing new writes the checkout. |
| Roles separate from providers/models | `Role` → `RoutingPlan` → `ExecutionTarget` → adapter | Packages choose a workflow, never a provider; routing stays per run. |
| Restart-safe persistence pattern | versioned snapshot → rehydration data → validated aggregate, insert-only inputs, per-aggregate event log | Copied for missions one-to-one (`missions`, `mission_inputs`, `mission_events`). |
| Read-side bounded settle | `ABANDONED_AFTER` observe pass in `inspect_run` | Missions observe committed run status on every read; they never drive a run. |
| Section-quoting UX (`## Bottom line`, `## Pull request`) | `tui::section`, stage prompt contracts | Later briefs quote artifacts the same way instead of composing claims. |
| Fix / continue cycles | `request_fix`, `request_continue` | Package remediation reuses these on the child run rather than a new mechanism. |

### What was missing

- A durable object above the run: goal, plan, packages, dependencies,
  decisions, attention. Nothing persisted linked two runs to one purpose.
- A structured delegation contract. The only way to hand work to another
  agent was to write a task string by hand.
- A run-to-purpose index: given a run, nothing said what it was for.
- Any notion of "integrated": a run being `Applied` is per run; a plan
  needs to know which of its parts are in the checkout so the next part
  can start on top of them.

### What must not be duplicated

- Run lifecycle, stage graph, scheduler, provider adapters, worktree
  lifecycle, apply, verification. Missions compose these; they do not
  reimplement any of them, and no mission code touches Git.
- Attention. Run-level attention (permission, decision, question) stays on
  the run. Mission attention is derived from package state on every read
  and is never stored, so it cannot disagree with the packages.
- Routing. A package names a workflow; routing is resolved per child run
  exactly as today (`--provider`, `--profile recommended`).

### Invariants and risks

- **Evidence over claims.** Package state moves only on committed run
  status (`NeedsUser`, `Completed`, `Applied`, `Failed`, `Discarded`) and
  integration needs an `Applied` run or a completed run with an empty
  delta. No agent report changes a package.
- **Contracts freeze when work starts.** A package nothing has run for may
  be revised; once a run serves it, the contract is history and a change
  is a new package or a fix cycle on the run.
- **Readiness means integrated dependencies.** A package becomes `Ready`
  only when every dependency is `Integrated`, so its child run's base
  commit (the checkout's `HEAD`) carries their changes. This is the honest
  rule while integration is the operator's `apply`; M6 may relax it with a
  mission integration branch.
- **Runs bound to a mission cannot be purged**: the mission's history is the
  reason the run existed (`ON DELETE RESTRICT` plus a typed store error).
- **Risk: two writers on the source checkout.** The Senate dogfoods itself;
  concurrent packages must never share one checkout. M6 concurrency will
  integrate through worktrees and branches, never by parallel applies.
- **Risk: a god object.** `MissionService` is use-case ordering only; rules
  live in the `Mission` aggregate, persistence in `store::mission`, and the
  lead (M3) will be a run-like session that proposes changes the service
  applies, not a process that owns state.

## Milestones

### M1 — Mission foundation (this branch)

- `Mission` aggregate with `WorkPackage`s, contracts, dependencies,
  decisions, derived attention, full invariant validation on rehydration.
- Schema v10: `missions`, `mission_inputs`, `mission_events`, `mission_runs`.
- `MissionService`: create, list, inspect, add/revise/depends, start a
  package as a child run whose immutable input is the rendered handoff,
  attach an existing run, integrate on run evidence, retry, cancel,
  decide, complete, cancel.
- CLI `senate mission ...`; no TUI screen yet.
- Tests: aggregate rules, restart rehydration, CAS and event integrity,
  purge refusal, end-to-end on the fake provider.

### M2 — Handoff and result model (this branch)

- Done: the handoff persisted as an immutable record beside the child run
  (`mission_handoffs`: contract hash, task hash and size, dependencies and
  decisions named), committed with the bind.
- Done: `WorkPackageResult` captured at delivery from the run store —
  changed files, verify/review/decision statuses, bottom lines and
  follow-ups quoted verbatim — kept in the mission snapshot (v2).
- Done: rework through the run's own fix/continue cycles (`mission fix`,
  `mission continue`), including cycles started outside the mission.
- Done: fan-in with `mission resume`; fan-out is starting several ready
  packages on native providers, whose runs return at "waiting for
  provider".
- Deferred to M6: injecting dependency artifacts (plan, decision) into a
  handoff, and a mission-level concurrency policy.

### M3 — Lead conversation and plan changes (this branch: lead session, `## Plan changes`, `mission ask`/`apply`; attention classes deferred)

- Mission-scoped lead session: a run-like, restart-safe conversation
  (provider session + retained output) whose prompt is the canonical
  mission state, not a chat log.
- Structured proposals: the lead answers with a `## Plan changes` section
  in a fixed shape (add/revise/reorder/cancel package, record decision);
  The Senate parses it into `MissionService` calls and shows the diff of the
  plan before applying material changes.
- Attention classes: informational (invisible), routine (lead handles),
  decision (user confirms), blocker (prominent). Routine transitions stay
  automatic.

### M4 — Calm command center (first slice on this branch: missions screen, start, integrate, open run)

- Missions screen in the TUI: goal, health, progress, current focus,
  attention; packages as engineering state, runs one level down, logs and
  diffs two levels down (existing `i` technical-details philosophy).
- Chat with the lead from the mission screen; plan-change confirmation
  overlay.
- Nothing shows per-file agent activity by default.

### M5 — Progress brief

- Structured brief model built from canonical facts (integrated packages,
  changed files, verification, review outcome, decisions, unfinished
  packages) with the lead's plain-language narrative attached per
  section and marked as narrative.
- Deterministic local HTML renderer under
  `~/.senate/missions/<mission-id>/briefs/<brief-id>/index.html`;
  technical section collapsed; optional visual evidence slots.
- Generated on request (`Open recap`), on milestone completion, and on
  decision requests; never per event.

### M6 — Richer orchestration

- Mission integration branch and worktree so concurrent packages land
  without touching the operator's checkout until the mission is applied.
- Worker/reviewer loops with bounded automatic remediation.
- Provider capability model (`invoke`, `continue`, `interrupt`, images,
  writable repo) as a trait the existing adapters implement; an
  `external/manual` adapter that exports a handoff and imports a result.
- Capability- and evidence-informed routing on top of the eval suites.
