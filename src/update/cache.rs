//! Tiny, forward-compatible update-check cache.
//!
//! The cache holds only public release metadata plus the time of the last
//! check. Nothing about the machine, its repositories, or its runs is stored,
//! and no failure to read or write it can affect startup.

use std::path::Path;

use chrono::{DateTime, TimeDelta, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

use super::UpdateInfo;

/// Bumped only when older documents must be discarded rather than migrated.
pub const CACHE_SCHEMA_VERSION: u32 = 1;

/// One completed check. `latest_*` stay `None` for a repository that has
/// published no stable release yet, which is a real answer worth caching.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCache {
    pub schema_version: u32,
    pub checked_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<Utc>>,
}

impl UpdateCache {
    #[must_use]
    pub fn from_release(now: DateTime<Utc>, release: Option<&super::Release>) -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            checked_at: now,
            latest_version: release.map(|release| release.version.to_string()),
            latest_tag: release.map(|release| release.tag.clone()),
            release_url: release.map(|release| release.url.clone()),
            published_at: release.and_then(|release| release.published_at),
        }
    }

    #[must_use]
    pub fn is_fresh(&self, now: DateTime<Utc>, ttl: TimeDelta) -> bool {
        if self.schema_version != CACHE_SCHEMA_VERSION {
            return false;
        }
        let age = now.signed_duration_since(self.checked_at);
        // A cache stamped in the future is a clock change, not evidence.
        age >= TimeDelta::zero() && age < ttl
    }

    #[must_use]
    pub fn latest_version(&self) -> Option<Version> {
        self.latest_version
            .as_deref()
            .and_then(|version| Version::parse(version).ok())
    }

    /// The cached release described as an update, when it really is newer than
    /// `current`.
    #[must_use]
    pub fn update_over(&self, current: &Version) -> Option<UpdateInfo> {
        let available = self.latest_version()?;
        if available <= *current {
            return None;
        }
        Some(UpdateInfo {
            current_version: current.clone(),
            available_version: available,
            tag: self.latest_tag.clone()?,
            release_url: self.release_url.clone().unwrap_or_default(),
            published_at: self.published_at,
        })
    }
}

/// Reads the cache, treating every failure — missing, unreadable, truncated,
/// malformed, or written by a future schema — as "no cache".
pub fn load(path: &Path) -> Option<UpdateCache> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<UpdateCache>(&raw) {
        Ok(cache) => Some(cache),
        Err(error) => {
            tracing::debug!(%error, "ignoring malformed update cache");
            None
        }
    }
}

/// Best-effort persistence: a cache that cannot be written costs one extra
/// check later and nothing else.
pub fn store(path: &Path, cache: &UpdateCache) {
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let encoded = serde_json::to_vec_pretty(cache)?;
        std::fs::write(path, encoded)
    };
    if let Err(error) = write() {
        tracing::debug!(%error, "update cache not persisted");
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use tempfile::TempDir;

    use super::*;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 22, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn cache(version: &str, checked: DateTime<Utc>) -> UpdateCache {
        UpdateCache {
            schema_version: CACHE_SCHEMA_VERSION,
            checked_at: checked,
            latest_version: Some(version.to_owned()),
            latest_tag: Some(format!("v{version}")),
            release_url: Some("https://example.invalid/release".to_owned()),
            published_at: None,
        }
    }

    #[test]
    fn freshness_respects_the_ttl_and_rejects_future_stamps() {
        let entry = cache("0.2.0", at(12));
        assert!(entry.is_fresh(at(12), TimeDelta::hours(24)));
        assert!(entry.is_fresh(at(23), TimeDelta::hours(24)));
        assert!(!entry.is_fresh(at(12) + TimeDelta::hours(24), TimeDelta::hours(24)));
        assert!(
            !entry.is_fresh(at(11), TimeDelta::hours(24)),
            "a cache stamped in the future is not evidence"
        );
    }

    #[test]
    fn a_foreign_schema_version_is_never_fresh() {
        let mut entry = cache("0.2.0", at(12));
        entry.schema_version = CACHE_SCHEMA_VERSION + 1;
        assert!(!entry.is_fresh(at(12), TimeDelta::hours(24)));
    }

    #[test]
    fn only_a_strictly_newer_version_is_an_update() {
        let current = Version::parse("0.1.0").unwrap();
        assert!(cache("0.2.0", at(12)).update_over(&current).is_some());
        assert!(cache("0.1.0", at(12)).update_over(&current).is_none());
        assert!(
            cache("0.0.9", at(12)).update_over(&current).is_none(),
            "an older published release is never offered"
        );
    }

    #[test]
    fn malformed_and_missing_caches_read_as_absent() {
        let fixture = TempDir::new().unwrap();
        let path = fixture.path().join("update.json");
        assert!(load(&path).is_none(), "missing");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_none(), "malformed");
        std::fs::write(&path, "{\"schema_version\":1}").unwrap();
        assert!(load(&path).is_none(), "missing required fields");
    }

    #[test]
    fn a_written_cache_round_trips_and_creates_its_directory() {
        let fixture = TempDir::new().unwrap();
        let path = fixture.path().join("nested").join("update.json");
        let entry = cache("0.3.1", at(9));
        store(&path, &entry);
        assert_eq!(load(&path).unwrap(), entry);
    }

    #[test]
    fn a_repository_without_releases_caches_the_absence() {
        let entry = UpdateCache::from_release(at(9), None);
        assert!(entry.latest_version().is_none());
        assert!(entry.is_fresh(at(9), TimeDelta::hours(24)));
    }
}
