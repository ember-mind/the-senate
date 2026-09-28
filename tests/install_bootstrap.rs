//! Bootstrap installer behavior, proven offline.
//!
//! Every test runs the real `install.sh` against a local release fixture
//! served over `file://`, with a scrubbed environment and a fixture `HOME`.
//! Nothing here reaches the network, and nothing touches the developer's own
//! installation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;

use tempfile::TempDir;

const INSTALLER: &str = include_str!("../install.sh");
const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");

/// The release the fixture publishes: this build's own version, so the
/// installed binary genuinely satisfies the installer's version check and can
/// write its own receipt.
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Fixture {
    root: TempDir,
}

impl Fixture {
    /// A published release carrying every official asset plus a matching
    /// SHA256SUMS, so any platform mapping resolves.
    fn new() -> Self {
        let fixture = Self {
            root: TempDir::new().unwrap(),
        };
        std::fs::create_dir_all(fixture.download_dir()).unwrap();
        std::fs::create_dir_all(fixture.api_dir()).unwrap();
        std::fs::create_dir_all(fixture.home()).unwrap();
        std::fs::create_dir_all(fixture.data()).unwrap();
        for asset in senate::update::OFFICIAL_TARGETS {
            std::fs::copy(
                env!("CARGO_BIN_EXE_senate"),
                fixture.download_dir().join(asset),
            )
            .unwrap();
        }
        fixture.write_manifest();
        std::fs::write(
            fixture.api_dir().join("latest"),
            format!("{{\"tag_name\":\"v{VERSION}\",\"draft\":false,\"prerelease\":false}}"),
        )
        .unwrap();
        fixture
    }

    fn download_dir(&self) -> PathBuf {
        self.root
            .path()
            .join("releases/ember-mind/the-senate/releases/download")
            .join(format!("v{VERSION}"))
    }

    fn api_dir(&self) -> PathBuf {
        self.root
            .path()
            .join("api/repos/ember-mind/the-senate/releases")
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn data(&self) -> PathBuf {
        self.root.path().join("data")
    }

    fn bin(&self) -> PathBuf {
        self.home().join(".local/bin")
    }

    fn target(&self) -> PathBuf {
        self.bin().join("senate")
    }

    fn receipt(&self) -> PathBuf {
        self.data().join("install.json")
    }

    /// Regenerates SHA256SUMS over whatever the fixture currently publishes.
    fn write_manifest(&self) {
        use std::fmt::Write as _;
        let mut manifest = String::new();
        for asset in senate::update::OFFICIAL_TARGETS {
            let bytes = std::fs::read(self.download_dir().join(asset)).unwrap();
            let _ = writeln!(manifest, "{}  {asset}", sha256(&bytes));
        }
        std::fs::write(self.download_dir().join("SHA256SUMS"), manifest).unwrap();
    }

    /// Replaces one asset's bytes, keeping the manifest honest unless the
    /// caller asks for a stale one.
    fn replace_asset(&self, contents: &str, refresh_manifest: bool) {
        let asset = current_asset();
        let path = self.download_dir().join(asset);
        std::fs::write(&path, contents).unwrap();
        if refresh_manifest {
            self.write_manifest();
        }
    }

    fn run(&self) -> Output {
        self.run_with(&[], None)
    }

    /// Runs the installer with a scrubbed environment. `path` overrides the
    /// tool search path, which is how a missing checksum tool is simulated.
    fn run_with(&self, extra: &[(&str, &str)], path: Option<&str>) -> Output {
        let mut env: BTreeMap<&str, String> = BTreeMap::new();
        env.insert("HOME", self.home().display().to_string());
        env.insert(
            "PATH",
            path.map_or_else(
                || "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
                ToOwned::to_owned,
            ),
        );
        env.insert("TMPDIR", self.root.path().join("tmp").display().to_string());
        env.insert("SENATE_DATA_DIR", self.data().display().to_string());
        env.insert(
            "SENATE_API_BASE",
            format!("file://{}", self.root.path().join("api").display()),
        );
        env.insert(
            "SENATE_DOWNLOAD_BASE",
            format!("file://{}", self.root.path().join("releases").display()),
        );
        env.insert("SENATE_INSTALL_DIR", self.bin().display().to_string());
        for (key, value) in extra {
            env.insert(key, (*value).to_owned());
        }
        std::fs::create_dir_all(self.root.path().join("tmp")).unwrap();
        // Absolute, so replacing PATH cannot make the interpreter unfindable.
        std::process::Command::new("/bin/sh")
            .arg(installer_path())
            .env_clear()
            .envs(env)
            .output()
            .expect("installer runs")
    }
}

fn installer_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")
}

