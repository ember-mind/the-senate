//! Safe replacement of an official The Senate binary.
//!
//! The pipeline is deliberately linear and refuses at the first doubt:
//! resolve this platform's asset, obtain its checksum from the release's own
//! `SHA256SUMS`, download to a staging file on the *same* filesystem as the
//! target, verify the digest, make it executable, confirm the staged binary
//! reports the version the release claims, and only then rename it over the
//! target. The existing executable is never removed, truncated, or written
//! through; the final step is a single atomic rename, so an interrupted
//! update leaves the old binary in place and usable.
//!
//! Release assets are uncompressed binaries plus one `SHA256SUMS` file. That
//! costs download size and removes archive handling — path traversal, symlink
//! escape, entry-count limits — from the update path entirely.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use semver::Version;
use sha2::{Digest as _, Sha256};

use super::{InstallReceipt, Release, ReleaseAsset};

/// Checksum manifest published alongside the binaries.
pub const CHECKSUM_ASSET: &str = "SHA256SUMS";

/// Largest binary The Senate will accept. Well above a real release build and a
/// hard stop for a hostile or broken response.
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;

/// Largest checksum manifest The Senate will read.
const MAX_CHECKSUM_BYTES: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("no official binary of The Senate is published for {0}")]
    UnsupportedPlatform(String),
    #[error("release {0} publishes no asset named {1}")]
    MissingAsset(String, String),
    #[error("release {0} publishes no {CHECKSUM_ASSET} manifest")]
    MissingChecksums(String),
    #[error("{CHECKSUM_ASSET} lists no entry for {0}")]
    MissingChecksum(String),
    #[error("checksum mismatch for {asset}: expected {expected}, computed {computed}")]
    ChecksumMismatch {
        asset: String,
        expected: String,
        computed: String,
    },
    #[error("downloaded binary reports version {computed}, but release claims {expected}")]
    VersionMismatch { expected: String, computed: String },
    #[error("download failed: {0}")]
    Download(String),
    #[error("staging failed: {0}")]
    Staging(String),
    #[error("installation failed: {0}")]
    Replace(String),
}

/// Whether the installed executable was recorded as officially managed.
///
/// Binary replacement and receipt persistence are separate facts, and this
/// keeps them separate: claiming a fully successful self-update after the
/// receipt failed would leave the next process unrecognized with no
/// explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Registration {
    /// The receipt names the installed executable; self-update stays available.
    Recorded,
    /// The binary is installed and working, but automatic updates are not
    /// registered for it.
    Failed(String),
}

/// A completed installation. The running process keeps executing the binary
/// it already loaded; the new one takes effect on the next start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub version: Version,
    pub executable: PathBuf,
    pub asset: String,
    pub registration: Registration,
}

impl Installed {
    /// The one sentence the user needs after a successful install.
    #[must_use]
    pub fn restart_notice(&self) -> String {
        format!(
            "Update installed. Senate {} will be used next time you start it.",
            self.version
        )
    }

    /// Present only when the executable was replaced but its registration was
    /// not. Callers must show it: the alternative is a silent loss of
    /// automatic updates.
    #[must_use]
    pub fn registration_warning(&self) -> Option<String> {
        match &self.registration {
            Registration::Recorded => None,
            Registration::Failed(reason) => Some(format!(
                "Automatic updates are no longer registered for this installation ({reason}). \
                 Re-run the Senate installer to restore them."
            )),
        }
    }
}

/// Fetches release assets. Injectable so the whole installer can be tested
/// without a network.
pub trait AssetDownloader {
    /// Streams one asset to `destination`.
    ///
    /// # Errors
    /// Returns transport failures and oversized responses.
    fn download(&self, url: &str, destination: &Path) -> Result<(), InstallError>;

    /// Reads one small text asset, such as the checksum manifest.
    ///
    /// # Errors
    /// Returns transport failures and oversized responses.
    fn fetch_text(&self, url: &str) -> Result<String, InstallError>;
}

