# Evaluations

Measure one provider/model candidate on one engineering role against source-controlled cases with deterministic oracles, without touching production runs or routing.

## Sub-features
- validation-bounds: trusted validation preserves the case's exact argv, uses a 1800 s limit per command including output draining, and records bounded 64 KiB tails per stream with omitted-byte counts in `validation.txt`. Timeout is benchmark failure; failure to start or inspect the command is infrastructure failure.
- fixture-git: fixture initialization uses the isolated Git command boundary, excluding ambient Git redirection and Jira importer credentials. The fixture commit disables hooks and GPG signing so global configuration cannot trigger hooks or an interactive signing prompt.
- list: prints each suite version, its fingerprint and cases (`role=`, `workflow=`).
- run: materializes a fresh fixture repository per repetition, drives the normal engine with an `eval_v1` routing plan (candidate on the target role, Fake on every other role), scores, and writes evidence.
- report: aggregates one or more result files/directories by suite version and target; never picks a winner.
- suites: `role_core_v1` (immutable, default), `role_core_v2` (calibrated reviewer fixtures), `role_core_v3` (hygiene successor backing `recommended_v2`).
- statuses: `Passed`, `Failed` (benchmark failure), `InfrastructureFailure` (fixture, provider, tmux, artifact, apply, safety).

## How to get to it (user POV)
Run `senate eval list` to see suites and cases. Run a candidate with `senate eval run`; native providers need `--allow-native-usage` on every invocation. Evidence lands under `~/.senate/evals/<evaluation-id>/<case-id>/rep-NNN/` unless `--out` is given. Read summaries with `senate eval report`. There are no evaluation controls in the TUI.

## Driving it
```bash
senate eval list
senate eval run --provider fake                                   # suite defaults to role_core_v1
senate eval run --suite role_core_v3 --provider fake
senate eval run --suite role_core_v3 --provider codex --repeat 3 --allow-native-usage
senate eval run --suite role_core_v3 --provider claude --model <model> --allow-native-usage --out <dir>
senate eval run --suite role_core_v3 --provider codex --effort medium --repeat 3 --allow-native-usage
senate eval report ~/.senate/evals/<evaluation-id>
senate eval report <codex-results> <claude-results>
```
Flags on `eval run`: `--suite <version>` (default `role_core_v1`), `--provider claude|codex|fake` (required), `--model <id>`, `--effort native|low|medium|high|xhigh` (default native; recorded as `requested_effort` on every result), `--repeat <n>` (default 1), `--allow-native-usage`, `--out <path>`.

## Where it lives
- `src/providers/verify/runner.rs`, `src/git/command.rs` — bounded command execution and isolated fixture Git.
- `src/cli/mod.rs` — `EvalCommand`, `EvalRunArgs`.
- `src/cli/commands.rs` — `eval`, `run_eval` (per-case progress lines and ✓/✗/! marks).
- `src/eval/case.rs` — cases and ground truth; fixture files embedded with `include_str!` from `evals/`.
- `src/eval/suite.rs` — `EvalSuite::load`, fingerprints.
- `src/eval/runner.rs` — isolated roots, `allow_native_usage` gate, `default_output_directory`.
- `src/eval/scorer.rs` — deterministic implementer/quality/spec scoring.
- `src/eval/result.rs`, `src/eval/report.rs` — `EvalResultV1` codec and report rendering.
- `evals/role_core_v1/`, `evals/role_core_v2/`, `evals/role_core_v3/` — fixture repositories (`Cargo.toml`, `src/lib.rs`, `.gitignore`, sometimes `Cargo.lock`).
- `tests/eval_cli.rs` — list output, native opt-in gate, synthetic Fake results, controlled Codex run.

## Gotchas
- The final arbiter of an eval is the harness (diff scope, trusted validation, structured artifact matching), never the agent's self-reported success. A successful terminal with only denied Bash/read history completes and is then scored; only a mutation request, a question or an unknown tool needs a human (`requires_eval_terminal_attention`).
- `--allow-native-usage` consent is never stored; omit it and a native run stops before any output directory is created.
- Fake results carry `synthetic = true` and cannot support Recommended policy.
- Eval runs never open `~/.senate/senate.db` and never appear in `senate runs`; deleting `~/.senate/evals` has zero production effect.
- V1, V2 and V3 result groups are never averaged together in reports.
- Fixture files are compiled into the binary via `include_str!`; editing `evals/` changes the suite fingerprint, and `role_core_v1`/`v2` are meant to stay byte-identical history.
- Architect, Researcher and EngineeringLead have no cases on purpose.
- Reviewer candidates run with the change handoff since M13a.5; pre- and post-handoff native result sets are not perfectly comparable even though the suite fingerprint is unchanged.
