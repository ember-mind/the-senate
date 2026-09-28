#!/bin/sh
# The Senate bootstrap installer.
#
# Downloads an official release binary, verifies its SHA-256 against the
# release's own SHA256SUMS, confirms the binary reports the version the release
# claims, installs it into a user-owned directory with an atomic rename, and
# then asks the installed binary to record itself as an official installation
# so `senate update` works later.
#
#   curl -fsSL https://raw.githubusercontent.com/ember-mind/the-senate/main/install.sh | sh
#
# Everything is configured through the environment, because a script run this
# way cannot receive flags:
#
#   SENATE_VERSION=0.1.1     install that exact release instead of the latest
#   SENATE_INSTALL_DIR=DIR   install into DIR instead of ~/.local/bin
#   SENATE_FORCE=1           replace a file at the destination that The Senate
#                              does not recognize as its own installation
#
# This script never uses sudo, never touches a repository, never edits shell
# configuration, and never installs anything it has not verified.

set -eu

REPOSITORY="${SENATE_REPOSITORY:-ember-mind/the-senate}"
# Test seams. They default to the official hosts and exist so the installer can
# be exercised offline against local fixtures.
API_BASE="${SENATE_API_BASE:-https://api.github.com}"
DOWNLOAD_BASE="${SENATE_DOWNLOAD_BASE:-https://github.com}"
CHECKSUM_ASSET="SHA256SUMS"

STAGING=""
WORK=""

cleanup() {
    [ -n "$WORK" ] && rm -rf "$WORK"
    [ -n "$STAGING" ] && rm -f "$STAGING"
    return 0
}
trap cleanup EXIT INT TERM

fail() {
    echo "senate install: $*" >&2
    exit 1
}

# --- platform -----------------------------------------------------------
# Maps a detected platform to one of the asset names the release workflow
# publishes. Kept in one place, and cross-checked against the Rust updater by
# tests so the two cannot drift apart.
detect_asset() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) echo "senate-aarch64-apple-darwin" ;;
                x86_64) echo "senate-x86_64-apple-darwin" ;;
                *) fail "unsupported macOS architecture: $arch" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64) echo "senate-x86_64-unknown-linux-gnu" ;;
                *) fail "unsupported Linux architecture: $arch (official builds are x86_64 only)" ;;
            esac
            ;;
        *)
            fail "unsupported operating system: $os (The Senate publishes macOS and Linux builds)"
            ;;
    esac
}

# --- prerequisites ------------------------------------------------------
require_tools() {
    command -v curl >/dev/null 2>&1 || fail "curl is required"
    command -v mktemp >/dev/null 2>&1 || fail "mktemp is required"
    if command -v sha256sum >/dev/null 2>&1; then
        CHECKSUM_TOOL="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        CHECKSUM_TOOL="shasum"
    else
        # Verification is mandatory, so a missing tool stops the install
        # rather than downgrading to an unverified download.
        fail "sha256sum or shasum is required to verify the download"
    fi
}

digest_of() {
    if [ "$CHECKSUM_TOOL" = "sha256sum" ]; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

fetch() {
    # Fail on HTTP errors, follow redirects, bound the time spent, and retry a
    # few times for transient failures.
    curl -fsSL --retry 3 --retry-delay 1 --connect-timeout 10 --max-time 300 \
        -o "$2" "$1"
}

# --- release selection --------------------------------------------------
# Canonical The Senate tags are v followed by a plain semver release; the
# updater ignores anything else, so the installer refuses it too.
canonical_tag() {
    printf '%s' "$1" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || return 1
    printf '%s' "$1"
}

resolve_tag() {
    if [ -n "${SENATE_VERSION:-}" ]; then
        # Accept 0.1.1 or v0.1.1 and canonicalize to the tag form.
        candidate="$SENATE_VERSION"
        case "$candidate" in v*) ;; *) candidate="v$candidate" ;; esac
        canonical_tag "$candidate" ||
            fail "SENATE_VERSION=$SENATE_VERSION is not a stable release version (expected MAJOR.MINOR.PATCH)"
        return 0
    fi
    # /releases/latest is GitHub's newest published, non-draft, non-prerelease
    # release — the same set the updater considers. The tag is still required
    # to be canonical before it is used.
    latest="$WORK/latest.json"
    fetch "$API_BASE/repos/$REPOSITORY/releases/latest" "$latest" ||
        fail "could not reach the release API for $REPOSITORY"
    tag="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$latest" | head -n 1)"
    [ -n "$tag" ] || fail "no published release found for $REPOSITORY"
    canonical_tag "$tag" ||
        fail "latest release tag $tag is not a canonical The Senate release tag"
}

