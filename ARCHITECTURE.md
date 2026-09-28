# The Senate Architecture

## Status

Current work is Milestone 13d3 (TUI visual polish). On top of Milestone 11's local role-specific evaluation and `recommended_v2` routing evidence, the code carries M13a resource observability (usage, latency, invocation count, injected prompt bytes), M13a.5 change handoff for review stages, M13b effort policy (`--effort`, per-role `ResourcePlan`, config schema v3, `recommended_v3` effort column with `xhigh`), M13e self-update (`senate update`, install receipts), the opt-in ImageGeneration tool (config schema v4, local Codex CLI backend), a third native provider `opencode` (fans one native CLI out to any vendor its own `opencode models` lists — DeepSeek, Gemini, Kimi, Qwen, and more — explicit-only via `--provider opencode --model <provider/model>`, never a Recommended candidate), plus stop, fix/continue cycles, and pull-request publishing. Ratatui control room, frozen M9 `recommended_v1` (still decoded for persisted runs), reviewer specialization, native provider semantics, durable recovery, and explicit apply/discard remain unchanged. Native authentication/configuration remains authoritative; no vendor API is called directly — the opt-in ImageGeneration tool keeps to that rule by driving the local Codex CLI, confined to `src/image/` and never modelled as a provider, and opencode keeps to it by driving the user's own local `opencode` CLI and its native per-vendor authentication, never a vendor SDK. A dedicated first-class Gemini provider, runtime failover, custom routing DSL, LLM judging, cloud benchmark service, async runtime, native process backend, daemon mode, Advisor, and direct provider chat remain deliberately absent; Gemini itself is reachable only through an explicit opencode model id, on the same terms as every other vendor opencode fans out to.

Legacy `agents-v3.0.0` was inspected after bootstrap. [LEGACY_BEHAVIOR.md](LEGACY_BEHAVIOR.md) records its behavioral contract, recovery edge cases, and intentional architectural departures.

## Product boundary

The Senate orchestrates native coding-agent CLIs. A provider represents a local CLI and its native authentication/session behavior; it does not imply direct use of a vendor API.

These concepts remain independent:

```text
workflow stage -> engineering role -> provider -> model
```

Workflow definitions depend only on roles. Immutable execution configuration maps each used role to an `ExecutionTarget { provider_id, model_id }` without embedding provider/model names in domain or DAG semantics.

## Milestone 9 routing boundary

```text
Stage
  -> Role
  -> immutable RoutingPlan
  -> ExecutionTarget(provider + optional configured model)
  -> native Provider adapter
  -> native CLI
```

New config payload schema v2 stores profile provenance, explicit role routes, safe display reasons, and provider-native versioned options. SQLite schema and `RunSnapshot` do not change: routing belongs to existing insert-only `ConfigSnapshot` authority.

`--provider claude|codex|fake` is uniform-routing shorthand, not a scheduler bypass. `--profile recommended` resolves the current source-controlled profile (`recommended_v3`) once at run creation; snapshots persisted under `recommended_v1` or `recommended_v2` continue decoding to their original routes and native-default effort. With both native providers ready, `recommended_v3` routes Implementer and SpecReviewer to Codex while Researcher, Architect, Simplifier, CodeQualityReviewer, legacy Reviewer, and EngineeringLead route to Claude — every route inherited from `recommended_v2` — and states a requested effort per role (reasoning roles `high`, Implementer `medium`, Simplifier `low`), each row `Provisional` until an effort sweep replaces it. Implementer, CodeQualityReviewer, and SpecReviewer decisions are backed by role_core_v3 native-runtime evidence (typed provenance in `app::routing`, fingerprint pinned); the remaining roles are inherited from `recommended_v1` with no current benchmark evidence. Evidence is runtime-level (native runtimes may orchestrate models/subagents) and encodes no monetary/token-cost claims. With only one authenticated native provider, every required role routes to that provider and fallback reason is persisted. Fake is never a Recommended fallback.

Persisted routes are authoritative. Resume, retry, recovery, and attention never re-run Recommended. Provider loss/auth expiry after creation produces configured-provider-unavailable error; no runtime rerouting occurs. Schema-v1 M5-M8 configuration normalizes in memory to uniform routes for roles in persisted workflow without rewriting immutable payload.

`WorkflowEngine` builds request, asks provider boundary for actual provider ID, compares it with same-stage checkpoint, then polls. Domain events, sessions, checkpoints, and artifacts therefore record `claude`, `codex`, or `fake`; no router pseudo-provider exists. `WaitingForProvider` carries current stage's attachment policy. Attention continuation reconstructs stage/role context and verifies route matches provider session that created request.

`RoutedProvider` loads routing structure without probing installations. Leaf adapters instantiate only when target is needed and cache by full provider+configured-model target. Completed historical provider may disappear without blocking unrelated remaining target. Status separates configured target from actual provider/model/session/process for each stage.

## System boundaries

```text
CLI
    |
application service + query DTOs
    |
workflow engine ---- domain state/events ---- SQLite store
    |
provider interface
    |
process backend (tmux first, native supervisor later)
    |
native coding-agent CLI inside isolated Git worktree
```

Human-readable artifacts, JSONL logs, and conversations complement SQLite. They never replace canonical machine state.

## Milestone 10 TUI boundary

```text
                         The Senate TUI
                             |
           +-----------------+-----------------+
           |                                   |
      read-only refresh                    user action
           |                                   |
           v                                   v
       Query DTOs                       command worker
           |                                   |
           +-----------------+-----------------+
                             v
                      Application layer
                             |
            +----------------+----------------+
            v                v                v
          Store          Workspace         Engine
                                               |
                                           Provider
                                               |
                                       Managed Process
```

TUI owns ephemeral presentation only: selection, screen, scroll, modal/input state, latest read model, busy marker, and transient errors. Database/application state wins on every refresh. Rendering has no edge to SQLite, Git, tmux, provider adapters, or mutable domain aggregates.

`RunService::list_runs` and `inspect_run` remain primary projections, with one bounded write: a run reading `Running` whose managed processes have all ended and which nothing has touched for 30 seconds is observed through the engine (`ResumeAction::Observe`) before projection, so a run whose provider died settles on the next read instead of waiting for a stop. The idle grace and the active-process check keep that pass off any run another process is still driving; observation never resumes provider work, and every failure leaves persisted state untouched. M10 adds narrow UI-agnostic reads: integrity-verified artifact list/read, bounded process log tail, and bounded workspace diff preview. Artifact bytes are size/SHA-256 checked again before return. Log tail uses explicit file length and offset without cursor acknowledgement. Diff preview uses same temporary index and binary/full-index Git delta as apply, writes Git output to a temporary file, reads at most 2 MiB, and creates no apply intent or source/index mutation.

The hero panel states one line of a finished stage before its artifact is opened. That line is always quoted, never composed: every stage prompt carries one provider-neutral contract (`stage_prompt::BOTTOM_LINE`) asking the agent to open its Markdown with a `## Bottom line` section of at most two plain sentences, and the TUI extracts that section verbatim. An artifact that predates the contract or ignored it is quoted from its opening paragraph instead, styled as the excerpt it is. Extraction is presentation-only: the persisted artifact is never rewritten, no provider is called for a summary, and the panel never asserts a verdict the artifact did not assert. Publish follows the same rule for the pull request: every stage kind that edits the workspace (`StageKind::edits_workspace` — Implementation, Simplification, Fix, FollowUp) carries one closing contract (`stage_prompt::PULL_REQUEST`) asking for a `## Pull request` section, title line first, and `pr` quotes the latest complete editing artifact's section as the title and description. No provider is invoked at publish time, the task text stands in when no stage wrote the section, and the artifact is read through the same integrity-verified path as opening it. The selected stage's artifact is read through the same integrity-verified path as opening it, and only when the stage, attempt, or content size behind the quote changes, so the half-second refresh does not rehash the same file forever.

One standard-thread command worker serializes start, resume/recover, retry, attention resolution, apply, and discard through `RunService`. Frontend thread continues terminal input, rendering, and periodic refresh; that refresh reads, and settles abandoned runs only under the conditions above. No async runtime or scheduler change exists. External CLI writes remain possible; optimistic concurrency and reload stay authoritative.

Quitting or Ctrl-C detaches frontend without joining active worker. Raw mode, alternate screen, bracketed paste, and cursor state are restored by RAII; panic hook performs best-effort restoration before original panic reporting. Local worker disappears when process exits, but tmux-owned provider continues and retained output remains durable. Reopening TUI is observational; explicit resume/recovery performs existing reconciliation. TUI-mode tracing uses a sink so stderr cannot corrupt alternate screen; actionable application failures stay visible in UI.

## Milestone 11–12 evaluation boundary