/// Every asset name an official release publishes, in the order the release
/// workflow builds them.
///
/// This is the bootstrap contract: `install.sh` maps a detected platform to
/// one of these names, the release workflow uploads exactly these names, and
/// [`target_asset_name`] resolves one of them for the running build. A test
/// cross-checks all three against this list so they cannot drift apart.
pub const OFFICIAL_TARGETS: [&str; 3] = [
    "senate-aarch64-apple-darwin",
    "senate-x86_64-apple-darwin",
    "senate-x86_64-unknown-linux-gnu",
];

/// The asset name for the platform this binary was compiled for, or `None`
/// where The Senate publishes no official build. Windows is absent because
/// The Senate requires tmux and is neither supported nor tested there.
#[must_use]
pub const fn target_asset_name() -> Option<&'static str> {
    match (cfg!(target_os = "macos"), cfg!(target_os = "linux")) {
        (true, _) if cfg!(target_arch = "aarch64") => Some(OFFICIAL_TARGETS[0]),
        (true, _) if cfg!(target_arch = "x86_64") => Some(OFFICIAL_TARGETS[1]),
        (_, true) if cfg!(target_arch = "x86_64") => Some(OFFICIAL_TARGETS[2]),
        _ => None,
    }
}

/// Human description of the current platform, for the unsupported message.
#[must_use]
pub fn target_description() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Finds one asset's digest in a `sha256sum`-style manifest.
///
/// Accepts the standard `<hex>  <name>` and `<hex> *<name>` forms, ignores
/// blank lines, and never matches a partial name.
#[must_use]
pub fn checksum_for(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        let name = Path::new(name).file_name()?.to_str()?;
        (name == asset && digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| digest.to_ascii_lowercase())
    })
}

fn digest_of(path: &Path) -> Result<String, InstallError> {
    let bytes = std::fs::read(path).map_err(|error| InstallError::Staging(error.to_string()))?;
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(&bytes) {
        use std::fmt::Write as _;
        let _ = write!(digest, "{byte:02x}");
    }
    Ok(digest)
}

/// Removes a staging file, ignoring failures: the install already failed and
/// a leftover temporary must not mask the real error.
fn discard(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, "staging file not removed");
    }
}

/// Downloads, verifies, and installs the release's binary over `executable`,
/// recording the result in the application's own receipt.
///
/// This is the production entry point; it resolves the receipt destination and
/// defers to [`install_with_receipt_path`]. Tests must use that function with a
/// fixture path instead, so no test can resolve — or overwrite — a real user's
/// receipt.
///
/// # Errors
/// Returns a typed failure at the first problem. The existing executable is
/// untouched unless the function returns `Ok`.
pub fn install(
    release: &Release,
    executable: &Path,
    downloader: &dyn AssetDownloader,
    now: DateTime<Utc>,
) -> Result<Installed, InstallError> {
    let receipt_path = crate::store::install_receipt_file()
        .map_err(|error| InstallError::Staging(error.to_string()))?;
    install_with_receipt_path(release, executable, &receipt_path, downloader, now)
}

