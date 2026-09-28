//! Update discovery for official The Senate releases.
//!
//! Update state is application preference/cache state, not run state: it lives
//! as a small JSON document under the Senate data directory and never enters
//! the `SQLite` run store. Discovery is best-effort by construction — every
//! network, parse, or filesystem problem degrades to
//! [`UpdateStatus::Unavailable`] so that nothing here can make The Senate fail to
//! start.

mod cache;
mod install;
mod installer;
mod release;

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use semver::Version;

pub use cache::{CACHE_SCHEMA_VERSION, UpdateCache};
pub use install::{
    InstallReceipt, InstallSource, InstallStrategy, RECEIPT_SCHEMA_VERSION, RegistrationError,
    classify, classify_path, detect_install_source, load_receipt, register_official_install,
    write_receipt,
};
pub use installer::{
    AssetDownloader, CHECKSUM_ASSET, HttpDownloader, InstallError, Installed, OFFICIAL_TARGETS,
    Registration, checksum_for, install, install_with_receipt_path, target_asset_name,
    target_description,
};
pub use release::{
    GitHubReleases, Release, ReleaseAsset, ReleaseError, ReleaseSource, canonical_tag_version,
};

/// The one canonical application version: whatever this build was compiled as.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Official upstream repository consulted for release metadata.
pub const OFFICIAL_REPOSITORY: &str = "ember-mind/the-senate";

/// How long a successful check is reused before The Senate asks GitHub again.
/// Reused, not authoritative: a release published inside the window is simply
/// not known about yet. One check per day keeps unauthenticated API use well
/// inside GitHub's limits without ever requiring a token.
pub const CACHE_TTL: TimeDelta = TimeDelta::hours(24);

/// Environment opt-out. The Senate's configuration file is resolved but not yet
/// parsed, so an environment variable is the only existing knob that can be
/// honored without inventing a preferences system.
pub const DISABLE_ENVIRONMENT_VARIABLE: &str = "SENATE_DISABLE_UPDATE_CHECK";

/// What a completed check concluded. Version comparison only: whether the
/// installation *can* be updated automatically is
/// [`InstallStrategy`], a separate and orthogonal question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// This build is at or ahead of the newest official stable release.
    Current,
    /// A newer official stable release exists.
    Available(Box<UpdateInfo>),
    /// No conclusion could be reached: checks are disabled, the network was
    /// unreachable, GitHub rate-limited us, or the response was unusable.
    Unavailable,
}

impl UpdateStatus {
    #[must_use]
    pub const fn available(&self) -> Option<&UpdateInfo> {
        match self {
            Self::Available(info) => Some(info),
            Self::Current | Self::Unavailable => None,
        }
    }
}

/// Only facts taken from official release metadata, plus this build's version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub current_version: Version,
    pub available_version: Version,
    pub tag: String,
    pub release_url: String,
    pub published_at: Option<DateTime<Utc>>,
}

/// Resolves the running build's version once, for every caller.
///
/// # Errors
/// Returns an error only when the compiled package version is not semver,
/// which a release build cannot be.
pub fn current_version() -> Result<Version, semver::Error> {
    Version::parse(CURRENT_VERSION)
}

/// Whether automatic checking is switched off for this process.
#[must_use]
pub fn checks_disabled() -> bool {
    disabled_with(|name| std::env::var(name).ok())
}

fn disabled_with(mut get_var: impl FnMut(&str) -> Option<String>) -> bool {
    get_var(DISABLE_ENVIRONMENT_VARIABLE).is_some_and(|value| {
        let value = value.trim().to_ascii_lowercase();
        !matches!(value.as_str(), "" | "0" | "false" | "no")
    })
}

/// Why a candidate release tag may not be published.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReleaseTagError {
    #[error(
        "tag {0:?} is not a canonical The Senate release tag (expected vMAJOR.MINOR.PATCH with no prerelease or build metadata)"
    )]
    NotCanonical(String),
    #[error("tag {tag} does not match the package version {package} it would publish")]
    VersionMismatch { tag: String, package: String },
}