```text
EvalCase
   -> materialize disposable fixture Git repository
   -> isolated The Senate database/worktree/process roots
   -> immutable eval_v1 RoutingPlan
        target role  -> candidate provider + optional model
        support role -> FakeProvider
   -> unchanged WorkflowEngine and native adapter
   -> verified artifact + bounded pre-apply diff
   -> existing explicit apply for implementation cases
   -> fixed offline validation / deterministic ground-truth scorer
   -> versioned EvalResultV1 evidence files
```

Evaluation is benchmark metadata, not production orchestration state. Recommended provenance is compiled-in typed policy; runtime routing never reads evaluation files. No `Run`, `Stage`, `RunSnapshot`, domain event, provider-session, managed-process, or production SQLite schema field changes. Every case repetition uses existing schema in separate runtime directory; ordinary run database is never opened. Final evidence remains outside `senate.db` as `result.json`, `artifact.md`, `diff.patch`, and `validation.txt`, with isolated raw runtime data retained nearby for debugging. Normal startup and Recommended resolution never scan evaluation files.

`eval_v1` is creation-only routing provenance unavailable from normal run CLI/TUI. Routing config contains one `eval_candidate` route and Fake `eval_support` routes for every other workflow role. `RoutedProvider` remains sole request-aware router; `WorkflowEngine` contains no evaluation condition. Explicit model uses existing `ExecutionTarget.model_id`, ConfigSnapshot schema v2, routed provider cache, and native command builders. Missing configured or confirmed model remains null.

`role_core_v1` embeds seven source-controlled fixtures for Implementer, CodeQualityReviewer, and SpecReviewer. Its seven-case list, fixtures, scorer behavior, and fingerprint are immutable. `role_core_v2` is side-by-side with the same conceptual IDs: implementer cases reuse calibrated behavior, reviewer quality fixtures are valid minimal Cargo repositories, and specification fixtures remove the accidental zero-quantity behavior. `role_core_v3` is the calibrated hygiene successor with a new fingerprint: implementer fixtures ignore generated `target/` and `Cargo.lock`, reviewer fixtures track canonical dependency-free locks and ignore `target/`, and quality tests cover meaningful branches without removing planted defects. V1 and V2 remain byte-for-byte source-controlled history. Architect, Researcher, and EngineeringLead are deferred rather than measured with weak proxies.

Scoring has independent deterministic oracle. Implementer measures trusted validation, changed-file scope, deletions/unexpected files, public-surface additions, and exact empty-diff plus structured `plan_mismatch` behavior. Reviewer structured blocks remain normal Markdown artifacts; V1 matcher remains unchanged. V2 quality identity excludes severity, records severity matches and under/over-classification, and classifies a second manifestation of one conceptual truth as a duplicate rather than a false positive. V2 specification truths may list multiple locations, but category stays strict; duplicate findings do not inflate recall. Clean quality/spec cases reward absence of invented defects. Prose outside structured block, including praise, is ignored. No LLM judge exists.

Candidate usage is computed from existing `ProviderUsageUpdated` events filtered by target stage; support Fake usage is excluded. Candidate latency prefers provider start-to-completion event timestamps and falls back to local execution boundary if unavailable. Provider CLI version comes from provider session discovery; configured and confirmed models stay distinct. Native evaluation requires explicit per-command `--allow-native-usage`; Fake results are marked synthetic and cannot support Recommended policy.

Results separate benchmark failure from infrastructure failure. Failed tests, scope violations, missed findings, false positives, and incorrect mismatch behavior describe candidate outcomes. Provider discovery/auth, tmux/protocol, artifact integrity, apply, fixture, and read-only safety failures remain infrastructure outcomes and are not scored as model failures. Reports group by suite version and target, expose V2/V3 duplicate/severity metrics, and never average incompatible versions. Existing result files remain readable; no production routing, scheduler, provider, database, or TUI path reads evaluation evidence.

## Mission boundary (M1)

```text
Mission (goal, packages, dependencies, decisions)      SQLite: missions / mission_inputs /
   └── WorkPackage (contract, status, runs)                    mission_events / mission_runs
          └── child Run  ── ordinary workflow, worktree, review, verify, apply
```

A mission is the durable plan several runs serve; a run stays the bounded engineering operation. The `Mission` aggregate owns its work packages exactly as `Run` owns its stages, so the invariants between packages — unique ids, acyclic dependencies, a package `Ready` only when every dependency is `Integrated`, run bound to at most one package, a `Blocked` or `Failed` package always carrying a reason, a `Completed` mission having integrated at least one package — are checked in one place, on every rehydration (`MissionSnapshotV1` → `MissionRehydrationData` → `Mission::rehydrate`) and before every commit. Persistence copies the run pattern: validated snapshot under compare-and-swap, a per-mission sequenced event log committed in the same transaction, an insert-only `mission_inputs` row for the immutable title, goal, repository and starting commit, and `mission_runs` as an indexed projection of the packages' run lists, synchronised inside the same transaction so a run traces up to its package without decoding every mission. A run a mission binds cannot be purged (`ON DELETE RESTRICT` plus `StoreError::RunBoundToMission`).

Package state follows run evidence and nothing else. `MissionService` observes the committed status of every active package's current run on every read and mutation and reports it to the aggregate (`Mission::observe_run`): `NeedsUser` blocks, `Completed`/`Applied` delivers, `Failed`/`Discarded` fails, anything else is progress; repeating an observation yields no event. Integration is recorded on `IntegrationEvidence` read from the run store — the run is `Applied`, or it completed with an empty delta (the same delta apply and the diff preview compute) — never on a report. Mission attention is derived from package state on every read and is not stored. The service never drives a run: `start_package` renders the handoff from canonical mission state (`handoff_task`: mission goal, contract, integrated dependencies, decisions) and starts an ordinary run through `RunService`, binding it to the package the moment `StartProgress::PreparingWorkspace` reports it persisted. Missions add no Git state, no routing state and no attention kind; a package chooses a workflow, and the child run resolves provider, model, effort, worktree and verification as any run does.

Two records make the delegation checkable evidence rather than remembered text (M2). The **handoff** (`mission_handoffs`, schema v11, insert-only, one per run, committed in the same transaction as the bind) holds the hash of the contract the task was rendered from, the hash and size of the rendered task exactly as the run's immutable input stores it, and the dependencies and decisions it named; a run attached by hand has none. The **result** (`WorkPackageResult`, mission snapshot v2) is captured from the run store the first time a package's run is observed finished and kept with the package, so it outlives the worktree: changed files from the same bounded delta apply and the diff preview compute, the latest verify stage, every review stage and the latest decision with their committed statuses, and the editing, review and decision artifacts' own `## Bottom line` and `## Follow-ups` sections quoted verbatim through the integrity-verified artifact path. Nothing in it is composed. Rework rides the run's own fix and continue cycles: a delivered package whose run is at work again is in progress again, and one whose run finished with more stages than its result covers is re-delivered from a fresh capture, so a cycle started outside the mission is honoured too. `resume_mission` is the fan-in: it resumes every package run left prepared, running, paused or interrupted and then observes, which is how several packages started on native providers come home.

The control room shows missions one level above runs (first slice of M4): `M` lists them, a mission opens onto its packages as engineering state, and a package's run is one Enter down, in the existing run detail. The screens read through `MissionService::list_missions` / `inspect_mission` on the same refresh cadence as the run list, so what they show is the observed, committed package state and never an agent's claim; the actions they offer (start a ready package, answer what a package's run asks, bring a finished package in, arm auto-approve for the mission's runs) go through the worker thread or the run service like every other mutation. Bringing a package in applies its run when the change is still in the worktree and then records the package, in one confirmed action: to the operator "integrate" means "bring it in", and apply already carries the verification gate. Planning is proposed by the mission's **lead** (M3) and confirmed by the user. The lead is not a process that owns state: it is one run per mission (`WorkflowKind::Lead`), each exchange one read-only `lead_N` stage whose instruction is the mission brief rendered from the read model plus the user's message, so the lead always answers over committed state and never over a transcript, and every answer is an integrity-checked artifact that survives a restart like any other. Its `## Plan changes` section is a fixed grammar parsed by `domain::plan_change` into `PlanChange`s; `MissionService::apply_plan_changes` applies a batch through the aggregate's own operations in one commit, so a refused change refuses the batch. Nothing an agent says moves a package: proposals wait for `mission apply`. The roadmap for handoff records, the lead session, the command-center TUI and the progress brief is in [docs/missions-roadmap.md](docs/missions-roadmap.md).

## State and events

Runs, stages, and attention requests use explicit typed state. Artifacts carry typed metadata; provider sessions use neutral identities; usage has a provider-neutral signal. Important changes return common semantic events. Future persistence, scheduling, UI, notification, and journals can consume those events instead of independently inferring state from files or provider-specific JSON.

