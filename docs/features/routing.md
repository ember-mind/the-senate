# Routing

Decide which native coding runtime, and optionally which model, executes each engineering role of a run, once, at creation.

## Sub-features
- uniform: `--provider claude|codex|fake` maps every role in the workflow to that provider.
- recommended: `--profile recommended` (the default when neither flag is given) resolves the current versioned profile, `recommended_v3`, and persists explicit per-role routes plus a requested effort per role (see observability-and-effort.md).
- frozen: runs persisted under `recommended_v1` or `recommended_v2` keep decoding their original routes and native-default effort.
- models: configured model is null unless the immutable config supplies one; confirmed model comes only from provider evidence.
- status: `status` prints the Routing table (role, configured provider, configured model, reason) and per-stage configured vs actual.
- verifier: `Role::Verifier` always resolves to provider `verify` inside the router (`VERIFY_PROVIDER_ID`); it is never written to a snapshot, never in the Routing table, never in a profile or the resource plan.
- retry-override: `retry <run-id> <stage-id> --provider claude|codex|fake [--model <id>]` sends one failed stage to another provider before retrying it. Only that stage moves; the snapshot, the Routing table and every other stage (the descendants the retry un-skips included) keep their routes. The override is stage state: it is recorded as a `StageRouteOverridden` event with reason `operator_override`, persisted in the run snapshot (v3), and it sticks, so a later plain retry of the same stage runs on the override, not on the route that failed. Without `--model` the provider's native default applies, never the model the snapshot pinned for the original route.
- model-fallback: when Codex refuses the model a stage ran on (`... model is not supported when using Codex with a ChatGPT account`, usually a `model = ...` in `~/.codex/config.toml` the account is not entitled to), `status` prints `try: senate retry <run-id> <stage-id> --provider codex --model gpt-5.6-luna` under the reason, and the TUI `t` chooser adds a `Codex on gpt-5.6-luna` row and opens on it. It is an ordinary `retry-override`; The Senate never edits the Codex config. The model is `CODEX_FALLBACK_MODEL` in `src/app/query.rs`, detection is `StageSummary::model_fallback`.

## How to get to it (user POV)
Omit both flags for Recommended, or pass exactly one of `--provider` or `--profile`. The TUI composer's Execution field cycles Recommended / Claude only / Codex only / Fake with ←/→. Read the resolved routes in `senate status <run-id>` under Routing.

When a stage fails because its provider is gone or out of quota, retry it somewhere else: `senate retry <run-id> <stage-id> --provider claude`, or in the TUI press `t` on the failed stage and pick a row in the chooser (Configured provider / Claude / Codex; Enter on the first row is the plain retry). The stage line in `status` then reads `configured=claude/native default (operator override)`, and the TUI runtime line shows `(override)`.

## Driving it
```bash
senate standard "<task>"                          # recommended_v3
senate standard "<task>" --profile recommended
senate standard "<task>" --provider claude
senate standard "<task>" --provider codex
senate standard "<task>" --provider fake
senate status <run-id>
senate retry <run-id> <stage-id> --provider claude            # this stage only, native default model
senate retry <run-id> <stage-id> --provider codex --model o3   # --model needs --provider
```
Current `recommended_v3` map with both providers ready (routes inherited unchanged from `recommended_v2`): Researcher, Architect, Simplifier, CodeQualityReviewer, EngineeringLead, legacy Reviewer -> Claude; Implementer, SpecReviewer -> Codex. Effort column: Researcher, Architect, CodeQualityReviewer, SpecReviewer, EngineeringLead `high`; Implementer `medium`; Simplifier `low`. With one native provider ready, every role routes there with a persisted fallback reason and keeps its level.

## Where it lives
- `src/app/routing.rs` — `RECOMMENDED_PROFILE_VERSION` (`recommended_v3`), `RECOMMENDED_PROFILE_VERSION_V2`, `RECOMMENDED_PROFILE_VERSION_V1`, provenance tables (`decisions` for routes, `efforts` for levels), `recommended_effort`, `resolve_config`, `RoutingPlan`, `unroutable_fix_role`, `VERIFY_PROVIDER_ID`, `RetryRoute`, `OPERATOR_OVERRIDE_REASON`.
- `src/app/provider_factory.rs` — `RuntimeProviderFactory`, `RoutedProvider` lazy adapter cache keyed by target and effort; `RoutedProvider::target_for` consults the request's override before the role's route; `ProviderFactory::require_provider` is the readiness check a retry override passes before anything is committed.
- `src/domain/stage.rs` — `StageRouteOverride`, `Stage::route_override`, `Stage::override_route` (failed stages only); `src/domain/run.rs` — `Run::override_stage_route` (same retry-safety guard as retry); `src/engine/scheduler.rs` — `retry_stage` commits the override and the retry together; `src/store/snapshot.rs` — run snapshot v3 carries it.
- `src/app/query.rs` — `configured_target`: `StageSummary::route_overridden`, `StageExecutionEvidence::route_overridden`.
- `src/tui/state.rs` — `Overlay::RetryRoute`, `RetryRouteChoice`; `src/tui/render.rs` — `render_retry_route`.
- `src/cli/commands.rs` — `start`: flag-to-`ExecutionSelection` mapping; only `recommended` is an accepted profile name.
- `src/store/config_snapshot.rs` — insert-only immutable config records (schema v2 routes, v3 adds resource plan).
- `src/tui/state.rs` — `ExecutionChoice`.
- `tests/routing_cli.rs` — default profile, flag conflict, no Fake fallback, persisted routes surviving provider loss, `retry --provider` moving one stage and refusing a missing provider.

## Gotchas
- `--provider` and `--profile` conflict at clap level (exit 2). Any profile other than `recommended` is rejected before state creation.
- Recommended never falls back to Fake; with no authenticated native CLI it refuses to start (`recommended profile requires authenticated Claude Code or Codex CLI`). Fake must be asked for by name.
- Routes are resolved only at creation; resume, retry, recovery and attention never re-run Recommended. Provider loss afterwards is a clear failure, not a reroute: the only way a stage changes provider is an operator's explicit `retry --provider` (TUI `t` chooser), and that moves one stage, never the run.
- A retry override is refused before any commit when the provider is not installed or authenticated (`require_provider`, same bar as `--provider` at creation), so a refused override leaves the stage failed and retryable on its original route. `--model` without `--provider` is a clap error. Evaluation runs never accept an override.
- The override replaces the whole target, model included. `retry --provider claude` on a stage whose snapshot route pinned a Codex model runs Claude's native default; name `--model` if a specific one is wanted.
- `recommended_v2` evidence is runtime-level (role_core_v3, 3 repetitions) and was taken at native-default effort; it encodes no cost or token claims. `recommended_v3` restates every route as `Inherited` for that reason, and every effort row is `Provisional` (benchmark kind `expert_provisional`) until an effort sweep with `eval run --effort` replaces it.
- The fallback row is withheld when the stage already ran on `CODEX_FALLBACK_MODEL`, so a refused fallback does not loop; bump the constant when ChatGPT's Codex line moves.
- Codex never confirms its model; `actual` model stays `unconfirmed` for Codex stages by design.
- Effort never changes routing; see observability-and-effort.md.
- The verifier's stage line in `status` shows configured provider `verify` although the Routing table above it has no such row; that is the implicit route, not a missing one. Snapshots sealed before the verifier existed load unchanged and can still grow fix cycles.
