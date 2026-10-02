# Install, update, doctor

Get an official binary onto the machine, keep it current with verified self-updates, and check that the local environment can run The Senate.

## Sub-features
- staging-integrity: every self-update uses its own private temporary staging file beside the executable; cleanup cannot remove another update's staging file. A staged binary must exit successfully from `--version` as well as report the expected version before replacement. Self-update and registration version probes require a successful exit, cap stdout/stderr at 64 KiB each, and time out after 5 s including output draining; timeout kills and reaps the process group. Both omit Jira importer credentials. No failed probe replaces a binary or writes a registration receipt.
- install.sh: downloads a release asset, verifies SHA-256 against the release's `SHA256SUMS`, checks `--version`, installs to `~/.local/bin/senate`, then registers the install through a hidden subcommand.
- update-check: automatic at most once per 24 h using public GitHub release metadata, cached in `update.json`; typed commands always check now.
- update-install: staging file beside the target, checksum and version verified, renamed into place; running process keeps the loaded binary.
- install-source: `official binary` installs update automatically; source builds, `cargo install` and package-manager prefixes are reported with the owning command instead.
- doctor: version, install source, update-check state, config/database paths and schema, Claude/Codex availability and auth, suspicious credential env var names, Git, tmux. Offline.
- tui-update-prompt: the Runs screen offers Yes/No when a newer release is installable.

## How to get to it (user POV)
Pipe `install.sh` into `sh` on macOS or Linux x86_64, then run `senate --version` and `senate doctor`. Later, run `senate update --check` to look, or `senate update` to install after a `[y/N]` prompt. Non-interactive stdin is never treated as consent; pass `--yes`.

## Driving it
```bash
curl -fsSL https://raw.githubusercontent.com/ember-mind/the-senate/main/install.sh | sh
SENATE_VERSION=0.1.1 sh install.sh
SENATE_INSTALL_DIR=~/bin sh install.sh
SENATE_FORCE=1 sh install.sh
senate --version
senate doctor
senate update --check
senate update
senate update --yes
export SENATE_DISABLE_UPDATE_CHECK=1
```
Hidden bootstrap hooks used by `install.sh` (not for humans):
```bash
senate __register-official-install <executable> [--asset <name>]
senate __install-source [<executable>]
```
TUI update overlay: `↑`/`↓` toggle Yes/No, `Enter` confirm, `Esc` dismiss.

## Where it lives
- `install.sh` — bootstrap installer.
- `src/cli/mod.rs` — `UpdateArgs` (`--check`, `--yes`), hidden `RegisterOfficialInstall`, `InstallSourceOf`.
- `src/cli/commands.rs` — `update`, `confirm_install`, `install_update`, `doctor`, `print_distribution`, `register_official_install`, `install_source_of`.
- `src/providers/verify/runner.rs` — bounded exact-argv supervision shared by version probes.
- `src/update/mod.rs` — `UpdateService`, `check_now` vs cached check, `detect_install_source`, `OFFICIAL_REPOSITORY`, `CURRENT_VERSION`.
- `src/update/release.rs`, `src/update/cache.rs`, `src/update/install.rs`, `src/update/installer.rs` — GitHub release source, `update.json`, `install.json` receipt, verified install.
- `src/tui/app.rs` — `handle_update_intent`, `begin_update_install`.
- `tests/install_bootstrap.rs` — installer against a fake release (checksum mismatch, missing manifest, forced replace).
- `tests/cli.rs` — `doctor_reports_runtime_prerequisites_without_creating_database`.

## Gotchas
- `update --check` can never reach the installer; a test asserts the CLI calls `check_now`, not the cache-aware entry point (answering a typed check from a day-old cache was a real bug).
- A check sends only a `senate/<version>` user agent; network failure leaves status `unavailable` and is never an error.
- `Automatic installation is unavailable for this build` means `install.json` under the data directory does not name the running executable; rerun `install.sh` to rewrite it.
- The new binary is used at the next start; the running process keeps the old one.
- `doctor` never touches the network and never creates the database; `secret environment` lists variable names only, never values.
- Windows is unsupported (tmux); Linux ARM has no official build.
