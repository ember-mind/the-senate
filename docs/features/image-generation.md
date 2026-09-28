# Image generation (`--allow-image-generation`, `__image-tool`)

Let the Implementer generate original PNG images into the run's worktree through a Senate-owned tool whose backend is the user's own Codex CLI and its built-in `image_gen` tool (native ChatGPT auth, no API key), while the PNG stays an ordinary worktree change.

## Sub-features
- authorization: `--allow-image-generation` on any workflow command seals an `image_generation` block in config schema v4 (`roles: ["implementer"]`, `max_generations: 4`); every run without the flag keeps its byte-identical v2/v3 payload and no tool. Resume, retry, fix and continue read the grant from the snapshot only.
- tool surface: one MCP tool, `image_generate(prompt, output_path, size?, quality?, transparent_background?)`, exposed to the granted role's native CLI only. Claude sees it as `mcp__senate_image__image_generate` (pre-allowed under `dontAsk`); Codex sees `image_generate` on server `senate_image`.
- run-scoped MCP: Claude gets `--mcp-config <json>` + `--allowedTools mcp__senate_image__image_generate`; Codex gets `-c mcp_servers.senate_image.command/args/tool_timeout_sec` root overrides plus `tools.image_generate.approval_mode="approve"`, without which `--ask-for-approval never` refuses every MCP call. Nothing is written to `~/.claude`, `~/.codex`, or the project; the user's own MCP servers stay configured (no `--strict-mcp-config`).
- backend and credential boundary: no vendor API and no API key. The `ImageToolHost` inside the Senate process runs `codex --sandbox read-only --ask-for-approval never exec --json -` in a scratch directory with the request on stdin, lets Codex's built-in `image_gen` tool generate, then collects the single PNG Codex wrote under `$CODEX_HOME/generated_images/<thread-id>/` (never a path the model typed). The native CLI running the stage launches `senate __image-tool --socket <path>` as a stdio MCP shim, which relays one JSON line per call over a `0600` Unix socket to that host. Nothing credential-shaped exists in argv, stdin, MCP config, the process spec, the snapshot, or the evidence.
- bound: 4 generations per run (all Implementer stages together), counted from insert-only `image_generations` rows, so a restart cannot reset it; call N+1 is a typed `limit_reached` tool error.
- placement: `output_path` is agent input. Relative only, plain components, no `.git`, lowercase `.png`, deepest existing ancestor canonicalized under the canonical worktree (symlink escapes refused), never overwrites (temp file, fsync, hard-link to the final name). Parents are created only at write time, after the vendor call succeeded.
- evidence: `image_generations` row per image (stage, attempt, ordinal, backend, model, worktree-relative path, SHA-256, size, prompt SHA-256, request id, timestamps) plus a run-private `runs/<run>/image-generations/NNN.json` carrying the prompt. `senate status` lists them under "Image generations".
- lifecycle: the PNG is an untracked binary file in the managed worktree; the diff preview marks it `binary`, the review handoff names the path without its bytes, apply installs the exact bytes, discard removes it with the worktree.
- doctor: one line, `image generation: available (backend Codex CLI <version> built-in image_gen, native auth via ...)` or `unavailable (<reason>; only needed with --allow-image-generation)`.

## How to get to it (user POV)
Have `codex` installed and logged in (`codex login`), add `--allow-image-generation` to a workflow command, and ask for an image in the task. The Implementer may be Claude or Codex; the image backend is always the local Codex CLI. The Implementer decides whether and where to generate. Inspect the run as usual; the PNG shows up in the diff preview and is applied or discarded with everything else. The TUI composer does not expose the flag yet.

## Driving it
```bash
senate standard "Build a landing page with an original hero image under assets/" --provider claude --allow-image-generation
senate standard "<task>" --provider codex --allow-image-generation
senate status <run-id>
senate doctor
senate __image-tool --socket /tmp/pcimg-<run-id>.sock
```
`__image-tool` is launched by the native CLI, not by people; run it by hand only to debug the shim.

