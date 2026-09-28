# Native providers (Claude Code, Codex CLI, Fake)

Run each stage through the locally installed `claude` or `codex` executable with its own authentication and configuration, or through the deterministic Fake provider for tests.

## Sub-features
- claude: non-interactive stream-JSON print mode, `dontAsk` permissions, prompt on immutable stdin; denied tools become typed permission attention; questions (`AskUserQuestion`) need `--response`.
- claude-baseline-allowlist: every Claude invocation starts from a fixed baseline (`BASELINE` in `src/providers/claude/permissions.rs`) whatever the repository says — reading the tree (`grep`, `rg`, `find`, `ls`, `cat`, `head`, `tail`, `sed`, `awk`, `cut`, `tr`, `sort`, `uniq`, `wc`, `diff`, `jq`, `echo`, `printf`, `basename`, `dirname`) plus the reporting-only `git` subcommands (`status`, `log`, `show`, `diff`, `grep`), and changing it (`Edit`, `MultiEdit`, `Write`). Nothing that installs, builds, runs a package script or reaches the network is in it: `yarn`, `npm`, `npx`, `node`, `python3`, `gh` and the rest stay a per-repository decision. Without this every run in every repository re-paid the same denials before it could look at anything.
- claude-repo-allowlist: the `[permissions] allow` table of `<worktree>/.senate.toml` — or of `<source repo>/.senate.toml` when the worktree carries no such file — becomes `--allowedTools` on every Claude invocation, initial and resumed, beside the baseline above and anything a resolved attention granted. Rules are native Claude Code rules, passed through verbatim; a rule that grants every tool (`*`, `Bash(*)`) or an empty one fails the stage.
- codex: `codex exec --json` with prompt `-` on stdin, `--ask-for-approval never`, sandbox `read-only` for non-mutating stage kinds and `workspace-write` for Implementation/Fix/FollowUp; no typed attention.
- codex-dead-process-completion: a `turn.completed` whose process then died (non-zero exit, signal, or vanished) completes only if the `--output-last-message` file equals the last agent message in the retained stream; otherwise the record is consumed and the stage becomes a recoverable interruption. A requested stop reports interruption without consulting the file, and a `broken` supervisor still fails the poll.
- fake: scripted signals (start, progress, usage, attention, pause, interruption, completion, failure) without editing files; scenario `development_fake/default_success_v1`.
- verify: provider id `verify`, a deterministic command runner serving only `Role::Verifier`; routed implicitly, never chosen by `--provider` or a profile, never a provider session (see verification.md).
- permission-continuation: `resolve` reconstructs the exact denial from retained output, converts it to a native `--allowedTools` rule, and resumes the same Claude session UUID in a new managed invocation.
- artifacts: `~/.senate/runs/<run-id>/artifacts/<stage-id>.md`, SHA-256 verified before use; downstream prompts get direct dependency artifacts only.
- doctor: reports CLI versions, auth status and suspicious credential environment variable names.

## How to get to it (user POV)
Install and log into `claude` and/or `codex` natively, confirm with `senate doctor`, then start runs with `--provider claude`, `--provider codex`, or Recommended. When a Claude stage stops with `needs_user`, read the attention line in `status` and answer with `resolve`. Codex has no attention path; a native denial fails the stage and you `retry`.

## Driving it
```bash
senate doctor
senate fast "<task>" --provider claude
senate fast "<task>" --provider codex
senate fast "<task>" --provider fake
senate resolve <run-id> <attention-id>                      # approve exact permission
senate resolve <run-id> <attention-id> --response "<text>"  # answer AskUserQuestion
SENATE_REAL_CLAUDE=1 cargo test --test claude_real -- --ignored --nocapture
SENATE_REAL_CODEX=1 cargo test --test codex_real -- --ignored --nocapture
```
TUI: `u` opens the attention overlay; ↑/↓ choose the request, type an answer if it is a question, Enter resolves.

## Where it lives
- `src/providers/claude/` — `detection.rs` (install/auth discovery), `command.rs` (argv, `--resume`, `--allowedTools`, `--effort`), `permissions.rs` (`.senate.toml` `[permissions]` reader), `protocol.rs` (JSONL decoder, `PermissionDenial`, terminal-attention rules), `prompt.rs`, `artifact.rs`, `mod.rs` (adapter).
- `src/providers/codex/` — `detection.rs`, `command.rs` (`exec --json`, sandbox, `-c model_reasoning_effort`), `protocol.rs`, `session_meta.rs`, `mod.rs`.
- `src/providers/session.rs`, `src/providers/checkpoint.rs`, `src/providers/artifact.rs` — provider session, atomic commit payload, immutable artifact record.
- `src/engine/fake.rs` — Fake scenarios.
- `src/providers/verify/` — the `verify` provider (`mod.rs` adapter, `config.rs`, `runner.rs`, `artifact.rs`).
- `src/engine/provider.rs` — provider-neutral `ProviderRequest`/`ProviderPoll` boundary.
- `src/store/provider.rs` — provider-session and artifact persistence.
- `tests/routing_cli.rs` — `recommended_attention_restart_routes_response_to_same_claude_session`.
- `tests/codex_cli.rs`, `tests/claude_real.rs`, `tests/codex_real.rs`.