fn current_asset() -> &'static str {
    senate::update::target_asset_name().expect("tests run on a supported platform")
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut digest = String::new();
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(digest, "{byte:02x}");
    }
    digest
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// No `senate-install.*` working directory survives a run.
fn no_temporary_files(fixture: &Fixture) -> bool {
    let tmp = fixture.root.path().join("tmp");
    std::fs::read_dir(&tmp).map_or(true, |entries| {
        entries.filter_map(Result::ok).all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with("senate-install")
        })
    })
}

#[test]
fn a_verified_release_installs_and_registers_itself_as_official() {
    let fixture = Fixture::new();
    let output = fixture.run();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains(&format!("The Senate {VERSION} installed successfully")));
    assert!(fixture.target().is_file());
    assert!(!fixture.target().is_symlink());

    // The receipt is written by the installed binary, so the schema and path
    // rules come from the updater rather than from shell.
    assert!(
        fixture.receipt().is_file(),
        "receipt honors SENATE_DATA_DIR"
    );
    let receipt = std::fs::read_to_string(fixture.receipt()).unwrap();
    assert!(receipt.contains("\"schema_version\": 1"));
    assert!(receipt.contains(current_asset()));

    // And the updater classifies the result as officially managed, which is
    // the whole point of the bootstrap.
    let classified = std::process::Command::new(fixture.target())
        .args(["__install-source"])
        .env("SENATE_DATA_DIR", fixture.data())
        .env("CODEX_HOME", fixture.data().join("codex-home"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&classified.stdout).trim(),
        "official binary"
    );
    assert!(no_temporary_files(&fixture));
}

#[cfg(unix)]
#[test]
fn the_installed_binary_is_executable_and_nothing_is_staged_alongside_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    assert!(fixture.run().status.success());
    let mode = std::fs::metadata(fixture.target())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
    let leftovers: Vec<_> = std::fs::read_dir(fixture.bin())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "senate")
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging file left behind: {leftovers:?}"
    );
}

#[test]
fn an_explicit_version_is_canonicalized_and_a_malformed_one_is_refused() {
    let fixture = Fixture::new();
    // Both `0.1.0` and `v0.1.0` name the same release.
    for form in [VERSION.to_owned(), format!("v{VERSION}")] {
        let output = fixture.run_with(&[("SENATE_VERSION", &form)], None);
        assert!(output.status.success(), "{form}: {}", stderr(&output));
    }
    for rejected in ["latest", "0.1", "banana", &format!("{VERSION}-rc.1")] {
        let output = fixture.run_with(&[("SENATE_VERSION", rejected)], None);
        assert!(!output.status.success(), "{rejected} must be refused");
        assert!(
            stderr(&output).contains("not a stable release version"),
            "{rejected}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn a_checksum_mismatch_refuses_the_install_and_leaves_the_destination_alone() {
    let fixture = Fixture::new();
    // Manifest deliberately left describing the previous bytes.
    fixture.replace_asset("#!/bin/sh\necho \"senate 9.9.9\"\n", false);
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("checksum mismatch"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.target().exists(), "nothing was installed");
    assert!(
        !fixture.receipt().exists(),
        "a failed install writes no receipt"
    );
    assert!(no_temporary_files(&fixture));
}

#[test]
fn a_manifest_without_this_platforms_entry_refuses_the_install() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.download_dir().join("SHA256SUMS"),
        "0000000000000000000000000000000000000000000000000000000000000000  something-else\n",
    )
    .unwrap();
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("lists no entry"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.target().exists());
}

#[test]
fn a_missing_manifest_refuses_the_install() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.download_dir().join("SHA256SUMS")).unwrap();
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("SHA256SUMS"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.target().exists());
}

