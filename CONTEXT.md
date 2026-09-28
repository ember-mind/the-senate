# The Senate Orchestration

Agents should read `docs/features/README.md` before driving or changing a feature.

The Senate coordinates recoverable engineering work performed by native coding-agent providers while preserving explicit human control over run progress and results.

## Language

**Run**:
A recoverable orchestration effort bound for its lifetime to one immutable input and one effective configuration snapshot.
_Avoid_: Job, execution

**Run input**:
Immutable normalized user intent for one run, stored separately from mutable lifecycle state and effective configuration. Leading/trailing whitespace is removed; internal Unicode and line breaks are preserved.
_Avoid_: Task field in Run, provider prompt, mutable description

**Stage**:
A resumable unit of engineering work within a run, with declared dependencies and one terminal outcome.
_Avoid_: Step, phase

**Run status**:
The run's current lifecycle condition, including a ready boundary after preparation and before execution. Cleanup is not a run status.
_Avoid_: Run state

**Stage status**:
The stage's current lifecycle condition. Ready means dependencies were checked and work is eligible to start.
_Avoid_: Stage state

**Execution-finished**:
A lifecycle condition with no active work. Failed work may still be retried, and a completed run may still be applied or discarded.
_Avoid_: Terminal

**Lifecycle-closed**:
A condition that rejects normal future lifecycle mutation: applied or discarded for a run, completed or skipped for a stage.
_Avoid_: Finished, terminal

**Ready**:
An atomic lifecycle boundary where preparation or dependency validation succeeded, but execution has not started.
_Avoid_: Waiting, pending

**Paused**:
A deliberate suspension requested by the user and eligible to resume from the condition that was suspended.
_Avoid_: Stopped, interrupted

**Interrupted**:
An unplanned suspension caused by loss of the active process or runtime, eligible for recovery from the condition that was interrupted.
_Avoid_: Paused, failed

**Resume**:
Continue deliberately paused work from its saved condition.
_Avoid_: Recover, retry

**Recover**:
Continue unexpectedly interrupted work from its saved condition.
_Avoid_: Resume, retry

**Attention request**:
A persisted, uniquely identified request for human permission, a decision, or an answer before affected work can continue. Multiple pending requests preserve creation order across pause or interruption.
_Avoid_: Needs-you signal, prompt

**Retry**:
An explicit decision that returns a failed stage to pending. Retry remains available while every downstream stage is pending or ready; recovery never silently retries failed work.
_Avoid_: Resume, recover

**Route override**:
An operator's explicit choice, made when retrying one failed stage, of which provider (and optionally model) runs that stage from then on. It lives on the stage, never in the configuration snapshot, and never spreads to other stages.
_Avoid_: Reroute, fallback, provider switch

**Required dependency**:
A predecessor whose successful completion is necessary before a stage is ready.
_Avoid_: Hard dependency

**Optional dependency**:
A predecessor that must reach an outcome before a stage is ready, but whose unsuccessful outcome permits degraded progress.
_Avoid_: Soft dependency

**Degraded readiness**:
Readiness achieved after one or more optional dependencies ended unsuccessfully, preserving that reduced-evidence condition.
_Avoid_: Partial success

**Configuration snapshot**:
The immutable effective configuration selected when a run is created and reused for every recovery or resume of that run.
_Avoid_: Current configuration, config copy

**Execution target**:
One immutable provider plus optional configured model selected for an engineering role. A missing model means that provider's native configured/default model, not a model name resolved today.
_Avoid_: Provider alone, confirmed model, current default model

**Routing plan**:
The validated immutable role-to-execution-target map inside a configuration snapshot. Routes are resolved when a run is created and loaded by pure lookup on restart.
_Avoid_: Workflow provider choice, mutable route table, runtime resolver

**Uniform routing**:
A routing plan created from explicit `--provider`, assigning every distinct role used by one workflow to the same execution target.
_Avoid_: Single-provider engine, routing bypass

**Recommended profile**:
A versioned source-controlled creation-time policy that probes native provider availability, resolves explicit role routes, and persists them. Provider loss after creation never re-routes existing work.
_Avoid_: Live recommendation, runtime fallback, benchmark claim

**Application service**:
The use-case boundary coordinating repository discovery, atomic creation, workspace lifecycle, provider reconstruction, scheduler commands, and committed query results.
_Avoid_: CLI command handler, domain aggregate

