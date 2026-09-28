//! Official release metadata, behind an injectable source.
//!
//! Only the documented GitHub REST releases endpoint is used; nothing scrapes
//! HTML. Requests are bounded in time and in body size, identify themselves,
//! and carry no information about the machine beyond a version string.

use std::time::Duration;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::Deserialize;

/// Largest release listing The Senate will read. Comfortably above the real
/// payload, and a hard stop for a hostile or broken response.
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;

/// Release pages requested. Enough history that a run of prereleases cannot
/// hide the newest stable release.
const RELEASE_PAGE_SIZE: u32 = 20;

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("release metadata is unreachable: {0}")]
    Unreachable(String),
    #[error("release metadata request was rate limited")]
    RateLimited,
    #[error("release metadata response was rejected: {0}")]
    Malformed(String),
}

/// One official release, already filtered and parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub url: String,
    pub published_at: Option<DateTime<Utc>>,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
}

impl Release {
    #[must_use]
    pub fn asset(&self, name: &str) -> Option<&ReleaseAsset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// Where release metadata comes from. Injectable so every policy decision can
/// be tested without touching the network.
pub trait ReleaseSource {
    /// The newest official stable release, or `None` when the repository has
    /// published none.
    ///
    /// # Errors
    /// Returns transport, rate-limit, or malformed-response failures. Callers
    /// treat all of them as "no conclusion".
    fn latest_stable(&self) -> Result<Option<Release>, ReleaseError>;
}

/// Raw shape of the GitHub releases endpoint. Unknown fields are ignored, so
/// the API may grow without breaking The Senate.
#[derive(Debug, Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Debug, Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Release policy, applied to a decoded listing.
///
/// Drafts and prereleases are rejected outright, as are tags that are not
/// canonical `vMAJOR.MINOR.PATCH`. A tag carrying semver pre-release
/// identifiers (`v0.2.0-rc.1`) is a prerelease whatever the flag says, and is
/// rejected too. The newest remaining release wins by semantic order, never by
/// list position or string comparison.
fn select_stable(raw: Vec<RawRelease>) -> Option<Release> {
    raw.into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            let version = canonical_tag_version(&release.tag_name)?;
            Some(Release {
                version,
                tag: release.tag_name,
                url: release.html_url,
                published_at: release.published_at,
                assets: release
                    .assets
                    .into_iter()
                    .map(|asset| ReleaseAsset {
                        name: asset.name,
                        download_url: asset.browser_download_url,
                        size: asset.size,
                    })
                    .collect(),
            })
        })
        .max_by(|left, right| left.version.cmp(&right.version))
}