/// Gate for the release pipeline: a tag may only be published when it is
/// canonical *and* names exactly the version this checkout would build.
///
/// Both halves reuse the rules the updater already relies on —
/// [`canonical_tag_version`] for shape and [`CURRENT_VERSION`] for the
/// package version — so a tag that passes here is by construction one the
/// updater would later recognize and one whose assets report the same
/// version.
///
/// # Errors
/// Returns [`ReleaseTagError`] for a non-canonical tag or for a canonical tag
/// that disagrees with the compiled package version.
pub fn verify_release_tag(tag: &str) -> Result<Version, ReleaseTagError> {
    let version =
        canonical_tag_version(tag).ok_or_else(|| ReleaseTagError::NotCanonical(tag.to_owned()))?;
    if version.to_string() == CURRENT_VERSION {
        Ok(version)
    } else {
        Err(ReleaseTagError::VersionMismatch {
            tag: tag.to_owned(),
            package: CURRENT_VERSION.to_owned(),
        })
    }
}

/// Reads cached release metadata, refreshes it when stale, and compares
/// versions. Construction never performs I/O.
pub struct UpdateService<S> {
    source: S,
    cache_path: PathBuf,
    current: Version,
    ttl: TimeDelta,
    disabled: bool,
}

impl UpdateService<GitHubReleases> {
    /// Builds the service every normal The Senate invocation uses.
    ///
    /// # Errors
    /// Returns an error when the data directory or the compiled version cannot
    /// be resolved. Network reachability is never an error here.
    pub fn from_environment() -> anyhow::Result<Self> {
        Ok(Self {
            source: GitHubReleases::new(OFFICIAL_REPOSITORY, Duration::from_secs(5)),
            cache_path: crate::store::update_cache_file()?,
            current: current_version()?,
            ttl: CACHE_TTL,
            disabled: checks_disabled(),
        })
    }
}

impl<S: ReleaseSource> UpdateService<S> {
    #[must_use]
    pub const fn new(source: S, cache_path: PathBuf, current: Version) -> Self {
        Self {
            source,
            cache_path,
            current,
            ttl: CACHE_TTL,
            disabled: false,
        }
    }

    #[must_use]
    pub const fn with_disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    #[must_use]
    pub const fn with_ttl(mut self, ttl: TimeDelta) -> Self {
        self.ttl = ttl;
        self
    }

    #[must_use]
    pub const fn current(&self) -> &Version {
        &self.current
    }

    /// The cache-aware status, for automatic background checking only.
    ///
    /// A fresh cache answers without any network use; a missing, stale, or
    /// unreadable one triggers exactly one bounded check. Never use this for a
    /// command the user typed: someone who explicitly asks The Senate to check
    /// must get a real check, not a day-old answer. That is [`check_now`].
    ///
    /// [`check_now`]: Self::check_now
    #[must_use]
    pub fn cached_status(&self, now: DateTime<Utc>) -> UpdateStatus {
        if self.disabled {
            return UpdateStatus::Unavailable;
        }
        match cache::load(&self.cache_path) {
            Some(cached) if cached.is_fresh(now, self.ttl) => self.compare(&cached),
            // A missing, stale, or malformed cache all mean the same thing:
            // there is nothing trustworthy to answer with, so refresh.
            _ => self.refresh(now),
        }
    }

    /// Forces one check regardless of cache age: what every explicitly typed
    /// update command uses.
    ///
    /// A successful check refreshes the cache, so an automatic check shortly
    /// afterwards costs nothing. A failed one reports `Unavailable` and leaves
    /// any existing cache untouched — the user asked for a fresh answer, so a
    /// stale "up to date" would be a lie, but the cache is still good evidence
    /// for the automatic path.
    ///
    /// Still honors the opt-out.
    #[must_use]
    pub fn check_now(&self, now: DateTime<Utc>) -> UpdateStatus {
        if self.disabled {
            return UpdateStatus::Unavailable;
        }
        self.refresh(now)
    }