**Control room**:
An ephemeral local projection and command surface over durable run state and application use cases. It never owns orchestration state, execution, provider output consumption, or workspace mutation.
_Avoid_: Execution engine, canonical dashboard, database client

**Frontend detach**:
Ending one local client connection to a run without interrupting its managed provider, changing lifecycle state, or disposing workspace changes.
_Avoid_: Stop, cancel, discard

**Evaluation suite**:
A human-versioned, source-controlled collection of role-specific cases whose meaning remains stable across result sets.
_Avoid_: Generic benchmark, live leaderboard

**Evaluation case**:
One stable scenario pairing an engineering role with disposable repository evidence and an independent deterministic oracle.
_Avoid_: Production run, prompt sample

**Evaluation target**:
One candidate provider plus optional configured model measured for exactly one role while support roles remain synthetic.
_Avoid_: Role route, Recommended profile, winner

**Evaluation result**:
Versioned evidence from one case repetition, preserving candidate identity, fixture identity, role metrics, usage, latency, and failure classification outside production run history.
_Avoid_: Routing policy, benchmark row in Run

**Benchmark failure**:
A measured candidate outcome that violates a case criterion, such as failed behavior, scope drift, missed finding, or false positive.
_Avoid_: Provider outage, harness failure

**Evaluation infrastructure failure**:
Failure of fixture, provider availability, supervision, protocol, artifact integrity, apply, or safety boundary that prevents trustworthy candidate scoring.
_Avoid_: Model failure, zero score

**Quiescence**:
A durable condition where synchronous execution has no immediate legal work: completed, awaiting attention, paused, interrupted, failed, applied, discarded, or provider-delayed.
_Avoid_: Every stop is an error, busy polling

**Run snapshot**:
A versioned persisted representation of current run state, accepted into the domain only after migration and full invariant validation.
_Avoid_: Serialized run, event replay

**Run workspace**:
The separately persisted physical Git resource assigned to one run: source repository identity, immutable base commit, managed worktree, and optional owned branch. It is infrastructure state, not part of the run snapshot.
_Avoid_: Repository fields in Run, execution directory

**Workspace mode**:
The explicit Git shape of a run workspace: branch-backed for implementation work or detached for review-only work.
_Avoid_: Mode inferred from branch text

**Workspace status**:
The recoverable lifecycle of the physical Git resource: preparing, ready, removing, removed, or broken. It does not determine run status.
_Avoid_: Run status, filesystem existence alone

**Workspace intent**:
A durable declaration of an intended Git resource change recorded before its non-transactional filesystem effect.
_Avoid_: Best-effort metadata, open database transaction around Git

**Workspace reconciliation**:
Validation of persisted workspace intent against current Git/filesystem evidence, followed only by a safe idempotent completion or an explicit broken outcome.
_Avoid_: Blind retry, filesystem inference

**Base commit**:
The immutable commit from which a run workspace was created and against which its apply delta is generated.
_Avoid_: Current source HEAD, mutable branch tip

**Rehydration data**:
Persistence-neutral, untrusted current-state input consumed by `Run::rehydrate`; it is not a valid run until all workflow, lifecycle, ownership, attention, suspension, and timeline invariants pass.
_Avoid_: Deserialized run, trusted snapshot

**Run revision**:
A per-run compare-and-swap counter changed by every committed state mutation. A stale expected revision means concurrent modification, not retryable success.
_Avoid_: Event sequence, schema version

**Atomic run commit**:
One SQLite transaction containing a run snapshot/revision update and its complete semantic-event batch, so neither side can become durable alone.
_Avoid_: Save then log, eventual event write

**Event sequence**:
A per-run ordinal defining authoritative semantic-event order independently from wall-clock timestamps.
_Avoid_: Timestamp order, global sequence

**Workflow**:
An opinionated identity plus a validated dependency graph describing which stages belong to a run.
_Avoid_: Pipeline script, custom graph

**Workflow scheduler**:
The execution service that repeatedly evaluates a workflow graph and advances eligible stages without workflow-specific procedures.
_Avoid_: Pipeline runner, stage script

**Provider signal**:
One provider-neutral report accepted from a provider while a stage executes, such as progress, usage, human attention, suspension, interruption, failure, or completion.
_Avoid_: Log line, provider callback

**Provider signal batch**:
An ordered group of provider signals that arise from one indivisible native record and must become durable together.
_Avoid_: In-memory follow-up, repeated raw-record acknowledgement