/// Canonical The Senate tags are `v` followed by a plain semver release.
///
/// This is the single rule for what counts as an official release tag: the
/// updater uses it to decide which releases exist, and the release workflow
/// uses it — through `senate __verify-release-tag` — to decide whether a
/// tag may be published at all. Both call this function, so the two can never
/// disagree about a tag's shape.
#[must_use]
pub fn canonical_tag_version(tag: &str) -> Option<Version> {
    let version = Version::parse(tag.strip_prefix('v')?).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

/// The real source: the public GitHub REST API.
pub struct GitHubReleases {
    repository: String,
    timeout: Duration,
}

impl GitHubReleases {
    #[must_use]
    pub fn new(repository: impl Into<String>, timeout: Duration) -> Self {
        Self {
            repository: repository.into(),
            timeout,
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/releases?per_page={RELEASE_PAGE_SIZE}",
            self.repository
        )
    }

    /// Identifies The Senate and its version, and nothing else about the host.
    fn user_agent() -> String {
        format!("senate/{}", super::CURRENT_VERSION)
    }
}

impl ReleaseSource for GitHubReleases {
    fn latest_stable(&self) -> Result<Option<Release>, ReleaseError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .user_agent(Self::user_agent())
            .build()
            .into();
        let mut response = agent
            .get(self.endpoint())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .map_err(|error| match &error {
                ureq::Error::StatusCode(403 | 429) => ReleaseError::RateLimited,
                _ => ReleaseError::Unreachable(error.to_string()),
            })?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|error| ReleaseError::Malformed(error.to_string()))?;
        let raw: Vec<RawRelease> = serde_json::from_str(&body)
            .map_err(|error| ReleaseError::Malformed(error.to_string()))?;
        Ok(select_stable(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(json: &str) -> Option<Release> {
        select_stable(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn canonical_tags_parse_and_everything_else_is_rejected() {
        assert_eq!(
            canonical_tag_version("v0.2.0"),
            Some(Version::parse("0.2.0").unwrap())
        );
        assert_eq!(
            canonical_tag_version("v1.10.3"),
            Some(Version::parse("1.10.3").unwrap())
        );
        assert!(
            canonical_tag_version("0.2.0").is_none(),
            "the v prefix is canonical"
        );
        assert!(
            canonical_tag_version("release-2").is_none(),
            "malformed tag"
        );
        assert!(
            canonical_tag_version("v0.2").is_none(),
            "not a full semver release"
        );
        assert!(canonical_tag_version("vbanana").is_none());
        assert!(
            canonical_tag_version("v0.2.0-rc.1").is_none(),
            "a prerelease tag is never stable"
        );
        assert!(canonical_tag_version("v0.2.0+build.7").is_none());
    }

    #[test]
    fn drafts_and_prereleases_are_ignored() {
        let latest = listing(
            r#"[
              {"tag_name":"v0.4.0","draft":true,"prerelease":false,"html_url":"u"},
              {"tag_name":"v0.3.0","draft":false,"prerelease":true,"html_url":"u"},
              {"tag_name":"v0.2.0","draft":false,"prerelease":false,"html_url":"u"}
            ]"#,
        );
        assert_eq!(latest.unwrap().version, Version::parse("0.2.0").unwrap());
    }

    #[test]
    fn the_newest_release_wins_semantically_not_lexicographically() {
        let latest = listing(
            r#"[
              {"tag_name":"v0.9.0","draft":false,"prerelease":false,"html_url":"u"},
              {"tag_name":"v0.10.0","draft":false,"prerelease":false,"html_url":"u"}
            ]"#,
        );
        assert_eq!(
            latest.unwrap().version,
            Version::parse("0.10.0").unwrap(),
            "0.10.0 beats 0.9.0 even though it sorts earlier as a string"
        );
    }

    #[test]
    fn a_listing_without_usable_releases_yields_nothing() {
        assert!(listing("[]").is_none());
        assert!(
            listing(r#"[{"tag_name":"nightly","draft":false,"prerelease":false}]"#).is_none(),
            "malformed tags never become a release"
        );
    }

    #[test]
    fn assets_and_publication_time_survive_decoding() {
        let latest = listing(
            r#"[{"tag_name":"v0.2.0","draft":false,"prerelease":false,
                 "html_url":"https://example.invalid/r",
                 "published_at":"2026-08-22T10:00:00Z",
                 "assets":[{"name":"senate-aarch64-apple-darwin",
                            "browser_download_url":"https://example.invalid/a","size":42}]}]"#,
        )
        .unwrap();
        assert_eq!(latest.url, "https://example.invalid/r");
        assert!(latest.published_at.is_some());
        let asset = latest.asset("senate-aarch64-apple-darwin").unwrap();
        assert_eq!(asset.size, 42);
        assert!(latest.asset("senate-x86_64-unknown-linux-gnu").is_none());
    }

    #[test]
    fn the_user_agent_identifies_senate_and_nothing_else() {
        let agent = GitHubReleases::user_agent();
        assert!(agent.starts_with("senate/"));
        assert!(agent.contains(super::super::CURRENT_VERSION));
    }

    #[test]
    fn the_endpoint_is_the_documented_api_not_a_web_page() {
        let source = GitHubReleases::new("ember-mind/the-senate", Duration::from_secs(5));
        assert_eq!(
            source.endpoint(),
            "https://api.github.com/repos/ember-mind/the-senate/releases?per_page=20"
        );
    }
}