/// Downloads, verifies, and installs the release's binary over `executable`,
/// writing the receipt to an explicit destination.
///
/// The receipt destination is a parameter rather than an ambient lookup so the
/// installation transaction and the metadata it records are separable, and so
/// a test can never reach a real receipt by omission.
///
/// # Errors
/// Returns a typed failure at the first problem. The existing executable is
/// untouched unless the function returns `Ok`. A receipt that cannot be
/// written is *not* an error — the binary really was installed — but it is
/// reported through [`Installed::registration`].
pub fn install_with_receipt_path(
    release: &Release,
    executable: &Path,
    receipt_path: &Path,
    downloader: &dyn AssetDownloader,
    now: DateTime<Utc>,
) -> Result<Installed, InstallError> {
    let asset_name = target_asset_name()
        .ok_or_else(|| InstallError::UnsupportedPlatform(target_description()))?;
    let asset: &ReleaseAsset = release
        .asset(asset_name)
        .ok_or_else(|| InstallError::MissingAsset(release.tag.clone(), asset_name.to_owned()))?;
    let manifest_asset = release
        .asset(CHECKSUM_ASSET)
        .ok_or_else(|| InstallError::MissingChecksums(release.tag.clone()))?;

    // Integrity metadata is obtained before anything is written, so a release
    // without checksums can never reach the staging step.
    let manifest = downloader.fetch_text(&manifest_asset.download_url)?;
    let expected = checksum_for(&manifest, asset_name)
        .ok_or_else(|| InstallError::MissingChecksum(asset_name.to_owned()))?;

    // Staging next to the target keeps the final rename on one filesystem,
    // which is what makes it atomic.
    let staged = staging_path(executable);
    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| InstallError::Staging(error.to_string()))?;
    }
    let result = stage_and_verify(asset, &staged, downloader, &expected, &release.version);
    if let Err(error) = result {
        discard(&staged);
        return Err(error);
    }

    // Single atomic step: after this the path holds either the old binary or
    // the fully verified new one, never a partial file.
    if let Err(error) = std::fs::rename(&staged, executable) {
        discard(&staged);
        return Err(InstallError::Replace(error.to_string()));
    }

    // The executable is in place and verified. A receipt failure from here on
    // is real but not fatal: destroying a working binary over metadata would
    // be worse, so the outcome is reported instead of hidden.
    let receipt = InstallReceipt::new(
        executable.to_path_buf(),
        release.version.to_string(),
        asset_name,
        now,
    );
    let registration = match super::write_receipt(receipt_path, &receipt) {
        Ok(()) => Registration::Recorded,
        Err(error) => {
            tracing::warn!(%error, "install receipt not persisted");
            Registration::Failed(error.to_string())
        }
    };
    Ok(Installed {
        version: release.version.clone(),
        executable: executable.to_path_buf(),
        asset: asset_name.to_owned(),
        registration,
    })
}

/// Everything that happens before the irreversible step, so the caller can
/// clean up a single failure path.
fn stage_and_verify(
    asset: &ReleaseAsset,
    staged: &Path,
    downloader: &dyn AssetDownloader,
    expected: &str,
    version: &Version,
) -> Result<(), InstallError> {
    downloader.download(&asset.download_url, staged)?;
    let computed = digest_of(staged)?;
    if computed != expected {
        return Err(InstallError::ChecksumMismatch {
            asset: asset.name.clone(),
            expected: expected.to_owned(),
            computed,
        });
    }
    make_executable(staged)?;
    verify_reported_version(staged, version)
}