**Provider checkpoint**:
A durable acceptance marker proving one raw provider record was consumed, with or without semantic events, from which processing continues without silent replay.
_Avoid_: Semantic event required, in-memory cursor, artifact presence

**Provider session**:
One logical provider conversation for one stage attempt, with opaque native identity independent from any individual process invocation.
_Avoid_: Managed process, backend session, stage

**Provider session status**:
Current conversation/infrastructure condition: created, starting, active, awaiting user, completed, failed, or interrupted. It does not replace stage status.
_Avoid_: Stage status, process status

**Provider invocation**:
One managed CLI process participating in a provider session. Permission resolution or recovery may create another invocation without creating another provider session.
_Avoid_: Attempt, provider session, retry

**Native session identity**:
Opaque provider-issued conversation identity used only by its adapter to continue same logical provider session.
_Avoid_: Managed process ID, backend session ID, fabricated provider checkpoint

**Semantic provider commit**:
One atomic acceptance boundary joining run state/events, provider-session revision, artifact metadata when produced, and exact raw-output acknowledgement.
_Avoid_: Parse then save, independent cursor update

**Provider artifact**:
Human-readable stage output whose immutable metadata and content identity are persisted; downstream stages consume only declared dependency artifacts.
_Avoid_: Completion sentinel, raw provider log, canonical run state

**Permission continuation**:
Explicit human resolution that continues same provider session with only safely representable native permission scope. Broad or ambiguous approval fails closed.
_Avoid_: Global bypass, new attempt, generic resume

**Native execution policy**:
Provider-run sandbox and approval constraints selected from stage semantics while preserving native authentication, configuration, and repository instructions.
_Avoid_: Workspace mode, provider fallback, disabled sandbox

**Managed process**:
A separately persisted infrastructure attempt that supervises one exact external command for one run stage without becoming run or stage state.
_Avoid_: Provider session, stage execution, child handle

**Managed process status**:
The recoverable infrastructure condition of one external-command attempt. It does not determine semantic stage success or failure.
_Avoid_: Stage status, provider outcome

**Process backend**:
A provider-independent supervisor boundary for starting, inspecting, interrupting, reading, and cleaning managed external processes.
_Avoid_: Provider adapter, scheduler, tmux API in provider request

**Backend session**:
An opaque supervisor resource identity whose ownership must be proven before reuse, interruption, or cleanup.
_Avoid_: Provider session, tmux name as domain identity

**Command fingerprint**:
An immutable SHA-256 identity of one managed launch specification and its run/stage/attempt binding, used for recovery evidence rather than authentication.
_Avoid_: Secret hash, provider checkpoint

**Output cursor**:
A per-stream acknowledged byte offset advanced explicitly after durable consumption; reading alone never changes it.
_Avoid_: File position, line number, provider checkpoint

**Exit evidence**:
An identity-bound durable record written when the supervised command ends, distinct from absence of its backend session.
_Avoid_: Sentinel completion, tmux disappearance, stage outcome

**Process reconciliation**:
Validation of persisted process intent against owned backend session, output, runtime, and exit evidence, producing a safe current infrastructure status.
_Avoid_: Blind relaunch, session-name inference

**Role**:
An engineering responsibility assigned to a stage, independent from provider and model selection.
_Avoid_: Agent, model

**Code Quality Review**:
Independent read-only assessment of HOW an implementation is engineered: simplicity, readability, maintainability, tests, errors, unnecessary abstraction, and implementation-level regressions.
_Avoid_: Specification review, generic review

**Specification Review**:
Independent read-only assessment of WHAT behavior an implementation delivers against immutable user intent and available design evidence, focusing on Missing, Wrong, and Unrequested behavior.
_Avoid_: Code quality review, test-passing check, generic review

**Legacy review**:
General review responsibility retained as historical meaning for runs created before reviewer specialization.
_Avoid_: Code quality review, specification review

**Stage kind**:
The semantic work performed by a stage, independent from the role responsible for it.
_Avoid_: Role, provider

**Provider**:
An extensible adapter identity for a native coding-agent CLI and its native session behavior.
_Avoid_: Model, role, vendor API

**Model**:
A provider-scoped identity. Configured target may omit it to request native default; actual session model remains separately provider-confirmed.
_Avoid_: Role, provider

**Apply**:
The explicit act of transferring a completed implementation run's changes back to its source checkout.
_Avoid_: Merge, commit

**Apply operation**:
A persisted recoverable intent identifying one exact patch transfer and whether its Git effect is prepared, present in the source, recorded, or failed.
_Avoid_: Generic job, implicit retry

