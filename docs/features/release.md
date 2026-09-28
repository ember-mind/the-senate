# Release

Publish official binaries from a canonical tag whose version matches `Cargo.toml`, verified by the same gate locally and in CI.

## Sub-features
- gate: `__verify-release-tag <tag>` compares `vMAJOR.MINOR.PATCH` against the compiled package version; offline and read-only.
- workflow: `.github/workflows/release.yml` builds each target natively, re-checks every binary's version, writes `SHA256SUMS`, publishes a stable release with four assets.
- assets: `senate-aarch64-apple-darwin`, `senate-x86_64-apple-darwin`, `senate-x86_64-unknown-linux-gnu`, `SHA256SUMS`.
- ci: `.github/workflows/ci.yml` runs fmt, clippy and tests as the quality gate.

## How to get to it (user POV)
Bump `version` in `Cargo.toml`, refresh `Cargo.lock`, run the local checks and the gate, merge to `main`, then tag and push the tag. Watch the release workflow, then verify with a clean install as in `RELEASING.md`.

## Driving it
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --no-fail-fast
cargo run --quiet -- __verify-release-tag v0.1.1
git tag v0.1.1
git push origin v0.1.1
SENATE_VERSION=0.1.1 SENATE_INSTALL_DIR=/tmp/pc-check sh install.sh
/tmp/pc-check/senate --version
/tmp/pc-check/senate doctor
```

## Where it lives
- `RELEASING.md` — the full procedure and end-to-end self-update validation.
- `src/cli/mod.rs` — hidden `VerifyReleaseTag`.
- `src/cli/commands.rs` — `verify_release_tag`.
- `src/update/mod.rs` — `verify_release_tag`; `src/update/installer.rs` — `target_asset_name`.
- `.github/workflows/release.yml`, `.github/workflows/ci.yml`.
- `tests/release_workflow.rs` — the gate refuses every non-matching tag; no job publishes without passing it.

## Gotchas
- No stacked PRs. A PR merged into another feature branch that was itself already merged shows MERGED and green, but its commits are absent from `main`. Open every PR against `main`, and before tagging run `git log main..<branch>` for each branch you believe is in.
- The gate is the authority; a tag that does not equal `Cargo.toml`'s version never reaches publication, locally or in CI.
- Signing and provenance attestation are not done; integrity is checked against `SHA256SUMS` from the same release only.
- A published release is what `senate update` sees; a mistaken stable release is immediately offered to every installed binary.