Events are semantic history and integration signals, not an event-sourcing system. Domain state persisted in SQLite is authoritative; restoration does not replay full history.

### M13a resource observability boundary

Resource telemetry is observation only: no token/context/usage value influences routing, Recommended profiles, scheduling, retries, permissions, or lifecycle classification. `UsageDelta` and `ProviderUsageUpdated` carry stable `input_units`/`output_units` plus optional provider-native dimensions (`cache_read_units`, `cache_write_units`, `reasoning_output_units`) and an optional typed `native_models` per-model breakdown; `None` always means the runtime did not report a dimension and is never collapsed into zero. Old persisted event payloads decode unchanged with the additions unavailable. Claude usage is taken from the terminal result record's cumulative totals (one atomic `[Usage, terminal]` signal batch per invocation, mirroring Codex `turn.completed`); per-assistant-message usage is intentionally discarded as unreliable (repeated per content block, partial output snapshots, sidechains indistinguishable). The Claude `modelUsage` breakdown overlaps the aggregate (it spans subagent models) and is never summed into it. Codex reports no native-confirmed model; confirmed model stays unavailable. Units are provider-native and never cross-provider normalized; the comparable dimensions are wall-clock provider latency (first `ProviderStarted` to last terminal provider event, derived from committed event timestamps), persisted invocation count, and injected prompt bytes (the exact immutable stdin bytes The Senate piped per invocation, measured from the SHA-256-verified stdin file without touching prompt content). Query DTOs (`UsageSummary`, `StageExecutionEvidence`) fold committed events at read time; nothing is pre-aggregated, so replay and restart semantics are unchanged and no schema migration was needed.

### M13b effort policy boundary

`EffortSetting` (`NativeDefault | Level(Low|Medium|High|XHigh)`) is provider-neutral resource intent in the domain layer; `ResourcePlan` is the validated immutable per-role map reconstructed from the config snapshot, following the same validated-plan philosophy as `RoutingPlan` and kept strictly separate from it — routing answers destination, the resource plan answers requested effort. Effort is requested through `EffortRequest` (`ProfileDefault | Uniform | PerRole`): the profile's own per-role levels (only Recommended has any; a uniform provider stays native), one level for every role, or some roles named with the rest from the profile. Persistence: any explicit level emits config schema v3 (`resource_plan` keyed by role, string-encoded settings, exact required-role coverage enforced); every role native keeps emitting the byte-identical pre-M13b schema v2 payload, which is what `--effort native` and every `--provider` run without the flag still do. Schema v1/v2 snapshots decode to `NativeDefault` for every role — explicitly not `Medium`, because `NativeDefault` means "preserve the runtime's native configured behavior exactly" and the two meanings must never collapse. Unknown settings, missing roles, or a resource plan smuggled into v2 fail closed. Execution boundary: `RoutedProvider` resolves effort once per role from the immutable plan and keys its lazy runtime cache by `(ExecutionTarget, EffortSetting)`; adapters receive their effort at construction via `with_effort` and own the native mapping (`claude --effort <level>`; `codex -c model_reasoning_effort="<level>"`), so no adapter performs config lookups and `ProviderRequest` stays a scheduler-owned transport unchanged by effort. `NativeDefault` omits the native flags entirely. Requested effort is persisted evidence (status/TUI per stage, additive `requested_effort` in eval results with old JSON decoding as absent); applied-effort confirmation is not invented — neither native CLI reports one. Effort is deterministic immutable intent: M13a telemetry never feeds it dynamically, no escalation exists, and workflow kinds imply no effort defaults — the profile does, per role, and the operator overrides it explicitly. The Architect contract asks for an executable plan (`## Plan`, `## Verification`, `## Out of scope`, `## Assumptions`) and the Implementer confirms the assumptions before editing, so the lower implementer level rests on removed uncertainty rather than on hope. Retry-with-higher-effort is deferred to M13b.1 as an explicit persisted per-stage override, the effort twin of `StageRouteOverride`.

### ImageGeneration tool boundary

The first capability that is neither a coding provider nor a role: the Implementer may *use* image generation, it does not *become* an image role. Routing still answers who and where, `ResourcePlan` how much effort, and the new `ImageGenerationPlan` answers only what additional tool a role may call and how often. It is not a `CapabilityPlan`; it is one grant, kept narrow until a second tool proves what the general shape should be.

```text
Implementer stage (Claude or Codex, native CLI)
  --stdio MCP-->  senate __image-tool --socket S      (shim, child of the CLI, no secrets)
  --unix socket S (0600, one JSON line each way)-->  ImageToolHost inside the The Senate process
                                                       ImageToolService: role · bound · path · PNG check · atomic write · evidence
                                                       ImageGenerator: Codex CLI built-in image_gen (`codex exec --json`) | Fake
  <-- PNG lands in the managed worktree; the ordinary diff/apply/discard machinery sees it
```

No vendor API is called and no API key exists: the backend is the user's own Codex CLI under its native authentication, driven exactly like a provider invocation (read-only sandbox, no approvals, stdin request, JSON events) but outside any run/stage record, and the PNG is collected from the thread directory Codex creates (`$CODEX_HOME/generated_images/<thread>/`), never from a path the model typed. Codex is a backend here, not a provider: it performs no role, holds no route, and its image model is not exposed, so evidence records `codex/image_gen`.

Authorization is immutable run configuration. `--allow-image-generation` seals config schema v4 with an `image_generation` block (`roles`, `max_generations`); a run without it keeps the byte-identical v2/v3 payload. Schema v1–v3 decode disabled; a v4 payload without the block, a block under an older schema, an unknown or duplicate role, or a bound outside `1..=32` fails closed. Resume, retry, fix and continue reconstruct the plan from the snapshot and never from current CLI flags. The bound is four generations per run, counted from the insert-only `image_generations` table (database schema v7), so a restart cannot reset it and call N+1 is a typed tool error. Authorization is never unlimited authorization.

The boundary is structural. Backend readiness (Codex installed and authenticated) is checked at run creation (fail fast) and at host construction; the stage CLI's environment, argv, stdin, MCP config, process spec, snapshot and evidence carry nothing new. The MCP shim the CLI launches is the The Senate executable itself with a socket path; the host that answers it lives in the The Senate process and is the only thing that runs the backend. The Senate neither injects nor strips anything in the stage's environment. Both native CLIs accept a run-scoped server without touching global configuration — Claude through `--mcp-config` plus one exact `--allowedTools` rule under `dontAsk`, Codex through `-c mcp_servers.<name>.*` root overrides — so v1 is provider-neutral. The runtime cache is keyed by `(target, effort, granted)` so a reviewer routed to the Implementer's target never inherits its tool, and a granted role whose host could not be bound is a typed refusal, not a silent run without the tool.

Placement treats `output_path` as untrusted: relative, plain components, not under `.git`, lowercase `.png`, containment checked against the canonical worktree through the deepest existing ancestor (defeating symlink escapes) before the vendor is called, re-checked after parents are created, and written temp-file → fsync → hard-link so an existing project file is never replaced. Failures are typed tool errors returned to the agent (`not_authorized`, `backend_not_configured`, `limit_reached`, `invalid_argument`, `invalid_output_path`, `output_exists`, `backend_rejected`, `backend_unreachable`, `invalid_image`, `write_failed`); none touches run or stage state, and the prompt tells the agent to continue without the image. A The Senate process that exited leaves the agent running in tmux with a dead socket: calls fail `backend_unreachable` until a resumed process rebinds the deterministic per-run path and re-arms the stage on its next poll.

Evidence answers who, when, which stage, which backend and model, where, and what bytes: one row per image with stage, attempt, ordinal, backend, model, worktree-relative path, SHA-256, size, prompt hash, request id and timestamps, plus a run-private prompt file beside the process logs. The PNG itself has no parallel artifact lifecycle: it is an untracked binary in the managed worktree, listed as binary by the preview, named by path in the review handoff, moved by apply with exact bytes, and removed by discard. Nothing has inspected its pixels; reviewers know an image changed, not what it shows, and no surface claims otherwise.

### M13a.5 change handoff boundary

