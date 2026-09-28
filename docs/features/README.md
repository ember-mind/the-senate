# Feature Map

This directory tells a coding agent what each user-facing The Senate feature does, how a user reaches it, the exact commands and keys that drive it, where it lives in code, and its known traps. It is not an architecture document; see `../../ARCHITECTURE.md` for that.
Open the relevant feature file before touching or driving that feature.
Every command, flag and key here is copied from `src/cli/mod.rs` and `src/tui/input.rs`; if they disagree with a file below, the code is right and the file is stale.

## Features

| Feature | Purpose | File |
|---|---|---|
| Run lifecycle | Start, inspect, stop, resume, recover, retry and resolve one run | [run-lifecycle.md](run-lifecycle.md) |
| Missions | Plan a multi-package project above runs, with dependencies, decisions, and integration | [missions.md](missions.md) |
| Workflows | Built-in Fast/Standard/Deep/Review graphs plus fix and continue cycles | [workflows.md](workflows.md) |
| Control room | Ratatui TUI: screens, overlays, exact keys | [control-room.md](control-room.md) |
| The Senate | `senate world`: local-only HTTP server and browser 3D view of a campaign | [world.md](world.md) |
| Workspace | Isolated worktrees, apply, discard, pull request | [workspace.md](workspace.md) |
| Verification | Run the repository's own checks after the last edit; failed checks block completion and apply, not `pr` | [verification.md](verification.md) |
| Routing | Roles to providers/models, `--provider`, `--profile recommended`, v1 frozen / v2 current, `retry --provider` per-stage override | [routing.md](routing.md) |
| Native providers | Claude Code and Codex adapters, permissions, sandboxes, attention | [providers.md](providers.md) |
| Evaluations | `eval list/run/report`, suites under `evals/`, evidence layout | [evaluations.md](evaluations.md) |
| Observability and effort | Usage/latency/prompt-bytes telemetry and `--effort` | [observability-and-effort.md](observability-and-effort.md) |
| Image generation | `--allow-image-generation`: the Implementer generates PNGs into the worktree through a Senate-owned OpenAI-backed tool | [image-generation.md](image-generation.md) |
| Configuration and appearance | Data/config paths and the environment variables the TUI reads | [configuration.md](configuration.md) |
| Install, update, doctor | Bootstrap installer, self-update, environment check | [install-update-doctor.md](install-update-doctor.md) |
| Process supervision | tmux-backed managed processes, `__run-process`, `__exec-process` | [process-supervision.md](process-supervision.md) |
| Release | Tagging, the release gate, PR discipline | [release.md](release.md) |

## Keeping this map current

When code under a feature's "Where it lives" paths changes, update that feature file in the same PR. Add a new file and a table row when a new user-facing feature lands; delete both when one is removed.

`cargo test --test feature_map` runs in CI and checks the mechanical part, both ways round. What the map claims must be true: every cited path exists, every `senate ...` line in a `Driving it` block is a command with flags the binary accepts, every key named in control-room.md is bound in `src/tui/input.rs`. And what the binary offers must be claimed: every command in `senate --help`, at every level, is mentioned by some feature file, and every key `src/tui/input.rs` binds is named in control-room.md. So a rename turns the map red, and so does a new command or key nobody wrote a line for.

The semantic part is a reading job. At the end of every milestone, an agent runs this pass:

1. For each feature file, open every path under "Where it lives" and compare with the file: sub-features that exist in code but have no line, lines that describe code that is gone, defaults and status names that changed.
2. For each gotcha, find the code or test that makes it true. If none exists any more, delete the gotcha.
3. Check README.md and ARCHITECTURE.md against the same code; those two drift the same way.
4. Open one PR titled `docs: feature map pass after <milestone>` with only those edits.

A stale entry looks like one of these:

- A command, flag, subcommand or key that `senate --help`, `src/cli/mod.rs` or `src/tui/input.rs` no longer has, or has under a different name.
- A "Where it lives" path that `ls` cannot find.
- A status name, default value, profile version or suite version that the code no longer uses.
- A gotcha describing a bug that has been fixed.
- A sub-feature that exists in code but has no line here.
- New behaviour on an existing command or screen — an extra line in its output, a new sentence in the TUI — with no line in its feature file. The test cannot see this one: nothing was renamed and nothing is missing, the map is simply saying less than the code does. It is the drift the milestone pass exists for.