    fn refresh(&self, now: DateTime<Utc>) -> UpdateStatus {
        match self.source.latest_stable() {
            Ok(latest) => {
                let cached = UpdateCache::from_release(now, latest.as_ref());
                // A cache we cannot write only costs one extra check later.
                cache::store(&self.cache_path, &cached);
                self.compare(&cached)
            }
            Err(error) => {
                tracing::debug!(%error, "update check unavailable");
                UpdateStatus::Unavailable
            }
        }
    }

    /// A completed check that found nothing newer — including a repository
    /// that has published no stable release at all — means this build is not
    /// behind anything. `Unavailable` stays reserved for checks that reached
    /// no conclusion.
    fn compare(&self, cached: &UpdateCache) -> UpdateStatus {
        cached
            .update_over(&self.current)
            .map_or(UpdateStatus::Current, |info| {
                UpdateStatus::Available(Box::new(info))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use chrono::TimeZone as _;
    use tempfile::TempDir;

    use super::*;

    /// Injectable source that records how often it was consulted, so cache
    /// behavior can be asserted without any network.
    struct FakeSource {
        result: fn() -> Result<Option<Release>, ReleaseError>,
        calls: Cell<usize>,
    }

    impl FakeSource {
        fn new(result: fn() -> Result<Option<Release>, ReleaseError>) -> Self {
            Self {
                result,
                calls: Cell::new(0),
            }
        }
    }

    impl ReleaseSource for FakeSource {
        fn latest_stable(&self) -> Result<Option<Release>, ReleaseError> {
            self.calls.set(self.calls.get() + 1);
            (self.result)()
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "matches the ReleaseSource return type the fake must produce"
    )]
    fn release(version: &str) -> Result<Option<Release>, ReleaseError> {
        Ok(Some(Release {
            version: Version::parse(version).unwrap(),
            tag: format!("v{version}"),
            url: "https://example.invalid/r".to_owned(),
            published_at: None,
            assets: Vec::new(),
        }))
    }

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 22, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn service(fixture: &TempDir, source: FakeSource, current: &str) -> UpdateService<FakeSource> {
        UpdateService::new(
            source,
            fixture.path().join("update.json"),
            Version::parse(current).unwrap(),
        )
    }

    #[test]
    fn only_a_canonical_tag_naming_this_build_may_be_released() {
        // The version this checkout compiles as is the one a release would
        // publish, so the gate is expressed against it directly.
        let package = CURRENT_VERSION;
        let matching = format!("v{package}");
        assert_eq!(
            verify_release_tag(&matching).unwrap().to_string(),
            package,
            "a canonical tag naming this build is allowed"
        );

        // A tag ahead of (or behind) Cargo.toml is the mistake this gate
        // exists to catch.
        let ahead = {
            let mut version = current_version().unwrap();
            version.patch += 1;
            format!("v{version}")
        };
        assert_eq!(
            verify_release_tag(&ahead).unwrap_err(),
            ReleaseTagError::VersionMismatch {
                tag: ahead.clone(),
                package: package.to_owned(),
            }
        );

        // Shape failures are rejected before any version comparison, so the
        // pipeline cannot publish a tag the updater would later ignore.
        for tag in [
            package.to_owned(),
            format!("v{package}-rc.1"),
            format!("v{package}+build.7"),
            "release-2".to_owned(),
            "v0.2".to_owned(),
            String::new(),
        ] {
            assert!(
                matches!(
                    verify_release_tag(&tag),
                    Err(ReleaseTagError::NotCanonical(_))
                ),
                "tag {tag:?} must be rejected as non-canonical"
            );
        }
    }

    #[test]
    fn the_release_gate_and_the_updater_share_one_tag_rule() {
        // Anything the gate accepts, the updater recognizes as an official
        // release tag, and anything it rejects for shape, the updater ignores.
        let accepted = format!("v{CURRENT_VERSION}");
        assert!(canonical_tag_version(&accepted).is_some());
        assert!(verify_release_tag(&accepted).is_ok());
        for rejected in ["0.1.0", "v0.1.0-rc.1", "nightly"] {
            assert!(canonical_tag_version(rejected).is_none());
            assert!(matches!(
                verify_release_tag(rejected),
                Err(ReleaseTagError::NotCanonical(_))
            ));
        }
    }

    #[test]
    fn a_newer_release_is_offered_and_an_equal_or_older_one_is_not() {
        let fixture = TempDir::new().unwrap();
        let status =
            service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0").check_now(at(12));
        let info = status.available().expect("0.2.0 is newer than 0.1.0");
        assert_eq!(info.available_version.to_string(), "0.2.0");
        assert_eq!(info.current_version.to_string(), "0.1.0");
        assert_eq!(info.tag, "v0.2.0");

        for remote in ["0.1.0", "0.0.9"] {
            let fixture = TempDir::new().unwrap();
            let status = match remote {
                "0.1.0" => service(&fixture, FakeSource::new(|| release("0.1.0")), "0.1.0"),
                _ => service(&fixture, FakeSource::new(|| release("0.0.9")), "0.1.0"),
            }
            .check_now(at(12));
            assert_eq!(status, UpdateStatus::Current, "remote {remote}");
        }
    }

    /// The exact situation found during the real v0.1.1 → v0.1.2 round trip:
    /// a still-fresh cache from before the release was published.
    #[test]
    fn a_fresh_cache_never_answers_a_manual_check() {
        let fixture = TempDir::new().unwrap();
        // A check five minutes ago concluded 0.1.1 was the newest release.
        cache::store(
            &fixture.path().join("update.json"),
            &UpdateCache {
                schema_version: cache::CACHE_SCHEMA_VERSION,
                checked_at: at(12),
                latest_version: Some("0.1.1".to_owned()),
                latest_tag: Some("v0.1.1".to_owned()),
                release_url: Some("https://example.invalid/r".to_owned()),
                published_at: None,
            },
        );
        let five_minutes_later = at(12) + TimeDelta::minutes(5);

        // 0.1.2 has since been published.
        let automatic = service(&fixture, FakeSource::new(|| release("0.1.2")), "0.1.1");
        assert_eq!(
            automatic.cached_status(five_minutes_later),
            UpdateStatus::Current,
            "the background path keeps honoring the cache"
        );
        assert_eq!(
            automatic.source.calls.get(),
            0,
            "and stays off the network entirely"
        );

        let manual = service(&fixture, FakeSource::new(|| release("0.1.2")), "0.1.1");
        let status = manual.check_now(five_minutes_later);
        assert_eq!(manual.source.calls.get(), 1, "a typed check really checks");
        assert_eq!(
            status.available().unwrap().available_version.to_string(),
            "0.1.2",
            "and reports the release the cache had not seen yet"
        );
    }

    #[test]
    fn a_successful_manual_check_refreshes_the_cache_for_the_automatic_path() {
        let fixture = TempDir::new().unwrap();
        let manual = service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0");
        assert!(manual.check_now(at(12)).available().is_some());

        // The automatic path now has a fresh answer and no reason to ask again.
        let automatic = service(&fixture, FakeSource::new(|| release("0.9.0")), "0.1.0");
        let status = automatic.cached_status(at(13));
        assert_eq!(automatic.source.calls.get(), 0);
        assert_eq!(
            status.available().unwrap().available_version.to_string(),
            "0.2.0"
        );
    }

    #[test]
    fn a_failed_manual_check_reports_unavailable_and_preserves_the_cache() {
        let fixture = TempDir::new().unwrap();
        let seeded = service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0");
        assert!(seeded.check_now(at(12)).available().is_some());
        let before = std::fs::read_to_string(fixture.path().join("update.json")).unwrap();

        let offline = service(
            &fixture,
            FakeSource::new(|| Err(ReleaseError::Unreachable("dns".to_owned()))),
            "0.1.0",
        );
        assert_eq!(
            offline.check_now(at(13)),
            UpdateStatus::Unavailable,
            "an explicit check reports its own failure rather than a cached answer"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("update.json")).unwrap(),
            before,
            "and never overwrites a good cache with a failure"
        );

        // The automatic path may still use that untouched cache.
        let automatic = service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0");
        assert!(automatic.cached_status(at(13)).available().is_some());
        assert_eq!(automatic.source.calls.get(), 0);
    }

    #[test]
    fn a_repository_without_stable_releases_is_not_behind_anything() {
        let fixture = TempDir::new().unwrap();
        let status = service(&fixture, FakeSource::new(|| Ok(None)), "0.1.0").check_now(at(12));
        assert_eq!(status, UpdateStatus::Current);
    }

    #[test]
    fn a_fresh_cache_answers_without_touching_the_network() {
        let fixture = TempDir::new().unwrap();
        let first = service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0");
        assert!(first.cached_status(at(12)).available().is_some());
        assert_eq!(first.source.calls.get(), 1);

        let second = service(&fixture, FakeSource::new(|| release("0.9.0")), "0.1.0");
        let status = second.cached_status(at(20));
        assert_eq!(second.source.calls.get(), 0, "the fresh cache answered");
        assert_eq!(
            status.available().unwrap().available_version.to_string(),
            "0.2.0",
            "the cached release, not a new fetch"
        );
    }

    #[test]
    fn a_stale_cache_triggers_exactly_one_refresh() {
        let fixture = TempDir::new().unwrap();
        let _ =
            service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0").cached_status(at(12));
        let later = service(&fixture, FakeSource::new(|| release("0.3.0")), "0.1.0");
        let status = later.cached_status(at(12) + TimeDelta::hours(25));
        assert_eq!(later.source.calls.get(), 1);
        assert_eq!(
            status.available().unwrap().available_version.to_string(),
            "0.3.0"
        );
    }

    #[test]
    fn a_malformed_cache_refreshes_instead_of_failing() {
        let fixture = TempDir::new().unwrap();
        std::fs::write(fixture.path().join("update.json"), "{ not json").unwrap();
        let service = service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0");
        assert!(service.cached_status(at(12)).available().is_some());
        assert_eq!(service.source.calls.get(), 1);
    }

    #[test]
    fn every_network_failure_is_boring() {
        for failure in [
            (|| Err(ReleaseError::Unreachable("dns".to_owned()))) as fn() -> _,
            || Err(ReleaseError::Unreachable("timed out".to_owned())),
            || Err(ReleaseError::RateLimited),
            || {
                Err(ReleaseError::Malformed(
                    "unexpected end of input".to_owned(),
                ))
            },
        ] {
            let fixture = TempDir::new().unwrap();
            let service = service(&fixture, FakeSource::new(failure), "0.1.0");
            assert_eq!(
                service.cached_status(at(12)),
                UpdateStatus::Unavailable,
                "a failed check never becomes an update or an error"
            );
            assert!(
                !fixture.path().join("update.json").exists(),
                "a failed check never poisons the cache"
            );
        }
    }

    #[test]
    fn disabled_checks_never_reach_the_network() {
        let fixture = TempDir::new().unwrap();
        let service =
            service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0").with_disabled(true);
        assert_eq!(service.cached_status(at(12)), UpdateStatus::Unavailable);
        assert_eq!(service.check_now(at(12)), UpdateStatus::Unavailable);
        assert_eq!(service.source.calls.get(), 0);
    }

    #[test]
    fn a_custom_ttl_is_honored() {
        let fixture = TempDir::new().unwrap();
        let _ =
            service(&fixture, FakeSource::new(|| release("0.2.0")), "0.1.0").cached_status(at(12));
        let short = service(&fixture, FakeSource::new(|| release("0.4.0")), "0.1.0")
            .with_ttl(TimeDelta::minutes(1));
        let _ = short.cached_status(at(12) + TimeDelta::minutes(2));
        assert_eq!(short.source.calls.get(), 1);
    }

    #[test]
    fn compiled_version_is_valid_semver_and_matches_the_package() {
        assert_eq!(current_version().unwrap().to_string(), CURRENT_VERSION);
        assert_eq!(CURRENT_VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn opt_out_accepts_only_meaningful_values() {
        assert!(disabled_with(|_| Some("1".to_owned())));
        assert!(disabled_with(|_| Some("true".to_owned())));
        assert!(!disabled_with(|_| None));
        assert!(!disabled_with(|_| Some("0".to_owned())));
        assert!(!disabled_with(|_| Some("false".to_owned())));
        assert!(!disabled_with(|_| Some("  ".to_owned())));
    }
}