#[test]
fn a_binary_whose_version_disagrees_with_the_release_aborts_before_installing() {
    let fixture = Fixture::new();
    // Checksums are honest; the binary simply is not the release it claims.
    fixture.replace_asset("#!/bin/sh\necho \"senate 9.9.9\"\n", true);
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("reports version 9.9.9"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.target().exists(), "the destination is untouched");
    assert!(!fixture.receipt().exists());
    assert!(no_temporary_files(&fixture));
}

#[test]
fn a_failed_download_never_touches_the_destination() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.download_dir().join(current_asset())).unwrap();
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("could not download"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.target().exists());
    assert!(no_temporary_files(&fixture));
}

#[test]
fn an_unrelated_executable_at_the_destination_is_never_overwritten() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.bin()).unwrap();
    let stranger = "#!/bin/sh\necho \"not senate\"\n";
    std::fs::write(fixture.target(), stranger).unwrap();

    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not a Senate installation"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.target()).unwrap(),
        stranger,
        "an unrelated file survives untouched"
    );

    // Explicit force is the only way past it, and it still verifies everything.
    let forced = fixture.run_with(&[("SENATE_FORCE", "1")], None);
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_ne!(
        std::fs::read_to_string(fixture.target()).unwrap_or_default(),
        stranger
    );
}

#[test]
fn an_existing_official_installation_is_replaced_without_force() {
    let fixture = Fixture::new();
    assert!(fixture.run().status.success());
    let second = fixture.run();
    assert!(
        second.status.success(),
        "reinstalling over our own installation is allowed: {}",
        stderr(&second)
    );
    assert!(fixture.target().is_file());
}

#[test]
fn a_missing_checksum_tool_stops_the_install_rather_than_skipping_verification() {
    let fixture = Fixture::new();
    // A tool directory with everything the installer needs except a SHA-256
    // implementation.
    let tools = fixture.root.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    for tool in [
        "curl", "mktemp", "grep", "awk", "sed", "cut", "head", "tr", "chmod", "cp", "mv", "mkdir",
        "rm", "uname", "cat",
    ] {
        if let Some(found) = which(tool) {
            std::os::unix::fs::symlink(found, tools.join(tool)).ok();
        }
    }
    let output = fixture.run_with(&[], Some(&tools.display().to_string()));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("sha256sum or shasum is required"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.target().exists());
}

#[test]
fn unsupported_platforms_fail_before_anything_is_downloaded() {
    let fixture = Fixture::new();
    for (os, arch, expected) in [
        ("Windows_NT", "x86_64", "unsupported operating system"),
        ("Linux", "riscv64", "unsupported Linux architecture"),
        ("Darwin", "ppc64", "unsupported macOS architecture"),
    ] {
        let tools = fake_uname(&fixture, os, arch);
        let output = fixture.run_with(&[], Some(&tools));
        assert!(!output.status.success(), "{os}/{arch} must fail");
        assert!(
            stderr(&output).contains(expected),
            "{os}/{arch}: {}",
            stderr(&output)
        );
        assert!(!fixture.target().exists());
        assert!(
            no_temporary_files(&fixture),
            "{os}/{arch} left temporary files"
        );
    }
}

#[test]
fn every_supported_platform_maps_to_its_official_asset() {
    let fixture = Fixture::new();
    for (os, arch, asset) in [
        ("Darwin", "arm64", "senate-aarch64-apple-darwin"),
        ("Darwin", "aarch64", "senate-aarch64-apple-darwin"),
        ("Darwin", "x86_64", "senate-x86_64-apple-darwin"),
        ("Linux", "x86_64", "senate-x86_64-unknown-linux-gnu"),
        ("Linux", "amd64", "senate-x86_64-unknown-linux-gnu"),
    ] {
        let tools = fake_uname(&fixture, os, arch);
        let output = fixture.run_with(&[], Some(&tools));
        // The install itself only completes for this machine's real asset;
        // what matters here is which asset the mapping chose.
        let combined = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            combined.contains(asset),
            "{os}/{arch} should select {asset}, got: {combined}"
        );
    }
}

#[test]
fn path_guidance_appears_only_when_the_directory_is_not_on_path() {
    let fixture = Fixture::new();
    let missing = fixture.run();
    assert!(missing.status.success());
    assert!(stdout(&missing).contains("Add The Senate to PATH:"));
    assert!(stdout(&missing).contains("export PATH="));

    let with_path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", fixture.bin().display());
    let present = fixture.run_with(&[], Some(&with_path));
    assert!(present.status.success(), "{}", stderr(&present));
    assert!(stdout(&present).contains("Run:"));
    assert!(stdout(&present).contains("senate doctor"));
    assert!(
        !stdout(&present).contains("Add The Senate to PATH"),
        "no guidance is printed when PATH already covers the directory"
    );
    assert!(
        !stdout(&present).contains("Warning:"),
        "a winning PATH entry stays quiet"
    );
}