# --- destination --------------------------------------------------------
# Refuses to overwrite anything The Senate does not recognize as its own
# installation. Classification is done by the freshly verified binary, so no
# untrusted file at the destination is ever executed to make this decision.
check_destination() {
    destination="$1"
    verified="$2"
    [ -e "$destination" ] || [ -L "$destination" ] || return 0
    if [ "${SENATE_FORCE:-}" = "1" ]; then
        echo "Replacing existing file at $destination (SENATE_FORCE=1)."
        return 0
    fi
    [ -f "$destination" ] || fail "$destination exists and is not a regular file"
    source="$("$verified" __install-source "$destination" 2>/dev/null || echo unknown)"
    if [ "$source" = "official binary" ]; then
        return 0
    fi
    fail "$destination already exists and is not a Senate installation managed by The Senate ($source).
  Re-run with SENATE_FORCE=1 to replace it, or set SENATE_INSTALL_DIR to install elsewhere."
}

# --- install ------------------------------------------------------------
main() {
    require_tools
    asset="$(detect_asset)"
    WORK="$(mktemp -d "${TMPDIR:-/tmp}/senate-install.XXXXXX")"

    tag="$(resolve_tag)"
    version="${tag#v}"
    install_dir="${SENATE_INSTALL_DIR:-$HOME/.local/bin}"
    target="$install_dir/senate"

    echo "Installing The Senate $version ($asset)"

    base="$DOWNLOAD_BASE/$REPOSITORY/releases/download/$tag"
    fetch "$base/$asset" "$WORK/$asset" ||
        fail "could not download $asset from release $tag"
    fetch "$base/$CHECKSUM_ASSET" "$WORK/$CHECKSUM_ASSET" ||
        fail "release $tag publishes no $CHECKSUM_ASSET manifest"

    expected="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1; exit }' \
        "$WORK/$CHECKSUM_ASSET")"
    [ -n "$expected" ] || fail "$CHECKSUM_ASSET lists no entry for $asset"
    printf '%s' "$expected" | grep -Eq '^[0-9a-fA-F]{64}$' ||
        fail "$CHECKSUM_ASSET entry for $asset is not a SHA-256 digest"

    # Digests are compared case-insensitively: sha256sum and shasum both emit
    # lowercase, but a manifest is external input.
    expected="$(printf '%s' "$expected" | tr 'A-F' 'a-f')"
    computed="$(digest_of "$WORK/$asset" | tr 'A-F' 'a-f')"
    [ "$computed" = "$expected" ] ||
        fail "checksum mismatch for $asset
  expected $expected
  computed $computed"

    chmod 755 "$WORK/$asset"

    # The binary must identify itself as the release being installed, before
    # anything at the destination is touched.
    reported="$("$WORK/$asset" --version 2>/dev/null | head -n 1 | awk '{ print $2 }')"
    [ "$reported" = "$version" ] ||
        fail "downloaded binary reports version ${reported:-unknown}, but release $tag claims $version"

    check_destination "$target" "$WORK/$asset"

    mkdir -p "$install_dir" || fail "could not create $install_dir"
    # Staging inside the destination directory keeps the final rename on one
    # filesystem, which is what makes it atomic. The existing executable is
    # never truncated or removed first.
    STAGING="$install_dir/.senate.install.$$"
    cp "$WORK/$asset" "$STAGING" || fail "could not stage the binary in $install_dir"
    chmod 755 "$STAGING"
    mv "$STAGING" "$target" || fail "could not install into $target"
    STAGING=""

    "$target" --version >/dev/null 2>&1 ||
        fail "installed binary at $target is not runnable"

    # The installed binary writes its own receipt, so the schema, the data
    # directory rules, and the path canonicalization all come from one Rust
    # implementation instead of being reproduced here.
    # Registration is what makes `senate update` able to replace this binary
    # later. A failure here leaves a perfectly working Senate, so it is
    # reported rather than treated as an install failure.
    if "$target" __register-official-install "$target" --asset "$asset" >/dev/null 2>&1; then
        registered=1
    else
        registered=0
    fi


    echo
    echo "The Senate $version installed successfully."
    echo
    echo "Location:"
    echo "  $target"
    # Being on PATH is not the same as winning PATH: an older senate earlier
    # in the search order would keep answering. Resolve what the shell would
    # actually run and say so when it is not what we just installed.
    resolved="$(command -v senate 2>/dev/null || true)"
    if [ -z "$resolved" ]; then
        echo
        echo "Add The Senate to PATH:"
        echo "  export PATH=\"$install_dir:\$PATH\""
    elif [ "$resolved" = "$target" ]; then
        echo
        echo "Run:"
        echo "  senate doctor"
    else
        echo
        echo "Warning: your shell currently resolves \`senate\` to:"
        echo "  $resolved"
        echo
        echo "Put $install_dir before that directory in PATH,"
        echo "or remove the older installation."
    fi
    if [ "$registered" -eq 0 ]; then
        echo
        echo "Note: this installation could not be registered for automatic updates."
        echo "  The Senate works normally; \`senate update\` will report that automatic"
        echo "  installation is unavailable until it is reinstalled."
    fi
}

main "$@"
