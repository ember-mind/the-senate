# Process supervision (tmux, __run-process, __exec-process)

Keep every native provider invocation alive and observable even when the Senate frontend goes away, with durable output and exit evidence.

## Sub-features
- managed-process: one persisted attempt per external command, keyed `(run, stage, attempt, invocation)`, with `spec.json`, `stdin.jsonl`, `runtime.json`, `stdout.log`, `stderr.log`, `exit.json` under `~/.senate/runs/<run-id>/processes/<process-id>/`.
- tmux-backend: isolated tmux server per process; launch is `senate __run-process <manifest>` as direct argv, never a shell string.
- runner: `__run-process` validates manifest and ownership, receives forwarded environment over a `0600` Unix socket, clears the environment, spawns `senate __exec-process <manifest>` in its own process group, redirects output to append-only files, writes `runtime.json` then `exit.json`.
- exec-bridge: `__exec-process` resets tmux's ignored SIGINT to default and then `exec`s the provider, so Ctrl-C works on macOS and Linux.
- cursors: SQLite owns acknowledged byte offsets per stream; reads never advance them.
- reconciliation: process status derived from owned session, runtime and exit evidence (`Running`, `Exited`, `Interrupted`, `Missing`, `Broken`, `Cleaned`).
- interrupt: `stop` validates ownership, pane PID, fingerprint and process group, then climbs a termination ladder over the group — SIGINT for 5s, SIGTERM for 3s, SIGKILL for 1s — stopping at the first rung that settles the process.

## How to get to it (user POV)
You do not call this directly. Every native stage goes through it. What you see: `process=` in `senate status`, raw log tails in the TUI (`l`), and the fact that quitting the TUI leaves the provider running. `senate doctor` reports whether tmux is available.

## Driving it
```bash
senate doctor                       # tmux: available (<version>)
senate status <run-id>              # process=<status> per stage
senate stop <run-id>                # interrupts active managed processes
```
Hidden internals, invoked only by the backend:
```bash
senate __run-process <manifest.json>
senate __exec-process <manifest.json>
```
TUI: `l` on run detail opens the bounded raw log tail (read-only, no cursor acknowledgement).

## Where it lives
- `src/process/backend.rs` — `ProcessBackend` contract.
- `src/process/tmux.rs` — `TmuxBackend`, `OWNER_PROCESS_ENV`, `OWNER_FINGERPRINT_ENV`, 8 MiB max read.
- `src/process/runner.rs` — `run_managed_process`, `exec_managed_process`.
- `src/process/environment.rs` — safe env allowlist, socket handoff (`SENATE_ENVIRONMENT_SOCKET`, 1 MiB cap).
- `src/process/manager.rs` — intent/effect/finalize, reconciliation, `interrupt`.
- `src/process/model.rs` — `LaunchManifestV1`, spec/status/output/exit records.
- `src/store/process.rs` — process rows and per-stream cursor CAS.
- `src/bin/senate-test-agent.rs` — fixture executable (`success`, `slow`, `stderr`, `fail-42`, `partial`, ...) used by process tests.
- `tests/process_tmux.rs` — exact argv/env safety, crash windows, replay without ack, foreign session collision, detached survival.

## Gotchas
- Absence never implies success: a missing session without valid `exit.json` is `Missing`, and adapters map that to semantic interruption only after a native session identity exists.
- tmux sessions survive client detachment but not reboot or tmux server loss; recovery is explicit (`resume`).
- Foreign tmux sessions with colliding names are neither reused nor killed; ownership needs both env markers to match persisted identity.
- Corrupt pre-existing exit evidence blocks launch and marks the process `Broken`.
- Cleanup retains all process files; `stop` interrupts only and never cleans, because recovery needs the records.
- Ctrl-C alone is not a stop. Agent CLIs forward it, but worker pools they spawn (Jest, Vitest) ignore it or outlast any usable wait, so a stop that ended at SIGINT left the pool holding the machine. `InterruptTimeout` now means the whole ladder including SIGKILL was climbed, not that a child declined to listen.
- Secrets never enter argv, tmux environment, manifest, SQLite or durable files; if a provider needs a credential variable that is not on the allowlist, it still arrives via the socket, once.
- Everything here is unix-only (`ProcessError::UnsupportedPlatform` elsewhere).