Stages whose responsibility is judging the implementation (CodeQualityReviewer, SpecReviewer, legacy Reviewer) receive a deterministic implementation-change map in their initial prompt; no other stage does. Researcher and Architect precede implementation; the Implementer authored the change; EngineeringLead/Decision already receives both review artifacts as direct dependency evidence, so re-injecting the diff would duplicate context. The handoff is derived at initial-invocation compose time from the persisted run base commit and the managed worktree via the same ephemeral-index delta used by `apply` and `preview_run_diff` (tracked, untracked, and deleted paths all included), which guarantees the reviewer sees exactly the change set apply would later move — and restart determinism, since nothing depends on the current source branch, HEAD, or in-memory prompt state. The rendered section is produced by one shared provider-neutral renderer embedded verbatim by every adapter; resume/continuation prompts never re-inject it because continuation rides provider-native session state. Binary changes are identified by path and never injected as content (`--numstat` markers; the diff is generated without `--binary`). Bounds are explicit: a 1 MiB diff cap (aligned with the per-block dependency-artifact injection cap) and a 200-entry file listing cap; exceeding either yields explicit bounded partial evidence with an INCOMPLETE completeness marker rather than silent truncation, because reviewers keep the worktree as source of truth — silent-loss semantics are reserved for immutable artifact data, which continues to fail closed. The handoff is not a stage artifact: it is deterministic derived evidence, never persisted as authored output. Review independence is untouched — both reviewers receive the same factual evidence and neither consumes the other's findings. role_core_v3 suite files and scoring are unchanged (fingerprint identical), but reviewer candidates now run with richer harness context: treat M13a.5 as runtime-harness evolution when comparing pre- and post-M13a.5 native eval result sets; they are not perfectly comparable.

### Milestone 2 persistence boundary

Implemented flow:

```text
SQLite snapshot JSON
    -> inspect schema_version
    -> decode RunSnapshotV1 or RunSnapshotV2
    -> migrate/normalize to RunRehydrationData
    -> Run::rehydrate
    -> full current-state invariant validation
    -> Run
```

`Run` fields remain private and `Run` does not implement `Deserialize`. `RunRehydrationData` is persistence-neutral, deliberately constructible as untrusted input, and produces no aggregate until validation succeeds.

Implemented persistence rules:

- Persistence deserializes a versioned `RunSnapshot`, migrates and normalizes it to the latest shape, then calls validated `Run::rehydrate`. Rehydration reconstructs current state without replaying every transition, but must enforce every current-state invariant.
- Immutable resolved configuration lives in a separate insert-only record keyed by `config_snapshot_id`, with schema version, JSON payload, content hash, and creation time. Exported runs inline that payload for portability.
- Each state mutation and its complete semantic-event batch commit in one SQLite transaction. Neither state nor events may commit alone.
- Events receive a per-run sequence number as authoritative ordering. UTC timestamps remain human/debugging chronology and may be equal; persisted chronology must be non-decreasing.
- State remains canonical. Event history must explain committed state, but restoration does not require full event replay.

SQLite schema v1 uses three tables:

```text
config_snapshots(id, schema_version, payload_json, content_hash, created_at)

runs(id, status, workflow, config_snapshot_id,
     snapshot_schema_version, snapshot_json, revision,
     created_at, updated_at)

events(run_id, sequence, event_id, event_type,
       payload_json, occurred_at, recorded_at)
```

Snapshot JSON holds aggregate reconstruction state; selected run columns are indexed projections and checked against decoded state on load. `events` has primary key `(run_id, sequence)` and globally unique `event_id`. Foreign keys are enabled on every connection. File-backed stores use WAL, normal synchronous mode, and a five-second busy timeout.

Schema v2 adds infrastructure records:

```text
run_workspaces(run_id, source_repo_path, git_common_dir, base_commit,
               worktree_path, branch_name, mode, status, branch_owned,
               removal_head, last_error, revision, created_at, updated_at)

run_apply_operations(run_id, status, patch_hash, run_revision,
                     last_error, revision, created_at, updated_at)
```

`RunSnapshot` remains logical orchestration state. `RunWorkspace` is a one-to-one physical resource record; it can be broken by external Git/filesystem changes without making domain rehydration depend on path existence. Existing v1 databases migrate forward without rewriting snapshots, events, or configuration.

Schema v3 adds immutable user intent:

```text
run_inputs(run_id, schema_version, task, created_at)
```

`RunInput` owns normalized task text outside `Run`, configuration, workspace, and events. New-run transaction inserts `RunInput`, configuration, `RunSnapshotV2`, and initial event atomically. Database triggers reject input update/delete. Legacy v1/v2 databases gain empty input table without fabricated task data; old `RunSnapshotV1.task` remains readable but is intentionally ignored by aggregate rehydration. `RunSnapshotV2` no longer contains task text.

Schema v4 adds separately owned process infrastructure:

```text
managed_processes(id, run_id, stage_id, attempt,
                  backend_kind, backend_session_id, status,
                  spec_schema_version, spec_json, command_fingerprint,
                  stdout_offset, stdout_cursor_revision,
                  stderr_offset, stderr_cursor_revision,
                  exit summary, interrupt_requested, revision, timestamps)
```

Schema v5 extends process identity with positive invocation number and immutable stdin path/hash, then adds provider sessions and artifacts:

```text
provider_sessions(id, run_id, stage_id, attempt, provider_id,
                  native_session_id, current_process_id, status,
                  protocol_version, invocation, model_id, cli_version,
                  pending attention range, revision, timestamps)

artifacts(id, run_id, stage_id, attempt, kind, status, role,
          provider_id, model_id, path, content_hash, content_size,
          base_commit, timestamps)
```

Provider-session identity `(run, stage, attempt, provider)` is immutable and distinct from backend/process identity. One attempt may use multiple invocations while continuing one native conversation. Artifact rows are insert-only; bytes are written and fsynced before metadata commit, then hash/size verified on insertion and load.

Launch identity is immutable and unique per `(run_id, stage_id, attempt, invocation)`. Lifecycle and each output cursor use independent compare-and-swap revisions. Process rows reference runs but never enter `RunSnapshot`; existing v1-v4 state migrates without rewriting run, event, configuration, input, workspace, or apply data.

Resolved config payloads are recursively key-sorted and compact-encoded before SHA-256 hashing. An existing config ID accepts an exact idempotent insert only; different content or metadata is rejected. Database triggers reject update and delete operations, enforcing insert-only storage beneath the Rust API.

Every update performs compare-and-swap:

```sql
UPDATE runs
SET snapshot_json = ?, revision = revision + 1, ...
WHERE id = ? AND revision = ?
```

Zero changed rows means `ConcurrentModification`. Snapshot update precedes event inserts inside one `BEGIN IMMEDIATE` transaction; any event constraint failure rolls back snapshot, revision, and event changes together. Store allocates contiguous event sequence values from the prior per-run maximum. Event timestamps must be non-decreasing, may be equal, and final event time must equal persisted `run.updated_at`.

Current aggregate snapshot excludes task input, artifact metadata, and provider-session state because `Run` does not own them. Provider-neutral session/checkpoint events remain durable history. `FakeProvider` reconstructs deterministic cursor from those events; Claude and Codex reconstruct separately owned provider-session/process/output state without weakening `Run::rehydrate`.

## Run lifecycle

```text
Created -> Preparing -> Ready -> Running
                                  |-> NeedsUser -> Running
                                  |-> Paused ----> Running
                                  |-> Interrupted -> Running
                                  |-> Completed -> Applied
                                  |              -> Discarded
                                  |-> Failed
                                  `-> Discarded
```

`Ready` is an explicit atomic boundary: immutable configuration and run preparation succeeded, but execution has not begun. `Run` owns all mutable stages and attention requests so lifecycle invariants can be checked in one aggregate.

`Completed`, `Failed`, and `Discarded` mean execution is finished. Only `Applied` and `Discarded` permanently close normal lifecycle mutation; `Completed` remains eligible for explicit apply or discard. Completion requires all stages to have outcomes, no unresolved attention, and no blocking failed stage.

## Stage lifecycle

```text
Pending -> Ready -> Running -> Completed
   ^                  |-> NeedsUser -> Running
   |                  |-> Paused ----> Running
   |                  |-> Interrupted -> Running
   |                  `-> Failed -> Retry -> Pending
   `---------------------- Skipped (from Pending or Ready)
```

`Ready` records that dependency outcomes were checked. Required dependencies must complete successfully. Optional dependencies must reach an outcome; failure or skip permits `Ready` with explicit degraded evidence. A failed stage is execution-finished but retryable, so it is not permanently closed. Completed and skipped stages cannot restart.

A failed stage may retry while every direct dependent remains `Pending` or `Ready`. `Ready` preserves evidence that dependency validation already occurred. Once a dependent starts execution or reaches any later outcome, retry is rejected; revisiting earlier work requires a new stage/attempt instead of rewriting history. Interrupted work uses recovery, not retry.

Pause and interruption are distinct at both levels. `Paused` records deliberate user suspension and accepts `Resume`; `Interrupted` records unexpected runtime loss and accepts `Recover`. Each suspension remembers whether work should return to `Running` or `NeedsUser`, preventing attention state from being lost.

## Attention

An attention request is uniquely identified, tied to one run and stage, and typed as permission, decision, or question. A stage may accumulate multiple requests. `NeedsUser` always corresponds to at least one unresolved request; resolving or cancelling the final request restores running state immediately or updates the saved resume target while suspended. Failing or discarding a run cancels pending attention without erasing its history.

## Workflow vocabulary

A stage kind describes work such as implementation or synthesis. A role describes responsibility such as implementer or engineering lead. Neither chooses provider nor model:

```text
workflow stage -> engineering role -> provider -> model
```

Workflow definitions are validated DAGs with unique stage IDs and known, non-self, non-duplicate required or optional dependencies. Built-in Fast, Standard, Deep, and Review workflows are ordinary Rust graph data.

New built-in graphs are:

```text
Fast
Implementation -> Verify