/// The staging file lives beside the target and is named so an interrupted
/// run leaves something obviously temporary rather than a plausible binary.
fn staging_path(executable: &Path) -> PathBuf {
    let name = executable.file_name().map_or_else(
        || "senate".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    executable.with_file_name(format!(".{name}.update"))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path)
        .map_err(|error| InstallError::Staging(error.to_string()))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .map_err(|error| InstallError::Staging(error.to_string()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), InstallError> {
    Ok(())
}

/// Confirms the staged binary is the version the release metadata claims, so
/// a mislabelled or mismatched asset cannot be installed. The checksum has
/// already matched the release's own manifest at this point.
fn verify_reported_version(staged: &Path, expected: &Version) -> Result<(), InstallError> {
    // Tests stage the freshly written binary and run it immediately, which
    // can race another test thread's fork still holding its write fd open
    // (`ETXTBSY`); retry_busy clears that window without hiding a real
    // failure.
    let output =
        crate::exec::retry_busy(|| std::process::Command::new(staged).arg("--version").output())
            .map_err(|error| InstallError::Staging(error.to_string()))?;
    let reported = String::from_utf8_lossy(&output.stdout);
    let computed = reported
        .split_whitespace()
        .find_map(|word| Version::parse(word.trim_start_matches('v')).ok())
        .ok_or_else(|| InstallError::VersionMismatch {
            expected: expected.to_string(),
            computed: reported.trim().to_owned(),
        })?;
    if computed == *expected {
        Ok(())
    } else {
        Err(InstallError::VersionMismatch {
            expected: expected.to_string(),
            computed: computed.to_string(),
        })
    }
}

/// The real downloader: HTTPS with a bounded body and a plain timeout.
pub struct HttpDownloader {
    timeout: std::time::Duration,
}

impl HttpDownloader {
    #[must_use]
    pub const fn new(timeout: std::time::Duration) -> Self {
        Self { timeout }
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .user_agent(format!("senate/{}", super::CURRENT_VERSION))
            .build()
            .into()
    }
}

impl AssetDownloader for HttpDownloader {
    fn download(&self, url: &str, destination: &Path) -> Result<(), InstallError> {
        let mut response = self
            .agent()
            .get(url)
            .call()
            .map_err(|error| InstallError::Download(error.to_string()))?;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_ASSET_BYTES)
            .read_to_vec()
            .map_err(|error| InstallError::Download(error.to_string()))?;
        std::fs::write(destination, bytes).map_err(|error| InstallError::Staging(error.to_string()))
    }

    fn fetch_text(&self, url: &str) -> Result<String, InstallError> {
        let mut response = self
            .agent()
            .get(url)
            .call()
            .map_err(|error| InstallError::Download(error.to_string()))?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_CHECKSUM_BYTES)
            .read_to_string()
            .map_err(|error| InstallError::Download(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::TimeZone as _;
    use tempfile::TempDir;

    use super::*;

    /// Serves canned bytes; records every URL it was asked for.
    struct FakeDownloader {
        assets: HashMap<String, Vec<u8>>,
    }

    impl FakeDownloader {
        fn new(assets: &[(&str, &[u8])]) -> Self {
            Self {
                assets: assets
                    .iter()
                    .map(|(url, bytes)| ((*url).to_owned(), (*bytes).to_vec()))
                    .collect(),
            }
        }
    }

    impl AssetDownloader for FakeDownloader {
        fn download(&self, url: &str, destination: &Path) -> Result<(), InstallError> {
            let bytes = self
                .assets
                .get(url)
                .ok_or_else(|| InstallError::Download(format!("no fixture for {url}")))?;
            std::fs::write(destination, bytes)
                .map_err(|error| InstallError::Staging(error.to_string()))
        }

        fn fetch_text(&self, url: &str) -> Result<String, InstallError> {
            self.assets
                .get(url)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .ok_or_else(|| InstallError::Download(format!("no fixture for {url}")))
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 22, 12, 0, 0)
            .single()
            .unwrap()
    }

    fn hex(bytes: &[u8]) -> String {
        let mut digest = String::new();
        for byte in Sha256::digest(bytes) {
            use std::fmt::Write as _;
            let _ = write!(digest, "{byte:02x}");
        }
        digest
    }

    /// A stand-in release binary: a shell script that answers `--version`
    /// exactly the way the real executable does. Never the test runner.
    fn fake_binary(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"senate {version}\"\n").into_bytes()
    }

    fn release(version: &str, assets: Vec<ReleaseAsset>) -> Release {
        Release {
            version: Version::parse(version).unwrap(),
            tag: format!("v{version}"),
            url: "https://example.invalid/r".to_owned(),
            published_at: None,
            assets,
        }
    }

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_owned(),
            download_url: format!("https://example.invalid/{name}"),
            size: 0,
        }
    }

    /// A release for this platform whose manifest matches its binary.
    fn healthy(version: &str) -> (Release, FakeDownloader, Vec<u8>) {
        let name = target_asset_name().expect("tests run on a supported platform");
        let binary = fake_binary(version);
        let manifest = format!("{}  {name}\n", hex(&binary));
        let release = release(version, vec![asset(name), asset(CHECKSUM_ASSET)]);
        let downloader = FakeDownloader::new(&[
            (
                &format!("https://example.invalid/{name}"),
                binary.as_slice(),
            ),
            (
                &format!("https://example.invalid/{CHECKSUM_ASSET}"),
                manifest.as_bytes(),
            ),
        ]);
        (release, downloader, binary)
    }

    /// Fixture-local receipt destination. Nothing in these tests may resolve
    /// the application receipt path, so a `cargo test` run cannot touch a real
    /// installation's registration.
    trait ReceiptFixture {
        fn receipt(&self) -> PathBuf;
    }

    impl ReceiptFixture for TempDir {
        fn receipt(&self) -> PathBuf {
            self.path().join("data").join("install.json")
        }
    }

    fn existing(fixture: &TempDir) -> PathBuf {
        let path = fixture.path().join("senate");
        std::fs::write(&path, b"#!/bin/sh\necho \"senate 0.1.0\"\n").unwrap();
        make_executable(&path).unwrap();
        path
    }

    #[test]
    fn this_platform_selects_a_deterministic_asset_name() {
        let name = target_asset_name().expect("macOS and Linux are supported");
        assert!(name.starts_with("senate-"));
        assert!(
            !name.contains(".tar") && !name.contains(".gz"),
            "assets are uncompressed binaries"
        );
        if cfg!(target_os = "macos") {
            assert!(name.ends_with("-apple-darwin"));
        } else {
            assert!(name.ends_with("-unknown-linux-gnu"));
        }
        assert!(!target_description().is_empty());
    }

    #[test]
    fn checksum_lookup_accepts_standard_manifests_and_rejects_near_misses() {
        let manifest = "\
aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233  senate-x86_64-unknown-linux-gnu
bb11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233 *senate-aarch64-apple-darwin
";
        // Digests here are 64 hex characters; only exact names match.
        let linux = checksum_for(manifest, "senate-x86_64-unknown-linux-gnu");
        assert!(linux.is_some());
        assert!(checksum_for(manifest, "senate-aarch64-apple-darwin").is_some());
        assert!(
            checksum_for(manifest, "senate-x86_64").is_none(),
            "a partial name never matches"
        );
        assert!(checksum_for("", "senate-x86_64-unknown-linux-gnu").is_none());
        assert!(
            checksum_for(
                "notahex  senate-x86_64-unknown-linux-gnu",
                "senate-x86_64-unknown-linux-gnu"
            )
            .is_none(),
            "a malformed digest is not a checksum"
        );
    }

    #[test]
    fn a_verified_release_installs_atomically_and_reports_restart_semantics() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let (release, downloader, binary) = healthy("0.2.0");
        let installed = install_with_receipt_path(
            &release,
            &executable,
            &fixture.receipt(),
            &downloader,
            now(),
        )
        .unwrap();

        assert_eq!(installed.version.to_string(), "0.2.0");
        assert_eq!(std::fs::read(&executable).unwrap(), binary);
        assert!(
            installed
                .restart_notice()
                .contains("next time you start it")
        );
        assert!(
            std::fs::read_dir(fixture.path())
                .unwrap()
                .all(|entry| entry.unwrap().file_name() != ".senate.update"),
            "no staging file survives a successful install"
        );
    }

    /// The field failure this seam exists to prevent: an installer unit test
    /// writing its fixture receipt into a real user's data directory. A
    /// sentinel standing in for `~/.senate/install.json` must survive a full
    /// install byte for byte.
    #[test]
    fn an_installer_test_can_never_reach_a_receipt_outside_its_fixture() {
        let user = TempDir::new().unwrap();
        let sentinel = user.path().join("install.json");
        let real = "{\n  \"schema_version\": 1,\n  \"executable\": \"/Users/e/.local/bin/senate\",\n  \"version\": \"0.1.3\"\n}\n";
        std::fs::write(&sentinel, real).unwrap();

        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let (release, downloader, _) = healthy("0.2.0");
        let installed = install_with_receipt_path(
            &release,
            &executable,
            &fixture.receipt(),
            &downloader,
            now(),
        )
        .unwrap();

        assert_eq!(installed.registration, Registration::Recorded);
        assert!(
            fixture.receipt().is_file(),
            "the fixture receipt is written"
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            real,
            "a receipt outside the fixture is never touched"
        );
    }

    #[test]
    fn a_receipt_that_cannot_be_written_is_reported_rather_than_hidden() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        // A regular file where the receipt's parent directory must go, so
        // creating it fails and the write cannot succeed.
        let blocked = fixture.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let receipt_path = blocked.join("install.json");

        let (release, downloader, binary) = healthy("0.2.0");
        let installed =
            install_with_receipt_path(&release, &executable, &receipt_path, &downloader, now())
                .unwrap();

        // The binary really was installed; pretending otherwise would be just
        // as untruthful as claiming full success.
        assert_eq!(std::fs::read(&executable).unwrap(), binary);
        assert!(matches!(installed.registration, Registration::Failed(_)));
        let warning = installed
            .registration_warning()
            .expect("a failed registration must be surfaced");
        assert!(warning.contains("no longer registered"));
        assert!(
            !installed.restart_notice().is_empty(),
            "the restart notice still describes what did happen"
        );
    }

    #[test]
    fn a_successful_install_reports_registration_and_offers_no_warning() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let (release, downloader, _) = healthy("0.2.0");
        let installed = install_with_receipt_path(
            &release,
            &executable,
            &fixture.receipt(),
            &downloader,
            now(),
        )
        .unwrap();
        assert_eq!(installed.registration, Registration::Recorded);
        assert!(installed.registration_warning().is_none());
    }

    #[test]
    fn a_failed_receipt_write_leaves_the_previous_receipt_intact() {
        let fixture = TempDir::new().unwrap();
        let path = fixture.path().join("install.json");
        let good = InstallReceipt::new(
            PathBuf::from("/usr/local/bin/senate"),
            "0.1.3",
            "senate-aarch64-apple-darwin",
            now(),
        );
        super::super::write_receipt(&path, &good).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // A directory where the temporary file must go makes the write fail
        // after the previous receipt already exists.
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        let replacement = InstallReceipt::new(
            PathBuf::from("/usr/local/bin/senate"),
            "9.9.9",
            "senate-aarch64-apple-darwin",
            now(),
        );
        assert!(super::super::write_receipt(&path, &replacement).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "an interrupted write never truncates the valid receipt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_installed_binary_is_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let (release, downloader, _) = healthy("0.2.0");
        install_with_receipt_path(
            &release,
            &executable,
            &fixture.receipt(),
            &downloader,
            now(),
        )
        .unwrap();
        let mode = std::fs::metadata(&executable).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "owner, group, and other can execute");
    }

    #[test]
    fn a_checksum_mismatch_rejects_the_install_and_leaves_the_old_binary() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let original = std::fs::read(&executable).unwrap();
        let name = target_asset_name().unwrap();
        let release = release("0.2.0", vec![asset(name), asset(CHECKSUM_ASSET)]);
        let downloader = FakeDownloader::new(&[
            (
                &format!("https://example.invalid/{name}"),
                fake_binary("0.2.0").as_slice(),
            ),
            (
                &format!("https://example.invalid/{CHECKSUM_ASSET}"),
                format!("{}  {name}\n", "0".repeat(64)).as_bytes(),
            ),
        ]);
        let error = install_with_receipt_path(
            &release,
            &executable,
            &fixture.receipt(),
            &downloader,
            now(),
        )
        .unwrap_err();
        assert!(matches!(error, InstallError::ChecksumMismatch { .. }));
        assert_eq!(std::fs::read(&executable).unwrap(), original);
        assert!(
            !fixture.path().join(".senate.update").exists(),
            "the staging file is cleaned up"
        );
    }

    #[test]
    fn a_release_without_checksums_is_never_installed() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let original = std::fs::read(&executable).unwrap();
        let name = target_asset_name().unwrap();
        let release = release("0.2.0", vec![asset(name)]);
        let downloader = FakeDownloader::new(&[(
            &format!("https://example.invalid/{name}"),
            fake_binary("0.2.0").as_slice(),
        )]);
        let error = install_with_receipt_path(
            &release,
            &executable,
            &fixture.receipt(),
            &downloader,
            now(),
        )
        .unwrap_err();
        assert!(matches!(error, InstallError::MissingChecksums(_)));
        assert_eq!(std::fs::read(&executable).unwrap(), original);
    }

    #[test]
    fn a_manifest_without_our_entry_is_never_installed() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let name = target_asset_name().unwrap();
        let release = release("0.2.0", vec![asset(name), asset(CHECKSUM_ASSET)]);
        let downloader = FakeDownloader::new(&[
            (
                &format!("https://example.invalid/{name}"),
                fake_binary("0.2.0").as_slice(),
            ),
            (
                &format!("https://example.invalid/{CHECKSUM_ASSET}"),
                b"aa  some-other-artifact\n".as_slice(),
            ),
        ]);
        assert!(matches!(
            install_with_receipt_path(
                &release,
                &executable,
                &fixture.receipt(),
                &downloader,
                now()
            )
            .unwrap_err(),
            InstallError::MissingChecksum(_)
        ));
    }

    #[test]
    fn a_release_missing_this_platforms_asset_fails_before_any_write() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let release = release("0.2.0", vec![asset(CHECKSUM_ASSET)]);
        let downloader = FakeDownloader::new(&[]);
        assert!(matches!(
            install_with_receipt_path(
                &release,
                &executable,
                &fixture.receipt(),
                &downloader,
                now()
            )
            .unwrap_err(),
            InstallError::MissingAsset(_, _)
        ));
        assert!(!fixture.path().join(".senate.update").exists());
    }

    #[test]
    fn an_asset_reporting_the_wrong_version_is_rejected() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let original = std::fs::read(&executable).unwrap();
        let name = target_asset_name().unwrap();
        // The manifest is internally consistent, but the binary identifies
        // itself as an older release than the tag claims.
        let binary = fake_binary("0.2.0");
        let manifest = format!("{}  {name}\n", hex(&binary));
        let release = release("0.3.0", vec![asset(name), asset(CHECKSUM_ASSET)]);
        let downloader = FakeDownloader::new(&[
            (
                &format!("https://example.invalid/{name}"),
                binary.as_slice(),
            ),
            (
                &format!("https://example.invalid/{CHECKSUM_ASSET}"),
                manifest.as_bytes(),
            ),
        ]);
        let error = install_with_receipt_path(
            &release,
            &executable,
            &fixture.receipt(),
            &downloader,
            now(),
        )
        .unwrap_err();
        assert!(matches!(error, InstallError::VersionMismatch { .. }));
        assert_eq!(std::fs::read(&executable).unwrap(), original);
        assert!(!fixture.path().join(".senate.update").exists());
    }

    #[test]
    fn a_failed_download_cleans_up_and_preserves_the_existing_binary() {
        let fixture = TempDir::new().unwrap();
        let executable = existing(&fixture);
        let original = std::fs::read(&executable).unwrap();
        let name = target_asset_name().unwrap();
        let release = release("0.2.0", vec![asset(name), asset(CHECKSUM_ASSET)]);
        let downloader = FakeDownloader::new(&[(
            &format!("https://example.invalid/{CHECKSUM_ASSET}"),
            format!("{}  {name}\n", "a".repeat(64)).as_bytes(),
        )]);
        assert!(matches!(
            install_with_receipt_path(
                &release,
                &executable,
                &fixture.receipt(),
                &downloader,
                now()
            )
            .unwrap_err(),
            InstallError::Download(_)
        ));
        assert_eq!(std::fs::read(&executable).unwrap(), original);
        assert!(!fixture.path().join(".senate.update").exists());
    }

    /// Structural fence for the whole module: the production wrapper resolves
    /// the real receipt path, so no test here may call it.
    #[test]
    fn no_test_in_this_module_uses_the_production_receipt_path() {
        let source = include_str!("installer.rs");
        let tests = source
            .split("#[cfg(test)]")
            .nth(1)
            .expect("this module has tests");
        let bare_calls = tests
            .match_indices("install(")
            .filter(|(index, _)| {
                // `install_with_receipt_path(` also ends in `install(`-free
                // text; only a call spelled exactly `install(` counts.
                !tests[..*index].ends_with('_')
            })
            .count();
        assert_eq!(
            bare_calls, 0,
            "installer tests must call install_with_receipt_path with a fixture destination"
        );
        assert!(
            !tests.contains("install_receipt_file"),
            "no test may resolve the application receipt path"
        );
    }

    #[test]
    fn staging_stays_beside_the_target_so_the_swap_is_atomic() {
        let staged = staging_path(Path::new("/usr/local/bin/senate"));
        assert_eq!(staged.parent(), Some(Path::new("/usr/local/bin")));
        assert_eq!(
            staged.file_name().unwrap(),
            ".senate.update",
            "an interrupted run leaves something obviously temporary"
        );
    }
}