## Gotchas
- `permission_denials` in a Claude result is per-process, not per-session: every retry after a `--resume` gets a new `tool_use_id`. A list that looks cumulative across the resume boundary is not; treating it as residual history misclassifies a fresh attempt. Split by `tool_use_id` (`PermissionDenial::same_request`).
- "Safe to auto-approve" and "denial blocks completion" are two different policies (`exact_rule` vs `requires_terminal_attention`); when they were one function, successful results were marked infrastructure failures or `NeedsUser`. Only Edit/Write/MultiEdit/NotebookEdit, AskUserQuestion, unknown tools and denied *acquisitions* are terminal; a denied local Bash/Read/Glob/Grep/LS never ran, so it is not terminal.
- Mutation is not the only unfinished business a denial leaves. A denied fetch — any `mcp__*` tool, `WebFetch`/`WebSearch`, `gh`/`curl`/`git fetch` under Bash — means the evidence the stage asked for never arrived, and the stage still terminates `"subtype":"success","is_error":false`. `PermissionDenial::is_denied_acquisition` makes those terminal for every stage kind, read-only ones included; before it, run `01M1K8KAJ1HMS47H7WR8YMN2PW` ran all five stages to the end on artifacts that each said "I could not read the pull request". A denial the agent demonstrably worked around is still excused first (`was_superseded`), so this never re-strands the runs that fix healed.
- `grant_rules` decides whether an approval is worth committing and runs at `resolve`, before anything is persisted; building the resume command never refuses again (except an unanswered question). Both gates refusing is what stranded runs: a resolution committed before the gate existed leaves a session whose every later drive fails building the command, and the stage sits `running` for good with no process behind it, reachable only by a `retry` that throws the work away. A resume that can grant nothing says so in its prompt and continues; a session with no denials at all — a stopped stage picking back up — resumes plainly.
- `resume` retries a lost revision race the way `stop` does. Resuming a run something else is still driving is normal, not exceptional: the driver commits on every provider signal it observes, so the resume collides on rows it touched first and used to surface a bare revision number to the user.
- On an editing stage a denied Bash is terminal *unless the invocation's own log shows the agent recovered*: `protocol::superseded_requests` scans that process' stdout for a later `tool_use` of the same tool whose `tool_result` was not an error, and `PermissionDenial::was_superseded` excuses only non-mutating requests on that evidence. A refused Edit/Write or question is never excused this way, an oversized (>8 MiB) or unreadable log yields no evidence and therefore asks, and the scan moves no output cursor. Without it a run stalls on commands no rule can ever grant — a pipeline into an interpreter, a heredoc — after the agent already finished the work another way.
- The Senate never adds `--dangerously-skip-permissions`, `--yolo`, `danger-full-access`, `--ephemeral` or any Git/config bypass. Wildcard or ambiguous permission rules fail closed at `resolve`, and the refusal names why: unreadable shell, a bare redirection, or the rule-syntax character (`*`, `?`, `)`) and the part of the command carrying it.
- A question response goes to run-private stdin, never argv or SQLite event payloads.
- Claude reviewer stages are prompt-prohibited from editing but have no hard sandbox; Codex reviewers do (`read-only`). This asymmetry is documented, not patched.
- Codex `thread.started` supplies the thread id; recovery resumes that exact id, never `--last`. A failed-stage retry creates a new session and thread.
- A retained `turn.completed` is not proof the turn finished: the same bytes survive a killed CLI. Without a clean exit, the proof is the `--output-last-message` file matching the last `item.completed` agent message byte for byte (only a trailing newline may differ, since `artifact::persist` appends it). "File exists and is non-empty" is not enough — a process killed mid-write leaves a readable prefix that would become a hash-verified half-artifact. The file is read only up to the 1 MiB artifact cap, so an oversized one fails corroboration instead of being pulled into memory. The cap counts the trailing newline `artifact::persist` appends, so a file of exactly 1 MiB fits only if it already ends in one. Uncorroborated means interruption, not an error: raising here re-reads and re-rejects the same record on every poll and pins the stage in `running` forever, reachable only by `retry`, which throws the finished work away. Recover with `senate resume <run-id>`.
- Unknown valid JSONL records become non-semantic checkpoints; an invalid complete record fails without advancing the cursor; a partial line waits.
- Claude usage comes from the terminal result record only; per-message usage is discarded on purpose.
- `--provider fake` still verifies for real: the Fake provider fakes agent roles only, and the `verify` stage runs the repository's commands regardless of the selection.