Standard
Architecture ---> Implementation ---> Simplification ---> Code Quality Review --+
|                 |                   |                                         |
+-----------------+-------------------+--> Specification Review ----------------+-> Decision
                                      |                                         |
                                      +--> Verify ------------------------------+

Deep
Research -> Architecture ---> Implementation ---> Simplification ---> Code Quality Review --+
            |                 |                   |                                         |
            +-----------------+-------------------+--> Specification Review ----------------+-> Decision
                                                  |                                         |
                                                  +--> Verify ------------------------------+

Review
Research -> Code Quality Review --+
        `-> Specification Review -+-> Synthesis -> Decision
```

Standard and Deep require both review branches before Decision and wait for the verification through an optional edge. Specification Review directly depends on Architecture, Implementation and Simplification so its prompt receives those artifacts; Code Quality Review depends on Implementation and Simplification. Decision directly receives both review artifacts and the verification artifact, passed or failed. Review keeps both reviewer-to-Synthesis edges optional, preserving degraded-evidence progress if one branch fails or skips.

`Verify` is the one stage kind no agent performs. `Role::Verifier` resolves inside the router to the deterministic `verify` provider (`src/providers/verify/`), which runs the repository's own commands — the `[verify]` table of `<worktree>/.senate.toml`, else of `<source repo>/.senate.toml`, else the command its build file implies — in the worktree, records every exit code in a Markdown artifact, and completes only when all are zero. The route is implicit: it is never written to a configuration snapshot, never part of a profile or resource plan, and never counted among the roles a snapshot must cover, so runs sealed before the stage existed still decode and still grow fix cycles, each of which carries its own `verify_<n>`. A failed verification does not fail the run — the decision's edge to it is optional, so the lead reads the failure and the run completes, leaving fix and continue cycles (each with its own `verify_<n>`) available to answer it; `apply` and `pr` refuse by name until the run's latest verify stage is `Completed`. Evaluations run the built-ins through `WorkflowDefinition::without_verification`, because the fixture's checks are not what they measure.

`CodeQualityReviewer` owns HOW implementation is engineered: simplicity, readability, maintainability, boundaries, error handling, tests, and avoidable complexity. `SpecReviewer` owns WHAT behavior was delivered against immutable `RunInput` and design evidence, separating Missing, Wrong, and Unrequested behavior. Both inspect actual worktree state independently and emit distinct Markdown artifacts. Shared provider-neutral semantic instructions keep these contracts aligned; native adapters add provider-specific framing.

Legacy `Reviewer`, `Review`, `IndependentReview`, and `DeepAnalysis` values remain valid. `RunSnapshotV2` already persists complete `StageDefinition` data, and resume passes `loaded.run.workflow()` to provider reconstruction. Existing runs therefore rehydrate and execute their original graph; only new run creation calls current built-in definitions. Adding enum values changes no snapshot shape or database table, so snapshot and database schema versions remain unchanged.

## Milestone 4 workflow execution

Scheduler loop is graph-driven:

```text
load validated Run + Ready workspace
    -> reject active apply intent
    -> evaluate every Pending stage dependency set
    -> atomically mark all newly Ready or blocked stages
    -> resolve stage role through immutable RoutingPlan to actual provider/model target
    -> consume one provider record (one signal or atomic signal batch)
    -> atomically commit Run state + complete event batch
    -> evaluate graph again
```

No branch checks `WorkflowKind` during scheduling. Specialized reviewer branches become Ready in one dependency pass when their declared prerequisites complete. Standard and Deep join successful review outcomes through required edges; Review synthesis joins terminal outcomes through optional edges. Initial scheduler executes one eligible stage at a time even when several are Ready, preserving deterministic tests while retaining DAG parallelism for a later process backend.

`FakeProvider` is first provider. Scenarios script start, progress, usage, human attention, pause, interruption, completion, failure, and explicit delay gates. Every consumed signal has one identifying semantic event (`ProviderStarted`, `ProviderProgress`, `ProviderNeedsUser`, `ProviderUsageUpdated`, `ProviderPaused`, `ProviderInterrupted`, `ProviderCompleted`, or `ProviderFailed`). Signal index and attempt derive from per-stage event history, resetting only after explicit `StageRetryScheduled`; restart therefore cannot silently replay an already committed fake signal. `ProviderNeedsUser` links provider/session to the independently persisted `NeedsUser` lifecycle event and attention request.

Execution commits recheck `WorkspaceStatus::Ready` inside same `BEGIN IMMEDIATE` transaction used for run compare-and-swap and event append. Active `Prepared` or `AppliedToSource` apply intent rejects execution through existing store guard. Preflight checks provide typed engine errors; transactional checks close race windows.

Attention resolution, stage resume, interruption recovery, and retry are scheduler-boundary commands. They use same workspace/apply guards and atomic run commit as automatic advancement.

## Milestones 7–9 application, providers, routing, and CLI

`RunService` is application boundary. CLI parses and prints only; service owns use-case ordering:

```text
validate RunInput + discover repository
    -> resolve immutable provider config and verify availability
    -> atomic Run + RunInput + config + created event
    -> prepare graph-selected workspace
    -> reconstruct provider from config + workflow + events
    -> drive scheduler to quiescence
    -> reload committed events and query DTO
    -> print
```

Provider construction sits behind `ProviderFactory`. Runtime factory accepts explicit `claude`, `codex`, `opencode`, or `fake`, persists choice in immutable run configuration, and reconstructs same provider on restart without fallback. Fake keeps `development_fake/default_success_v1`. Claude, Codex, and opencode configurations persist safe selection/options only; native credentials and environment are never snapshotted. Installed CLI version and provider-confirmed model/session, when exposed, are runtime metadata. `ExecutionSelection::UniformWithModel` is the one additive shape this took: `Uniform(UniformProvider)` alone would send opencode's every role to the CLI's own default, which for opencode is not one provider's default but whichever vendor its default model happens to name — the opposite of the flag's purpose — so an explicit `--model` on `--provider` now reaches uniform routing directly instead of only through a stage-level `retry --provider --model` override.

`ClaudeProvider` uses native CLI structured print mode with `dontAsk`, never broad permission bypass. Initial prompt and continuations enter through immutable stdin. JSONL decoder accepts one complete record per poll; partial record waits, unknown valid records become non-semantic checkpoints, and invalid JSON fails without cursor advancement. System init binds opaque native session, assistant/result records map to provider-neutral usage/progress/attention/failure/completion.

Specialized Claude reviewer prompts explicitly prohibit edits. Current Claude adapter retains native `dontAsk` policy and user configuration; it has no additional provider-independent hard read-only sandbox equivalent to Codex. The Senate documents this limitation instead of overriding native permissions with a custom mechanism.

A denied native tool call becomes typed permission attention, with one evidence-based exception: on a successful terminal, a denied non-mutating request whose refusal the agent demonstrably worked around — a later call of the same tool, in the same invocation, returning a non-error result — is spent history and raises nothing. The evidence is read from that invocation's own retained output without moving any cursor; absent, unreadable, or oversized output yields no evidence and the operator is asked. A refused mutation or question is never excused this way, because the operator still needs to know the agent was blocked from the work itself.

Denied native tool calls become typed permission attention. SQLite stores only attention identity and exact raw-record range; human resolution reconstructs structured denial from retained output, converts only safely representable exact rule to native `--allowedTools`, and starts new `--resume <same-session>` invocation. Ambiguous/wildcard rules fail closed. Native questions require explicit `resolve --response`; answer is immutable run-private stdin, not argv or SQLite event payload.

On success, Claude result becomes human-readable stage artifact. Downstream prompt includes only direct dependency artifacts. Provider session CAS, raw-output cursor CAS, run snapshot/revision, complete semantic event batch, and artifact metadata share one `BEGIN IMMEDIATE` transaction. Fault before commit replays record; no accepted signal can exist without matching session/cursor checkpoint.

### Native Codex CLI

`CodexProvider` uses `codex exec --json`, prompt `-` on immutable stdin, and `--output-last-message` under run-private provider output. Native user/project configuration, authentication, `AGENTS.md`, rules, skills, MCP, and hook trust remain active. Codex immutable config is `native_codex` schema 1 with `exec_json_v1`, `stage_kind_v1`, and approval `never`; model `null` omits `--model` and preserves native default.