**Patch hash**:
A SHA-256 identity of exact apply bytes used to detect changed workspace output across crash recovery; it is evidence of operation identity, not authentication.
_Avoid_: Content signature, run revision

**Branch ownership**:
Persisted and revalidated evidence that a managed branch was created for one run and may be deleted only while its expected tip still matches.
_Avoid_: Prefix-only deletion permission

**Discard**:
The explicit terminal disposition that abandons a run's working changes while retaining its recorded history.
_Avoid_: Delete, cleanup

**Cleanup**:
An artifact-retention operation that may remove run resources without changing the run's lifecycle status.
_Avoid_: Cleaned status, discard

**Verify**:
The stage that runs the repository's own verification commands in the run's worktree after the last editing stage, records every command and exit code in an artifact, and completes only when every exit code is zero. Its failure reaches the decision as evidence and blocks apply and pull request until a later verification passes; it does not fail the run.
_Avoid_: Test stage, CI, check

**Verifier**:
The role every Verify stage carries, always served by the deterministic `verify` provider through an implicit route that no configuration snapshot names.
_Avoid_: Tester, validator, agent

**Mission**:
A durable project goal above the run, made of work packages with dependencies between them, the decisions the plan rests on, and the child runs that deliver each package. Persisted and restart-safe like a run; its lifecycle is Planning, Active, Completed, Cancelled.
_Avoid_: Project, epic, swarm, session

**Work package**:
One engineering contract inside a mission — title, goal, why it exists, scope, acceptance criteria, verification expectations, delivering workflow — with a lifecycle of its own and at most one current child run. Its contract freezes once a run serves it.
_Avoid_: Task, ticket, prompt, subtask

**Handoff**:
The child run's immutable input rendered from canonical mission state: mission goal, package contract, integrated dependencies, and recorded decisions. Never typed by hand and never composed by an agent.
_Avoid_: Prompt, brief, delegation message

**Package readiness**:
A package is ready when every package it depends on is integrated, so the run that serves it starts on a checkout that already carries their changes.
_Avoid_: Unblocked, schedulable

**Delivered**:
The package's current run completed. Its change is in the run's worktree, not yet in the source checkout. The control room says `done, bring it in` for it; that is the operator's phrase, not a status name.
_Avoid_: Done (in code and docs), merged

**Integrated**:
The delivered package's change reached the source checkout, on run evidence: the run is applied, or it completed with an empty delta. `mission integrate` and the control room's `I` apply the run first when its change is still in the worktree. Dependents may become ready. The control room says `in`.
_Avoid_: Merged, accepted, applied package

**Lead session**:
The one run a mission talks through (`WorkflowKind::Lead`): every message from the user is a `lead_N` stage whose instruction is the mission brief plus the message, and every answer is that stage's artifact.
_Avoid_: Chat, thread, conversation log

**Mission brief**:
The mission's canonical state rendered as text for the lead: goal, packages with state and evidence, decisions, what needs the user. Rendered fresh for every turn; never accumulated.
_Avoid_: Context, history, transcript

**Plan change**:
One proposed operation on the plan (add, revise, cancel a package; record a decision) parsed from the lead answer's `## Plan changes` section. Proposed until `mission apply` lands it; a batch lands whole or not at all.
_Avoid_: Suggestion, edit, patch

**Mission decision**:
An insert-only record of a design choice the plan rests on, with its rationale and author (user or lead). A reversed decision is a new decision.
_Avoid_: Note, comment, plan change

**Mission attention**:
What the user must look at, derived on every read from package state: blocked packages (their run needs the user), failed packages, and delivered packages awaiting integration. Never stored.
_Avoid_: Notification, alert, inbox

**Handoff record**:
The insert-only row binding a package's child run to the mission state its task was rendered from: contract hash, task hash and size, named dependencies and decisions. Evidence of what the worker was told; absent for a run attached by hand.
_Avoid_: Prompt log, transcript

**Package result**:
Evidence captured from the run store when a package's run first finishes and kept with the package: changed files, verify, review and decision statuses, and the artifacts' own bottom lines and follow-ups quoted verbatim.
_Avoid_: Summary, report, agent claim

**Rework**:
A delivered package going back to work on its own run through a fix or continue cycle. It is in progress again and delivers afresh; its previous result is replaced, never edited.
_Avoid_: Retry (that is a new run), revision
