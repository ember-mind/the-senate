# Configuration and appearance

Know where The Senate keeps its state and which environment variables change its paths, colors and motion.

## Sub-features
- config-path: `$SENATE_CONFIG_DIR/config.toml`, else `$XDG_CONFIG_HOME/senate/config.toml`, else `$HOME/.config/senate/config.toml`; resolved, printed by `doctor`, but never read or created.
- repo-config: `<repo>/.senate.toml`, read from the run's worktree, else from the source repository the worktree was cut from; its `[verify]` table (`commands`, `timeout_seconds`) by the Verify stage (see verification.md), its `[setup]` table (`commands`, `timeout_seconds`) by workspace preparation (see workspace.md), and its `[permissions]` table (`allow`) by the Claude adapter (see providers.md).
- data-dir: `SENATE_DATA_DIR` relocates the SQLite database (`senate.db`), managed worktrees (`worktrees/`), process data (`runs/<run-id>/processes/`), artifacts, `update.json` and `install.json`. Default `~/.senate`.
- appearance: `NO_COLOR`, `SENATE_THEME` (`vivid` or default `native`), `SENATE_MOTION` (`off`, `reduced`, default), `COLORTERM` (read only to decide whether `vivid` can render).
- update-kill-switch: `SENATE_DISABLE_UPDATE_CHECK=1` stops every network check, typed or automatic.
- diagnostics: `RUST_LOG` directives (default `senate=info`, the unchanged crate target); in TUI mode logs go to a sink so stderr cannot corrupt the screen.

## How to get to it (user POV)
There is no user config file to edit today. A repository may carry `.senate.toml` with a `[verify]` table naming its verification commands and a `[permissions]` table naming the tools every Claude run may use without asking. When the repository is one you cannot commit to, keep that file untracked in your own checkout (list it in `.git/info/exclude`) — runs cut from that checkout read it there. Set environment variables before launching The Senate; the TUI reads appearance variables once at startup. `senate doctor` prints the resolved config path, database path and schema version.

## Driving it
```bash
senate doctor
SENATE_DATA_DIR=/tmp/senate-data senate runs
SENATE_THEME=vivid senate
SENATE_MOTION=reduced senate
NO_COLOR=1 senate
SENATE_DISABLE_UPDATE_CHECK=1 senate update --check
RUST_LOG=senate=debug cargo run -- doctor
```
`<repo>/.senate.toml`:
```toml
[setup]
commands = ["yarn build-packages"]
timeout_seconds = 3600

[verify]
commands = ["cargo fmt --check", "cargo clippy --all-targets", "cargo test"]
timeout_seconds = 1800

[permissions]
allow = ["Bash(yarn jest:*)", "Bash(yarn lint:css:*)", "mcp__linear-server"]
```

## Where it lives
- `src/config/mod.rs` — `config_file` resolution order.
- `src/providers/verify/config.rs` — `.senate.toml` `[verify]` reader and build-file detection.
- `src/providers/claude/permissions.rs` — `.senate.toml` `[permissions]` reader.
- `src/providers/repo_config.rs` — `locate`, the worktree-then-source-repository lookup both readers share, and `ConfigOrigin`.
- `src/store/path.rs` — `database_file`, `worktree_root`, `SENATE_DATA_DIR`.
- `src/store/migrations.rs`, `src/store/sqlite.rs` — schema lifecycle (v1..v5), WAL, busy timeout, insert-only triggers.
- `src/tui/theme.rs` — `NO_COLOR`, `COLORTERM`, `SENATE_THEME`.
- `src/tui/motion.rs` — `SENATE_MOTION`.
- `src/update/mod.rs` — `DISABLE_ENVIRONMENT_VARIABLE`, `CACHE_TTL` (24 h).
- `src/lib.rs` — `init_tracing`.
- `src/process/environment.rs` — `safe_environment_name` allowlist forwarded into managed processes.

## Gotchas
- Path resolution is side-effect free; opening the store creates the directory and database and applies migrations. `runs` and `doctor` never create a missing database.
- `<repo>/.senate.toml` has three readers and they are independent: the Verify stage reads `[verify]`, workspace preparation reads `[setup]`, the Claude adapter reads `[permissions]`. Each tolerates the others' tables and rejects unknown keys inside its own, so a misspelt key fails rather than silently doing nothing. The user config file is still never read.
- Both readers resolve to one file, never a merge of two: the worktree's `.senate.toml` if it exists, otherwise the source repository's. A worktree file carrying only one of the tables therefore hides the source repository's other table as well — keep both tables in whichever file answers.
- An empty `NO_COLOR` is not a request for mono; only present-and-non-empty counts. `TERM=dumb` behaves like `NO_COLOR`.
- `vivid` on a terminal without truecolor falls back to the named ANSI palette; `NO_COLOR` outranks it.
- Read screens (artifact, logs, diff, composer) and every overlay never animate regardless of `SENATE_MOTION`; the variable can only lower what a screen permits.
- Only the allowlist in `safe_environment_name` (HOME, PATH, LANG, TERM, XDG_*, CLAUDE_CONFIG_DIR, GIT_CONFIG_*, SSH_AUTH_SOCK, ...) enters the tmux session; other provider variables cross once through a `0600` Unix socket into runner memory. `SENATE_*` variables set for the parent do not reach the provider.
- Database triggers reject update/delete on run inputs, config snapshots and artifacts; do not try to edit them through SQL.