## Where it lives
- `src/image/mod.rs` — `ImageGenerator` trait, `ImageRequest`, `GeneratedImage`, `ImageBackendError`; boundary diagram.
- `src/image/codex.rs` — `CodexImageGenerator`: `codex exec --json` driving the built-in `image_gen` tool; `thread_from_events`, `collect_output`; `backend_available` for doctor and run creation.
- `src/image/fake.rs` — `FakeImageGenerator`: deterministic PNG per prompt, request log, scripted failures.
- `src/image/service.rs` — `ImageToolService`: role check, bound, path validation, PNG validation, atomic write, evidence row + prompt file; `ImageToolCall`, `ImageToolError`, `ImageToolErrorCode`.
- `src/image/path.rs` — `validate_output_path`, `ValidatedOutput::write_no_overwrite`.
- `src/image/host.rs` — `ImageToolHost`: per-run socket (`pcimg-<run-id>.sock` under the temp dir), `activate`/`deactivate`, `server_command`.
- `src/image/mcp.rs` — the stdio MCP server (`initialize`, `tools/list`, `tools/call`, `ping`) and the tool schema.
- `src/app/routing.rs` — `ImageGenerationPlan` (`disabled`, `implementer_only`, `from_snapshot`), schema v4, `resolve_config_with_image`, `DEFAULT_MAX_IMAGE_GENERATIONS`.
- `src/app/provider_factory.rs` — `config_for_new_run_with_image` (creation-time credential check), `start_image_host`, `RoutedProvider::with_image_tool`, runtime cache keyed by `(target, effort, granted)`.
- `src/providers/claude/command.rs`, `src/providers/codex/command.rs` — `mcp_config_json`, `image_tool_rule`, `mcp_overrides`.
- `src/providers/claude/mod.rs`, `src/providers/codex/mod.rs` — `with_image_tool`, `arm_image_tool` on every poll, prompt addendum.
- `src/providers/stage_prompt.rs` — `image_tool_section`.
- `src/store/image.rs`, `src/store/migrations.rs` — `image_generations` table (schema v7), insert/list/count.
- `src/app/query.rs` — `ImageGenerationSummary` on `RunDetails`.
- `src/cli/mod.rs`, `src/cli/commands.rs` — the flag, the hidden subcommand, the doctor line, the status section.

## Gotchas
- Image generation is not a provider and not a role. Routing, effort and workflow semantics are untouched; the plan only says which role may use the tool and how often.
- Reviewers never get the tool, and they never see the image's pixels: the change handoff names the file as a binary change. Nothing in The Senate has visually reviewed a generated image; do not read "reviewed" into a passing QualityReview.
- Enabling the tool changes nothing about the stage's environment; the ordinary forwarding is untouched and The Senate injects nothing.
- A run resumed in a process where Codex is missing or logged out still hosts the tool; calls fail typed as `backend_not_configured`. A run started with the flag while Codex is unavailable is refused before anything is persisted.
- Each generation is one extra `codex exec` session on the user's ChatGPT plan, outside any Senate run/stage record; only the resulting PNG and the evidence row are the Senate's. Codex does not report which image model its tool used, so evidence says `codex/image_gen`.
- Codex's own `imagegen` skill may make a Codex Implementer generate images natively, bypassing this tool, bound and evidence; the prompt note tells it to use `image_generate` instead, but that is instruction, not enforcement.
- While no The Senate process drives the run (it exited, the agent kept working in tmux), calls fail as `backend_unreachable`; the agent is told to continue without the image. Nothing is retried on its behalf.
- Concurrent tool calls from one agent turn are serialized in the host; the second waits, it does not double-spend the bound.
- Codex's native tool timeout is 60 s; the override raises it to 300 s for this server only. Claude's default MCP timeout is already longer.
- Only lowercase `.png`, only generation (no edits, no variations), only one image per call. Cost accounting is invocation count; no monetary estimate is invented.
- Generation time is dominated by Codex (about one to three minutes); the Codex Implementer's MCP tool timeout is raised to 300 s for this server only.
