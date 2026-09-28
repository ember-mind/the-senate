# Releasing The Senate

The Senate publishes official binaries from a canonical tag. The release workflow's
guard job is the authority on tag correctness: it refuses to build or publish
anything unless the tag is `vMAJOR.MINOR.PATCH` and names exactly the version in
`Cargo.toml`. Nothing below overrides that check.

## Cutting a release

1. Bump `version` in `Cargo.toml` (for example `0.1.0` → `0.1.1`) and refresh
   `Cargo.lock` with `cargo build`.
2. Verify locally:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --no-fail-fast
   ```

3. Confirm the tag you are about to push agrees with the package:

   ```bash
   cargo run --quiet -- __verify-release-tag v0.1.1
   ```

   This is the same check the workflow runs. A mismatch here is a mismatch there.
4. Commit the bump and merge it to `main`.
5. Tag and push:

   ```bash
   git tag v0.1.1
   git push origin v0.1.1
   ```

6. Watch the release workflow. It builds each target natively, re-checks that every
   built binary reports the tagged version, generates `SHA256SUMS` over the final
   assets, and publishes a stable (non-draft, non-prerelease) release.
7. Verify the published release carries four assets:

   ```
   senate-aarch64-apple-darwin
   senate-x86_64-apple-darwin
   senate-x86_64-unknown-linux-gnu
   SHA256SUMS
   ```

8. Install it on a clean machine or a scratch directory:

   ```bash
   SENATE_VERSION=0.1.1 SENATE_INSTALL_DIR=/tmp/pc-check sh install.sh
   ```

9. Confirm the chain closed:

   ```bash
   /tmp/pc-check/senate --version   # 0.1.1
   /tmp/pc-check/senate doctor      # install source: official binary
                                      # automatic update: supported
   ```

## Validating self-update end to end

This exercises the M13e updater against real releases. It needs two published
releases, so run it deliberately.

### Phase A — install the older release

```bash
SENATE_VERSION=0.1.1 sh install.sh
senate --version   # 0.1.1
senate doctor      # install source: official binary
                     # automatic update: supported
```

### Phase B — publish the newer release and update into it

Cut `v0.1.2` with the steps above, then:

```bash
senate update --check
# Current version: 0.1.1
# Update available: 0.1.1 → 0.1.2
# Install source: official binary

senate update      # confirms, downloads, verifies, installs
```

The running process keeps using the binary it already loaded — that is by design.
Start The Senate again:

```bash
senate --version   # 0.1.2
```

The TUI path is equivalent: open `senate`, and on the Runs screen the update prompt
offers `Yes` / `No`. Choosing `Yes` performs the same verified install and reports
that the new version applies at the next start.

### What to check if Phase B fails

- `senate update --check` reporting `Update status is unavailable right now.` means
  the check itself failed — network, rate limit, or a release that is not a canonical
  stable tag. It is never an installation problem.
- `Automatic installation is unavailable for this build` means the receipt at
  `$SENATE_DATA_DIR/install.json` (default `~/.senate/install.json`) does not name
  the running executable. Reinstalling with `install.sh` rewrites it.
- A checksum or version-mismatch error aborts before the executable is touched; the
  existing installation stays usable.

## Not covered yet

Release assets are verified against `SHA256SUMS` published in the same release, so
integrity is checked but provenance is not: the manifest and the binary come from the
same trust boundary. Signing and provenance attestation are deliberately left to a
later milestone.