Codex maps both specialized reviewer stage kinds to native `read-only` sandbox with approval `never`. Implementation and Fix alone receive `workspace-write`; adding reviewer roles never weakens this stage-derived policy.

Execution controls are explicit and separate. `Implementation`, `Fix`, and `FollowUp` select `workspace-write`; all other stage kinds select `read-only`. Approval is `never` for deterministic non-interactive execution but sandbox remains enabled. Dangerous sandbox/approval bypass, `danger-full-access`, ephemeral sessions, Git-check bypass, and native config/rules bypass are prohibited.

`thread.started` binds provider-issued `thread_id` to generic native session identity. Duplicate identical identity checkpoints; conflict fails closed. Recovery consumes retained output first, then resumes exact persisted thread through `codex exec ... resume <thread-id> -` in new invocation. Failed-stage retry creates new provider session and thread. If process disappears before any thread identity exists, retained output is still parsed; only absence of recoverable identity permits later initial invocation for same attempt.

Decoder handles one complete JSON line per poll. Partial line waits; unknown valid event checkpoints cursor without semantic event; invalid complete JSON fails without cursor advance. Agent messages may become progress. Reasoning content is never exposed. Command/file/MCP/web/plan items produce only bounded generic progress or checkpoints, never raw payloads. `turn.failed` and `error` fail stage. Current stable exec JSON offers no typed safely resumable approval/question request, so Codex does not fabricate `NeedsUser` from prose; this differs intentionally from Claude typed permission continuation.

One `turn.completed` contains both stable input/output token usage and successful boundary. Generic `ProviderPoll::Emission` therefore carries ordered signal batch. Scheduler applies `[Usage, Completed]` to in-memory run, then existing semantic provider transaction commits run/events, provider session Completed, artifact metadata, and one output-cursor acknowledgement. Crash before commit replays raw line; after commit both effects exist once. Fake and Claude use singleton batches, preserving behavior.

Provider waits for protocol completion plus successful managed-process exit, or — when the process died after `turn.completed` with a non-zero, signalled, or entirely absent exit result — corroboration of the persisted `--output-last-message` file against the retained stream. Codex reports its final message twice, as the last `item.completed` agent message and byte for byte in that file, so corroboration requires the two to be equal (a missing trailing newline is the sole tolerated difference, because the artifact writer adds it anyway). Equality proves completeness, not mere existence: a file the CLI was killed part-way through writing is a strict prefix and fails, and a stream carrying no agent message corroborates nothing. The file is read only through the artifact ceiling, so an oversized one is refused rather than buffered, and it is measured as the artifact writer measures it — the newline that writer appends counts against the ceiling, in both the corroboration check and the writer's own. An uncorroborated dead-process completion is not trusted and not raised either: it consumes the record and reports provider interruption, so the attempt stays recoverable through the ordinary recovery path instead of stranding finished work behind a protocol error that every later poll repeats. Corroboration covers only those deaths: a requested interruption reports itself as one without consulting any file, and `Broken` — the supervisor failing rather than Codex — keeps raising, because a completion mapped over it would hide an infrastructure failure. Final assistant file is copied into canonical immutable artifact with write-once fsync/hash semantics before transaction. Crash before metadata commit leaves replayable final file and possibly identical canonical orphan; replay verifies/reuses bytes without duplicate artifact row. Downstream prompts remain limited to direct dependency artifacts.

Before continuation, application reconciles workspace. Engine/store guards still require `WorkspaceStatus::Ready` and reject active apply intent at mutation transaction. Resume policy continues Ready/Running, resumes deliberate suspension, recovers interruption, preserves `NeedsUser`, refuses implicit retry from Failed, reports Completed/Applied, and rejects Discarded. `resolve` and `retry` perform exact explicit action then drive again.

Query DTOs (`RunListItem`, `RunDetails`, `StageSummary`, `AttentionSummary`, `UsageSummary`) isolate CLI formatting from mutable domain/store internals. List query uses indexed run columns with `run_inputs`/workspace joins and does not decode every snapshot. Detail query rehydrates one run and aggregates committed usage events. Read-only commands perform no lifecycle mutation; `runs` does not create missing database.

Execution reports contain only event rows reloaded after successful commits. CLI therefore never publishes speculative provider signals. Needs-user, pause, interruption, and failed outcomes use exit 0 as valid quiescent states; operational failures use exit 1 and Clap parse failures use exit 2.

### Native opencode CLI

`OpencodeProvider` drives `opencode run --format json -m <provider/model> [--variant <level>] [--session <id>] --dir <worktree> "<prompt>"`. Native `opencode --version`, `auth login`/`auth list`, and `opencode models` remain the sole authentication and discovery surface (`~/.local/share/opencode/auth.json`); a configured model absent from `opencode models` fails closed before any process launches, ahead of the vendor's own vague `UnknownError`. opencode is explicit-only routing (`--provider opencode --model <provider/model>`, and the same explicit model on `--provider opencode`'s own uniform routing via `ExecutionSelection::UniformWithModel`): it is never a `--profile recommended` candidate and never a Recommended fallback, because the model id — not the provider — carries the vendor, and Recommended's provenance names concrete runtimes it measured, not an open-ended model catalogue.

opencode's `run --help` (v1.18.32, verified by hand) offers no stdin or file-based prompt; every other adapter's immutable-stdin prompt has no analogue here; the stage prompt is instead the invocation's last positional argument, held under a 256 KiB argv-safety ceiling — well inside every documented POSIX `ARG_MAX` — with the same navigation-evidence shedding (change handoff, then operator instruction) Codex's own over-limit prompt uses before failing closed.

Permissions travel through a per-invocation JSON file passed as `OPENCODE_CONFIG` (verified by hand to merge with, not replace, `~/.config/opencode`). Read-only stage kinds — the identical set Codex maps to `read-only` — deny `edit`, `bash`, `webfetch`, and `external_directory` outright. `Implementation`, `Fix`, and `FollowUp` allow `edit`, still deny `webfetch` and `external_directory`, and gate `bash` behind `{"*":"ask", <pattern>: "allow", ...}`; the allowed patterns are the repository's own Claude `[permissions] allow` baseline (`src/providers/claude/permissions.rs`, exposed `pub(crate)` for exactly this reuse) translated from Claude's `Bash(<prefix>:*)` rule syntax onto opencode's `<prefix> *` glob pattern, so a repository states its safe commands once rather than once per provider. `--auto` is never passed.