/// Being on PATH is not the same as winning it: an older installation earlier
/// in the search order keeps answering, and the installer must say so.
#[test]
fn a_shadowing_installation_earlier_in_path_is_reported() {
    let fixture = Fixture::new();
    let older = fixture.root.path().join("cargo-bin");
    std::fs::create_dir_all(&older).unwrap();
    let shadow = older.join("senate");
    std::fs::write(&shadow, "#!/bin/sh\necho \"senate 0.0.1\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // The older installation comes first, so it wins the lookup.
    let path = format!(
        "{}:{}:/usr/bin:/bin:/usr/sbin:/sbin",
        older.display(),
        fixture.bin().display()
    );
    let output = fixture.run_with(&[], Some(&path));
    assert!(output.status.success(), "installation still succeeds");
    let text = stdout(&output);
    assert!(text.contains("installed successfully"));
    assert!(text.contains("Warning:"), "{text}");
    assert!(
        text.contains(&shadow.display().to_string()),
        "the shadowing executable is named: {text}"
    );
    assert!(
        text.contains("before that directory in PATH"),
        "the fix is spelled out: {text}"
    );
    assert!(
        !text.contains("senate doctor"),
        "a shadowed install must not tell the user to run a command that would \
         reach the wrong binary"
    );
    // Never destructive: the other installation is left alone.
    assert!(shadow.is_file());
}

#[test]
fn the_installer_never_edits_shell_configuration_or_uses_sudo() {
    // Structural: these are the boundaries the installer must not cross. Only
    // executable lines count — the header documents these boundaries in prose.
    let code: String = INSTALLER
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "sudo",
        "git ",
        "eval ",
        ".zshrc",
        ".bashrc",
        ".profile",
        "--insecure",
        "-k ",
    ] {
        assert!(
            !code.contains(forbidden),
            "install.sh must not use {forbidden}"
        );
    }
    assert!(INSTALLER.contains("set -eu"), "strict shell behavior");
    assert!(
        INSTALLER.contains("trap cleanup EXIT"),
        "temporary files are cleaned up"
    );
    assert!(
        !INSTALLER.contains("skip-verification") && !INSTALLER.contains("--no-verify"),
        "verification can never be skipped"
    );
}

/// The three definitions of the official asset set must agree: the Rust
/// updater, the release workflow that uploads them, and the installer that
/// downloads them.
#[test]
fn asset_names_cannot_drift_between_rust_the_workflow_and_the_installer() {
    for asset in senate::update::OFFICIAL_TARGETS {
        let triple = asset.strip_prefix("senate-").unwrap();
        assert!(
            WORKFLOW.contains(&format!("target: {triple}")),
            "release.yml does not build {triple}"
        );
        assert!(
            INSTALLER.contains(asset),
            "install.sh does not map any platform to {asset}"
        );
    }
    // And nothing else is published or expected.
    let workflow_targets = WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .count();
    assert_eq!(
        workflow_targets,
        senate::update::OFFICIAL_TARGETS.len(),
        "the workflow builds a different number of targets than Rust knows about"
    );
    assert!(
        WORKFLOW.contains("senate-${{ matrix.target }}"),
        "the workflow must name assets exactly as the updater expects"
    );
    assert!(
        INSTALLER.contains("SHA256SUMS"),
        "the installer must consume the manifest the workflow publishes"
    );
}

/// Writes a `uname` stub reporting the requested platform, in a directory
/// that otherwise mirrors the normal tool path.
fn fake_uname(fixture: &Fixture, os: &str, arch: &str) -> String {
    let dir = fixture.root.path().join(format!("uname-{os}-{arch}"));
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join("uname");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\ncase \"$1\" in\n  -s) echo {os} ;;\n  -m) echo {arch} ;;\n  *) echo {os} ;;\nesac\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", dir.display())
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(tool))
            .find(|candidate| candidate.is_file())
    })
}
