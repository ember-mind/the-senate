# Resource observability and effort policy

See what each stage consumed and how long it took, and request how much native-runtime effort a run should use, without either influencing routing.

## Sub-features
- usage: provider-native units per stage (input, output, cache read, cache write, reasoning output) plus Claude `modelUsage` per-model view; `unavailable` means not reported, never zero.
- latency: wall-clock from first `ProviderStarted` to the last terminal provider event per invocation, from committed event timestamps.
- invocations and prompt bytes: count of persisted native invocations and exact stdin bytes piped per invocation (SHA-256-verified stdin file).
- effort: requested per role and persisted as a `ResourcePlan` in config schema v3; adapters map it to `claude --effort <level>` or `codex -c model_reasoning_effort="<level>"`. Levels: `low|medium|high|xhigh`, plus `native` (the runtime's own default).
- profile-effort: under Recommended (`recommended_v3`) each role has a default level — Researcher, Architect, both reviewers and EngineeringLead `high`; Implementer `medium`; Simplifier `low` — stated as `Provisional` in the profile's provenance until an effort sweep replaces it. A uniform `--provider` run has no profile policy and stays native.
- effort-override: `--effort <level>` sets every role; `--effort role=level[,role=level]` names some roles (snake_case, as `status` prints them) and leaves the rest to the profile; `--effort native` opts every role out.
- surfaces: `status` prints `effort=<requested> [→ <observed>]` per stage and Usage lines per provider; the TUI technical view (`i`) shows the same evidence.

## How to get to it (user POV)
Omit `--effort` to take the routing profile's per-role levels, or add it to any workflow command to override them; the TUI composer's Effort field cycles profile default / native / low / medium / high / xhigh for every role. Read the sealed level per role in `senate status <run-id>` under Routing (`effort=`), and usage and latency in the Stages and Usage sections or in the TUI with `i` on the run detail screen. Eval results also record `requested_effort`.

## Driving it
```bash
senate standard "<task>"                                   # profile levels: planner high, implementer medium, simplifier low
senate standard "<task>" --effort high                     # every role high
senate deep "<task>" --effort architect=xhigh              # one role raised, the rest from the profile
senate standard "<task>" --effort implementer=low,simplifier=low
senate standard "<task>" --effort native                   # every role native, pre-effort behaviour
senate eval run --suite role_core_v3 --provider fake --effort medium
senate status <run-id>
```
TUI: `i` on run detail toggles technical details; the composer Effort field cycles profile default / native default / low / medium / high / xhigh with ←/→.

## Where it lives
- `src/domain/effort.rs` — `EffortSetting` (`NativeDefault | Level(Low|Medium|High|XHigh)`), `EffortLevel` (ordered).
- `src/cli/commands.rs` — `parse_effort` (the `--effort` grammar; unknown words and roles fail closed), `parse_effort_level`, `parse_role`.
- `src/app/routing.rs` — `EffortRequest` (`ProfileDefault | Uniform | PerRole`), `RecommendedEffort` rows in `RECOMMENDED_V3_PROVENANCE`, `recommended_effort`, `ResourcePlan` (`effort`, `efforts`, `from_snapshot`), schema v3 emission in `resolve_config`.
- `src/tui/state.rs` — `EFFORT_CHOICES`, `effort_label`.
- `src/app/provider_factory.rs` — adapter cache keyed by `(ExecutionTarget, EffortSetting)`, `with_effort`.
- `src/providers/claude/command.rs`, `src/providers/codex/command.rs` — native flag mapping.
- `src/app/query.rs` — `UsageSummary`, `StageExecutionEvidence`, `RunUsage`; folded at read time from events.
- `src/domain/event.rs` — `ProviderUsageUpdated` with optional native dimensions and `native_models`.
- `src/cli/commands.rs` — `usage_lines` (never sums across providers).
- `src/tui/render.rs` — `technical_row("Effort", ...)`.

## Gotchas
- `NativeDefault` is not `medium`; `--effort native` (or omitting it under `--provider`) keeps every native invocation byte-identical to pre-effort behavior and emits the pre-M13b schema v2 payload. Omitting it under Recommended now seals the profile's levels under schema v3.
- What `native` resolves to lives in the CLIs' own files (`~/.claude/settings.json` per-model `effortLevel`, `~/.codex/config.toml` `model_reasoning_effort`), which The Senate never reads; on a machine where those say `medium` and `xhigh`, a native run has its executor at the highest effort and its planner at medium.
- The profile's levels are per role, not per provider: with one native provider ready every role falls back to it and keeps its level on that runtime's own scale. Claude `medium` and Codex `medium` are not the same dial.
- `--effort role=level` names only roles the workflow can route; `architect=…` on `fast` and `verifier=…` anywhere are refused before anything is sealed.
- `max` is a Claude-only level and is refused: The Senate states no level it cannot hand to every native runtime.
- Old v1/v2 config snapshots decode to `NativeDefault` for every role; unknown or malformed effort values and a resource plan smuggled into v2 fail closed.
- Neither native CLI confirms applied effort; `observed` is only shown when the runtime reports something, and The Senate never invents it.
- Never compare Claude and Codex usage units; comparable dimensions are latency, invocation count, injected prompt bytes and eval pass/fail.
- Injected prompt bytes exclude everything the runtime reads on its own (repository, CLAUDE.md/AGENTS.md, MCP, skills, system prompts).
- Telemetry never feeds routing, retries, permissions or effort; there is no escalation yet. Retry-with-higher-effort is deferred (M13b.1) and is meant to mirror the per-stage route override (`retry --provider`).