Every opencode event (`step_start`, `tool_use`, `text`, `step_finish`, `error`) carries its own `sessionID`; there is no dedicated `thread.started` announcement the way Codex has one. The adapter therefore binds — or, on a resumed invocation, re-confirms — native session identity from whichever record happens to arrive first for an invocation, driven by the persisted session's own `Starting` status rather than by record type: `Started` is emitted only when that bind is the stage's very first signal, every later invocation (including a resume) reports `Resumed`. Headless opencode auto-rejects a permission request it cannot interactively ask about and keeps going; the denial becomes bounded `ProviderProgress` (never the raw command or, for a denied external-directory write, the full permission-rule dump opencode's own error text can carry) and no typed continuable permission request is fabricated — opencode's protocol offers no safely representable way to resume with broadened scope, the same posture Codex holds for its own undecided attention, and a documented difference from Claude's typed permission continuation.

Tokens are reported per `step_finish`, not once per turn as Codex's `turn.completed` is; the adapter emits one `UsageDelta` per non-terminal step plus the terminal one, and the existing multi-event usage summation folds them at read time exactly as multi-message Claude/Codex usage already does. `input` is net of `cache.read` (verified against a real record: `input + output + cache.read == total`), the same convention Claude uses and the opposite of Codex's cache-inclusive `input_tokens`. opencode's stream never names the model or effort it resolved and, unlike Codex, no read-only observation of opencode's own session storage exists here to confirm one after the fact; confirmed model and effort stay permanently unavailable for opencode, the same as Codex before any `RuntimeObserved` evidence — a documented gap, not an oversight.

A terminal `step_finish` (`reason: "stop"`) is trusted as completion only on a clean process exit. opencode writes no second, independently-written file the way Codex's `--output-last-message` is cross-checked against a retained `item.completed` agent message, so there is no corroboration path available for an unclean exit; any process ending other than a clean zero-code exit after a `stop` step is reported as a recoverable interruption rather than a trusted completion — a stricter, simpler rule than Codex's, made explicit because the file that rule depends on has no opencode analogue. The final answer itself is reconstructed, not read from a dedicated file: the adapter concatenates every `text` part retained in the process's own stdout under the winning step's `messageID`, in stream order, up to the deciding `step_finish` record's own offset.

The Implementer world view (`senate world`) colours a stage by the vendor its model names rather than by `opencode` itself: `src/world/projection.rs::colour_key` maps an opencode stage's model id onto the same vendor keys (`deepseek`, `gemini`, `qwen`, `kimi`) `world/js/models.js` already carried, so a DeepSeek and a Kimi stage read as distinctly as Claude and Codex do; an opencode model naming no recognised vendor keeps the existing unrecognised-provider fallback.

## Process and recovery

Provider adapters depend on `ProcessBackend`, not tmux. `TmuxBackend` implements availability, exact launch, owned-session inspection, raw output reads, graceful interruption, and ownership-safe cleanup. `ProviderRequest` remains provider-neutral; `ProviderPoll` may carry signal plus neutral persistence checkpoint.

Process launch uses intent/effect/finalize:

```text
Preparing persisted
    -> immutable spec/stdin/output files materialized
    -> Starting claimed by lifecycle CAS
    -> tmux direct-argv runner launch
    -> owned session or valid exit evidence observed
    -> Running / Exited finalized
```

Tmux receives hidden runner executable, subcommand, and manifest path as separate arguments. No launch path uses `sh -c`, quoting, `eval`, or interpolated command text. Each managed process uses isolated tmux server. Session environment carries safe operational variables plus non-secret process ID, fingerprint, and one-time socket path; existing sessions are reusable or removable only when both ownership markers match persisted identity.

Parent environment is cleared before tmux server starts. Native provider variables excluded from safe session allowlist cross through bounded user-only (`0600`) Unix socket after launch, never argv, tmux environment, manifest, SQLite, or durable file. Runner receives bytes in memory, validates framing/size, clears inherited environment, and reconstructs provider environment before exact child exec. This preserves native environment-based authentication without credential persistence or command-line exposure.

Runner validates its manifest and ownership, creates a separate child process group through hidden exec bridge, redirects stdout/stderr to regular append-only files, persists live runner/child identity in atomic `runtime.json`, waits for exact child exit, then publishes atomic `exit.json`. Exec bridge converts tmux's inherited ignored SIGINT disposition into caught disposition before spawn and then Unix `exec` resets it to default in provider image; this keeps Ctrl-C termination portable across macOS and Linux. Backend interruption validates session ownership, runner pane PID, runtime fingerprint, and child process group before sending SIGINT. Cleanup is separate and retains all process files.

Process state is reconciled from independent evidence:

```text
Preparing + no session/no exit       -> safe to start
Preparing + owned session            -> Running
Starting/Running + owned session      -> Running
active + no session + valid exit      -> Exited or Interrupted
active + no session + no exit         -> Missing
mismatched/corrupt evidence           -> Broken
terminal + owned cleanup              -> Cleaned, files retained
```

Absence never implies success. Tmux sessions survive client detachment/process exit, but not reboot or tmux server loss. Without valid exit evidence, lost supervisor state becomes `Missing`; Claude and Codex map loss to semantic interruption after native identity exists and recover through same native session only after explicit run recovery.

Output files live under `runs/<run>/processes/<process>/`. Reads return raw byte chunks with start/end offsets and do not mutate SQLite. Consumer explicitly acknowledges consumed prefix through per-stream cursor CAS. Native-provider semantic records combine run/events, provider session, artifact metadata, and acknowledgement in one transaction. Crash before commit replays bytes; reads remain available after exit.

## Git safety

Git runs through `std::process::Command` with direct argument arrays; no shell command interpolation is used. Repository discovery persists canonical source path, canonical Git common directory, and immutable base commit. Paths are passed as OS arguments, while NUL-delimited Git output is used where records must be parsed. Binary patches use short-lived temporary files so large input cannot deadlock subprocess pipes; files are removed automatically after each command.

Managed worktrees live under:

```text
~/.senate/worktrees/<sanitized repository + short common-dir hash>/<run-id>
```

Implementation worktrees use deterministic `senate/run-<run-id>` branches. Review worktrees are explicitly detached. Source checkout may be dirty during preparation because worktree starts from committed `HEAD`; source must be fully clean during apply.

SQLite and Git cannot share a transaction. Workspace lifecycle therefore uses durable intent and reconciliation:

```text
Preparing persisted -> git worktree add -> identity validation
    -> workspace Ready + Run Ready committed atomically

Removing persisted -> ownership validation -> git worktree remove
    -> compare-and-delete owned branch -> Removed persisted
```

Reconciliation retries absent `Preparing` resources, finalizes already-created valid resources, continues `Removing`, and leaves repeated terminal operations idempotent. Missing ready resources, relocated repositories, foreign paths, branch collisions, moved branch tips, or other ambiguous evidence become `Broken`; The Senate does not guess or delete foreign data.

Workspace status changes are infrastructure control state, not new domain events. Existing `RunPreparationStarted`, `RunPrepared`, `RunApplied`, and `RunDiscarded` events capture semantic behavior; individual Git commands and cleanup progress stay out of domain history.

Apply computes exact delta from persisted base commit using a temporary Git index. `read-tree`, `git add -A`, and `git diff --cached --binary --full-index` include tracked edits, untracked files, deletions, file modes, unusual UTF-8 filenames, and binary data without touching source or worktree index. Apply then requires clean source, persists SHA-256 patch intent and run revision, runs `git apply --check`, and runs `git apply` without staging or committing.

Git apply and SQLite lifecycle finalization form another intent/effect/finalize boundary. Recovery regenerates patch and requires identical hash. Forward-check success means effect may proceed; reverse-check success with forward failure proves expected patch is already present and permits exactly-once logical finalization. Ambiguous evidence fails closed. Apply retains worktree for inspection.

Creating apply intent uses run revision compare-and-swap. While status is `Prepared` or `AppliedToSource`, ordinary run commits and workspace cleanup are rejected; final apply records operation, run snapshot, revision, and `RunApplied` event in one SQLite transaction.

Discard commits `RunStatus::Discarded` before cleanup. Cleanup independently removes worktree resources for completed, applied, or discarded runs without changing logical status. Branch deletion requires persisted ownership, no remaining checkout, and atomic expected-tip deletion; movement after removal intent produces `Broken` and preserves branch.

## Current layout

```text
src/
├── lib.rs           importable application and domain library
├── main.rs          thin process entry
├── cli/
│   ├── mod.rs       CLI schema
│   └── commands.rs  thin use-case dispatch and committed-state rendering
├── app/
│   ├── run_service.rs orchestration use cases and quiescence policy
│   ├── mission_service.rs mission use cases, run observation, handoff rendering
│   ├── mission_result.rs delivery evidence captured from the run store
│   ├── mission_query.rs mission read models
│   ├── provider_factory.rs restart-stable provider construction
│   ├── query.rs      CLI-facing read models
│   └── error.rs      typed application failures
├── config/
│   └── mod.rs       side-effect-free configuration path resolution
├── bin/
│   └── senate-test-agent.rs fixture executable driven by process tests
├── domain/
│   ├── run.rs       aggregate, lifecycle, dependency and attention rules
│   ├── stage.rs     stage state machine
│   ├── workflow.rs  workflow identity and validated DAG definition
│   ├── attention.rs human-attention lifecycle
│   ├── event.rs     provider-neutral semantic events
│   ├── artifact.rs  typed artifact metadata
│   ├── role.rs      provider/model-independent responsibility
│   ├── effort.rs    provider-neutral resource intent
│   ├── rehydration.rs persistence-neutral reconstruction data
│   ├── mission.rs   mission aggregate: work packages, dependencies, decisions, derived attention
│   └── ids.rs       strong domain identities
├── engine/
│   ├── scheduler.rs deterministic DAG evaluation and guarded commits
│   ├── provider.rs  provider-neutral synchronous signal boundary
│   ├── fake.rs      validated scripts and restart-stable FakeProvider
│   └── error.rs     typed execution/protocol failures
├── eval/
│   ├── case.rs       source-controlled role cases and ground truth
│   ├── suite.rs      versioned suite identity and fingerprints
│   ├── runner.rs     isolated orchestration, evidence capture, validation
│   ├── scorer.rs     deterministic role-specific matching
│   ├── result.rs     validated EvalResultV1 codec
│   └── report.rs     role aggregation and target comparison
├── process/
│   ├── backend.rs   provider-independent process supervisor contract
│   ├── manager.rs   persisted intent/effect/finalize and reconciliation
│   ├── tmux.rs      ownership-safe shell-free tmux backend
│   ├── runner.rs    hidden exact-argv child runner and durable evidence
│   ├── model.rs     process spec/status/output/exit records
│   ├── ids.rs       managed-process and backend-session identities
│   └── error.rs     typed process/backend failures
├── providers/
│   ├── session.rs   provider-neutral conversation identity and lifecycle
│   ├── checkpoint.rs atomic provider commit payload
│   ├── artifact.rs  immutable artifact record
│   ├── stage_prompt.rs shared provider-neutral stage semantics
│   ├── change_handoff.rs deterministic bounded implementation-change handoff for review stages
│   ├── continue_instruction.rs immutable run-private storage for one continue cycle's instruction
│   ├── repo_config.rs which checkout `.senate.toml` is read from: worktree, then source repository
│   ├── claude/      native discovery, argv, prompts, JSONL decoder, adapter
│   ├── codex/       native discovery, exec argv, prompts, JSONL decoder, adapter
│   ├── opencode/    native discovery/model listing, argv, permission-config JSON, prompts, JSON-line decoder, adapter
│   └── verify/      deterministic command runner: `.senate.toml` reader, argv runner with timeout, Markdown artifact
├── store/
│   ├── sqlite.rs    transactional store and indexed projections
│   ├── snapshot.rs  RunSnapshotV1/V2 migration and codec
│   ├── migrations.rs SQLite schema lifecycle
│   ├── config_snapshot.rs immutable config and canonical hash
│   ├── image.rs     insert-only image-generation evidence rows
│   ├── mission.rs   mission input, snapshot codec, event log, run index
│   ├── run_input.rs immutable normalized task input
│   ├── process.rs   process lifecycle and output-cursor CAS persistence
│   ├── provider.rs  provider-session/artifact persistence and atomic commits
│   ├── workspace.rs workspace/apply intent persistence and CAS
│   ├── path.rs      data and worktree path resolution
│   └── error.rs     typed persistence failures
├── image/
│   ├── mod.rs        ImageGenerator trait and backend-neutral request/result types
│   ├── codex.rs      backend on the local Codex CLI built-in image_gen tool
│   ├── fake.rs       deterministic test backend
│   ├── service.rs    authorization, bound, placement, evidence for one call
│   ├── path.rs       untrusted output_path validation and no-overwrite atomic write
│   ├── host.rs       per-run unix-socket host inside the The Senate process
│   ├── mcp.rs        stdio MCP shim the native CLI launches (`__image-tool`)
│   └── png.rs        PNG header validation and deterministic synthesis
├── git/
│   ├── command.rs    native Git command runner
│   ├── repository.rs canonical repository identity
│   ├── worktree.rs   create/inspect/remove and branch ownership
│   ├── remote.rs     remote URL lookup and non-forced branch push
│   ├── patch.rs      temporary-index patch generation and apply
│   └── error.rs      typed Git failures
├── workspace/
│   ├── manager.rs    intent/effect/finalize orchestration
│   ├── model.rs      workspace and apply-operation records
│   ├── github.rs     minimal GitHub CLI (`gh`) boundary for opening pull requests
│   ├── setup.rs      `[setup]` commands run in a new worktree before it is ready
│   └── error.rs      typed lifecycle/reconciliation failures
├── update/
│   ├── mod.rs        update discovery for official releases
│   ├── release.rs    official release metadata behind an injectable source
│   ├── cache.rs      tiny forward-compatible update-check cache
│   ├── install.rs    how this copy of The Senate was installed and what an update may touch
│   └── installer.rs  safe replacement of an official binary
└── tui/
    ├── app.rs        refresh/event loop and application-action dispatch
    ├── state.rs      ephemeral presentation state and composer editing
    ├── render.rs     Ratatui rendering and TestBackend coverage
    ├── input.rs      terminal key-to-intent mapping
    ├── worker.rs     one serialized standard-thread action worker
    ├── terminal.rs   raw-mode/alternate-screen RAII and panic restoration
    ├── bottom_line.rs artifact's own opening statement reduced to one line
    ├── follow_ups.rs decision's `## Follow-ups` section read verbatim
    ├── section.rs    shared Markdown heading matching for bottom_line and follow_ups
    ├── markdown.rs   bounded terminal Markdown renderer for artifact viewing
    ├── format.rs     deterministic operational formatters
    ├── theme.rs      one palette and the surfaces built from it
    ├── motion.rs     when the interface is allowed to move
    ├── mascot.rs     POD, the The Senate Operator Droid
    └── mod.rs        public TUI entry point
```

Domain operations are deterministic: callers supply UTC timestamps. Invalid transitions and persistence failures return typed `thiserror` errors; `anyhow` remains at the application boundary. Serde uses inspectable snake-case values. Aggregate deserialization is prohibited; versioned DTO decoding always ends at validated rehydration.

## Decisions

- Single Cargo package and binary; no workspace.
- Rust 2024 edition with Rust 1.85 minimum, matching stable edition support.
- Configuration lookup uses standard environment variables and no platform-directory crate because required location is explicitly `~/.config/senate`.
- Missing subcommand opens TUI only when stdin and stdout are interactive; non-interactive use prints help without terminal control sequences.
- No configuration file is created during bootstrap.
- Internal domain/database schema must not depend on provisional branding beyond filesystem and package identity where unavoidable.
- Explicit `Ready` is persisted because preparation and dependency validation need atomic recovery boundaries.
- User-requested pause and unexpected interruption are separate states and transitions.
- Cleanup is resource retention, not a run lifecycle state.
- `rusqlite` uses bundled SQLite for deterministic local availability; persistence remains synchronous until orchestration needs async boundaries.
- JSON snapshots avoid premature normalization while indexed projection columns support current list/inspection needs.
- Run workspace state stays outside logical run snapshots; filesystem availability never participates in `Run::rehydrate`.
- Git effects use intent/effect/finalize sagas with explicit reconciliation; SQLite transactions never span subprocess execution.
- Apply uses patch transfer instead of merge/cherry-pick and never stages or commits source changes.
- Discard is a logical disposition; cleanup is an independent physical-resource operation.
- Built-in workflows are validated DAG data; scheduler contains no workflow-specific execution branches.
- New built-ins use independent Code Quality Review and Specification Review branches; legacy persisted generic review graphs remain unchanged.
- Reviewer semantic contracts are provider-neutral, while native adapters retain provider-specific transport and safety policy.
- One consumed provider record produces one durable checkpoint; an ordered signal batch shares same atomic run commit and one raw cursor acknowledgement.
- Scheduler is synchronous and single-stage deterministic in Milestone 4; async/process concurrency remains a backend concern.
- User task is immutable `RunInput`, not aggregate lifecycle state or provider configuration.
- Workflow workspace mutability derives from stage kinds (`Implementation`/`Fix`), not workflow-name branches.
- CLI execution choice is explicit: `--provider` produces uniform routes and `--profile recommended` produces versioned creation-time routes; both are restart-stable immutable run configuration.
- Application commands run scheduler to durable quiescence and render only reloaded committed state/events.
- Managed processes are separate infrastructure attempts; process exit does not directly mutate semantic run/stage state.
- Exact external argv is preserved end to end; tmux launches multiple command arguments directly rather than a shell command string.
- Process launch, interrupt, and cleanup require persisted intent plus fingerprint-bound ownership evidence.
- Raw output read and acknowledgement are separate; provider semantic commit atomically joins cursor, session, artifact metadata, run state, and events.
- Provider session, managed process, and backend session are distinct identities; continuation advances invocation without changing attempt/native conversation.
- Native Claude default model is used unless immutable configuration supplies one; model shown to user comes from provider confirmation.
- Permission continuation uses same Claude UUID and exact safely representable native allow rule; broad/ambiguous approval fails closed.
- Native Codex default model is used unless immutable configuration supplies one; no model is marked confirmed without protocol evidence.
- Codex sandbox derives from stage kind and remains enabled with approval `never`; no prose heuristic creates human attention.
- TUI is an ephemeral projection/control surface; canonical state and all execution remain below application boundary.
- TUI read APIs are side-effect free: no reconciliation, output acknowledgement, apply intent, real-index mutation, or semantic event.
- Blocking application actions are serialized on one standard thread; frontend detach never implies provider interruption or run disposition.
- A mission is a plan above runs, not a new execution model: packages are delivered by ordinary runs, package state follows committed run status, and integration is recorded on run evidence (`Applied` or an empty delta), never on an agent's report.
- Package readiness requires integrated dependencies, so a child run's base commit already carries what it builds on; a contract freezes once a run serves it.
- opencode is explicit-only routing, never a Recommended candidate: its model id, not the provider, carries the vendor, so Recommended's per-runtime provenance has no concrete target to measure.
- opencode's stage prompt travels as argv, under a documented ceiling, because the installed CLI offers no stdin or file-based alternative; every other adapter's stdin-only prompt privacy does not extend to opencode.
- A denied opencode permission becomes visible progress, never a fabricated typed continuable request: headless opencode auto-rejects what it cannot interactively ask about, and no safely representable resume-with-broadened-scope exists for that shape.
- An unclean opencode process exit after a terminal step is never trusted as completion, because opencode has no second independently-written file to corroborate it against the way Codex's `--output-last-message` is.
