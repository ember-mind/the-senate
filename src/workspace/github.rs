//! Minimal GitHub CLI (`gh`) boundary for opening pull requests.
//!
//! Publishing is push-first: the branch reaches the remote through Git alone,
//! and everything here is the optional last step that turns a pushed branch
//! into a pull request. Every failure is therefore reported as an outcome the
//! caller can present, never as an error that undoes the push.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};

/// Why a pull request could not be opened, in words meant for the operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhUnavailable(pub String);

/// Where one pull request reads its commits from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestHead {
    pub branch: String,
    /// True when the head branch lives in a fork. The Senate pushes to `origin`
    /// alone, so a fork's branch is not a branch it can update.
    pub cross_repository: bool,
}

pub struct GhClient {
    executable: OsString,
}

impl Default for GhClient {
    fn default() -> Self {
        Self {
            executable: OsString::from("gh"),
        }
    }
}

impl GhClient {
    /// Uses a specific `gh` executable instead of the one on `PATH`.
    #[must_use]
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    /// Returns the URL of an open pull request whose head is `branch`, if one
    /// exists.
    pub(crate) fn existing_pull_request(
        &self,
        cwd: &Path,
        branch: &str,
    ) -> Result<Option<String>, GhUnavailable> {
        let stdout = self.run(
            cwd,
            &[
                OsStr::new("pr"),
                OsStr::new("list"),
                OsStr::new("--head"),
                OsStr::new(branch),
                OsStr::new("--json"),
                OsStr::new("url"),
                OsStr::new("--jq"),
                OsStr::new(".[].url"),
            ],
        )?;
        Ok(stdout.lines().next().map(str::to_owned))
    }

    /// Reads the head of `pull_request`: the branch its commits are read from,
    /// and whether that branch lives in a fork rather than in the repository
    /// the pull request targets.
    pub(crate) fn pull_request_head(
        &self,
        cwd: &Path,
        pull_request: &PullRequestRef,
    ) -> Result<PullRequestHead, GhUnavailable> {
        let url = pull_request.url();
        let stdout = self.run(
            cwd,
            &[
                OsStr::new("pr"),
                OsStr::new("view"),
                OsStr::new(&url),
                OsStr::new("--json"),
                OsStr::new("headRefName,isCrossRepository"),
                OsStr::new("--jq"),
                OsStr::new(".headRefName + \"\\t\" + (.isCrossRepository | tostring)"),
            ],
        )?;
        let line = stdout.lines().next().unwrap_or_default();
        let (branch, cross) = line.split_once('\t').ok_or_else(|| {
            GhUnavailable(format!("gh pr view reported an unreadable head: {stdout}"))
        })?;
        if branch.is_empty() {
            return Err(GhUnavailable(
                "gh pr view reported a pull request with no head branch".to_owned(),
            ));
        }
        Ok(PullRequestHead {
            branch: branch.to_owned(),
            cross_repository: cross.trim() == "true",
        })
    }

    /// Opens a pull request from `branch` against the repository's default
    /// branch and returns its URL.
    pub(crate) fn create_pull_request(
        &self,
        cwd: &Path,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<String, GhUnavailable> {
        let stdout = self.run(
            cwd,
            &[
                OsStr::new("pr"),
                OsStr::new("create"),
                OsStr::new("--head"),
                OsStr::new(branch),
                OsStr::new("--title"),
                OsStr::new(title),
                OsStr::new("--body"),
                OsStr::new(body),
            ],
        )?;
        // gh prints the new pull request's URL as the last stdout line.
        stdout
            .lines()
            .rev()
            .find(|line| line.starts_with("https://"))
            .map(str::to_owned)
            .ok_or_else(|| {
                GhUnavailable(format!(
                    "gh pr create reported success without a pull request URL: {stdout}"
                ))
            })
    }

    fn run(&self, cwd: &Path, args: &[&OsStr]) -> Result<String, GhUnavailable> {
        // Tests exec a stub `gh` script the moment they write it, which can
        // still race another test thread's fork holding the write fd open
        // (`ETXTBSY`); retry_busy clears that window without hiding a real
        // failure.
        let output = crate::exec::retry_busy(|| {
            Command::new(&self.executable)
                .current_dir(cwd)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
        })
        .map_err(|source| match source.kind() {
                std::io::ErrorKind::NotFound => GhUnavailable(
                    "GitHub CLI (gh) is not installed — the branch is pushed; open the pull request manually or install gh".to_owned(),
                ),
                _ => GhUnavailable(format!("failed to run gh: {source}")),
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(GhUnavailable(format!(
                "gh {} failed: {}",
                args.first()
                    .and_then(|arg| arg.to_str())
                    .unwrap_or("command"),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

/// One pull request named by a task, as parsed from its URL.
///
/// The Senate never requires the pull request to live in the run's repository:
/// a review of a remote pull request is a legitimate run whose evidence
/// arrives over the network. What it does require is that the evidence can
/// actually arrive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestRef {
    pub host: String,
    pub owner: String,
    pub repository: String,
    pub number: u64,
}

impl PullRequestRef {
    /// First `https://<host>/<owner>/<repo>/pull/<number>` mentioned in `task`.
    #[must_use]
    pub fn parse(task: &str) -> Option<Self> {
        task.split_whitespace().find_map(Self::parse_url)
    }

    fn parse_url(candidate: &str) -> Option<Self> {
        let rest = candidate
            .trim_matches(|character: char| matches!(character, '<' | '>' | '(' | ')' | ',' | '.'))
            .strip_prefix("https://")
            .or_else(|| candidate.strip_prefix("http://"))?;
        let mut segments = rest.split('/');
        let host = segments.next()?;
        let owner = segments.next()?;
        let repository = segments.next()?;
        if segments.next()? != "pull" {
            return None;
        }
        let number = segments
            .next()?
            .split(|character: char| !character.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        if host.is_empty() || owner.is_empty() || repository.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_owned(),
            owner: owner.to_owned(),
            repository: repository.to_owned(),
            number,
        })
    }

    #[must_use]
    pub fn url(&self) -> String {
        let Self {
            host,
            owner,
            repository,
            number,
        } = self;
        format!("https://{host}/{owner}/{repository}/pull/{number}")
    }
}

/// Host, owner, and repository name read from a Git remote URL, in whichever
/// of the three shapes Git accepts: `https://host/owner/repo(.git)(/)`,
/// scp-style `git@host:owner/repo(.git)`, or `ssh://git@host/owner/repo(.git)`.
///
/// A checkout's `origin` and a pull request's URL name the same repository
/// through whichever shape their author's Git configuration happened to
/// prefer, so choosing between local checkouts by string-comparing raw remote
/// URLs would miss every match that used a different shape than the pull
/// request's own `https://` URL does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteRepository {
    host: String,
    owner: String,
    repository: String,
}

impl RemoteRepository {
    /// Parses a Git remote URL, or returns `None` for a shape this does not
    /// recognise — a local path, a shorthand alias, anything other than the
    /// three forms above.
    #[must_use]
    pub(crate) fn parse(url: &str) -> Option<Self> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .or_else(|| url.strip_prefix("ssh://git@"))
            .map(ToOwned::to_owned)
            .or_else(|| {
                let (host, path) = url.strip_prefix("git@")?.split_once(':')?;
                Some(format!("{host}/{path}"))
            })?;
        let mut segments = rest
            .trim_end_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty());
        let host = segments.next()?;
        let owner = segments.next()?;
        let repository = segments.next()?.trim_end_matches(".git");
        if host.is_empty() || owner.is_empty() || repository.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_owned(),
            owner: owner.to_owned(),
            repository: repository.to_owned(),
        })
    }

    /// Whether this remote names the same repository as `pull_request`.
    ///
    /// Compared case-insensitively: GitHub treats host, owner, and repository
    /// names case-insensitively, so a remote configured with different casing
    /// than a pull request's own URL still names the repository it belongs to.
    #[must_use]
    pub(crate) fn names(&self, pull_request: &PullRequestRef) -> bool {
        self.host.eq_ignore_ascii_case(&pull_request.host)
            && self.owner.eq_ignore_ascii_case(&pull_request.owner)
            && self
                .repository
                .eq_ignore_ascii_case(&pull_request.repository)
    }
}

/// Result of asking whether a run's stages could read the pull request at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullRequestReach {
    /// The pull request was read successfully.
    Reachable,
    /// The host answered, and the answer was "no": bad auth, missing
    /// repository, network failure. The run cannot succeed as written.
    Unreachable(String),
    /// Nothing could be concluded — `gh` is not installed, so a route unavailable
    /// to The Senate (an MCP provider, a pasted diff) may still work. Absence of
    /// `gh` is not evidence of absent access, so this never blocks a run.
    Unknown,
}

impl GhClient {
    /// The title of `pull_request`, when `gh` can read it.
    ///
    /// Decoration, not evidence: every failure — no `gh`, no login, no
    /// network — is simply no title.
    #[must_use]
    pub fn pull_request_title(&self, pull_request: &PullRequestRef) -> Option<String> {
        let PullRequestRef {
            host,
            owner,
            repository,
            number,
        } = pull_request;
        let output = crate::exec::retry_busy(|| {
            Command::new(&self.executable)
                .args([
                    OsString::from("api"),
                    OsString::from(format!("repos/{owner}/{repository}/pulls/{number}")),
                    OsString::from("--hostname"),
                    OsString::from(host),
                    OsString::from("--jq"),
                    OsString::from(".title"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
        })
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let title = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!title.is_empty()).then_some(title)
    }

    /// Whether `gh` can read `pull_request` right now.
    ///
    /// Runs before a run persists anything, so a pull request behind an
    /// expired login or an unreachable host stops the run at creation instead
    /// of stranding every stage on evidence that never arrives.
    #[must_use]
    pub fn pull_request_reach(&self, pull_request: &PullRequestRef) -> PullRequestReach {
        let PullRequestRef {
            host,
            owner,
            repository,
            number,
        } = pull_request;
        // See the comment in `run`: a stub `gh` can still be draining an
        // inherited write fd from another test's fork when this execs it.
        let output = crate::exec::retry_busy(|| {
            Command::new(&self.executable)
                .args([
                    OsString::from("api"),
                    OsString::from(format!("repos/{owner}/{repository}/pulls/{number}")),
                    OsString::from("--hostname"),
                    OsString::from(host),
                    OsString::from("--jq"),
                    OsString::from(".number"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
        });
        match output {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                PullRequestReach::Unknown
            }
            Err(source) => PullRequestReach::Unreachable(format!("failed to run gh: {source}")),
            Ok(output) if output.status.success() => PullRequestReach::Reachable,
            Ok(output) => {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                PullRequestReach::Unreachable(if detail.is_empty() {
                    format!("gh api exited with {}", output.status)
                } else {
                    detail
                })
            }
        }
    }
}

#[cfg(test)]
mod pull_request_ref_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn parses_the_enterprise_pull_request_url_that_started_run_01m1k8ka() {
        let parsed =
            PullRequestRef::parse("https://github.a8c.com/Automattic/missioncontrol/pull/164318")
                .expect("a pull request URL");
        assert_eq!(
            parsed,
            PullRequestRef {
                host: "github.a8c.com".to_owned(),
                owner: "Automattic".to_owned(),
                repository: "missioncontrol".to_owned(),
                number: 164_318,
            }
        );
        assert_eq!(
            parsed.url(),
            "https://github.a8c.com/Automattic/missioncontrol/pull/164318"
        );
    }

    #[test]
    fn finds_a_pull_request_named_inside_a_longer_task() {
        let parsed = PullRequestRef::parse(
            "Review https://github.com/Automattic/wp-calypso/pull/12#discussion_r1 for regressions",
        )
        .expect("a pull request URL");
        assert_eq!(parsed.repository, "wp-calypso");
        assert_eq!(parsed.number, 12);
    }

    #[test]
    fn tasks_without_a_pull_request_url_are_left_alone() {
        for task in [
            "Fix the flaky scheduler test",
            "https://github.com/Automattic/wp-calypso",
            "https://github.com/Automattic/wp-calypso/issues/12",
            "see pull/12",
        ] {
            assert!(PullRequestRef::parse(task).is_none(), "{task}");
        }
    }

    fn stub_gh(directory: &std::path::Path, body: &str) -> GhClient {
        let script = directory.join("gh");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        GhClient::with_executable(script)
    }

    fn pull_request() -> PullRequestRef {
        PullRequestRef {
            host: "github.a8c.com".to_owned(),
            owner: "Automattic".to_owned(),
            repository: "missioncontrol".to_owned(),
            number: 164_318,
        }
    }

    #[test]
    fn a_readable_pull_request_is_reachable() {
        let temp = tempfile::tempdir().unwrap();
        let gh = stub_gh(temp.path(), "echo 164318");
        assert_eq!(
            gh.pull_request_reach(&pull_request()),
            PullRequestReach::Reachable
        );
    }

    #[test]
    fn a_pull_request_title_is_read_and_any_failure_is_no_title() {
        let temp = tempfile::tempdir().unwrap();
        let gh = stub_gh(temp.path(), "echo 'Fix the checkout race'");
        assert_eq!(
            gh.pull_request_title(&pull_request()).as_deref(),
            Some("Fix the checkout race")
        );
        let failing = tempfile::tempdir().unwrap();
        let gh = stub_gh(failing.path(), "echo 'i/o timeout' >&2\nexit 1");
        assert_eq!(gh.pull_request_title(&pull_request()), None);
        let missing = GhClient::with_executable(temp.path().join("no-such-gh"));
        assert_eq!(missing.pull_request_title(&pull_request()), None);
    }

    /// The real failure behind run `01M1K8KAJ1HMS47H7WR8YMN2PW`: the host is
    /// only reachable over VPN, and `gh` times out at the TCP dial.
    #[test]
    fn a_host_that_cannot_be_dialled_is_unreachable_with_its_own_words() {
        let temp = tempfile::tempdir().unwrap();
        let gh = stub_gh(
            temp.path(),
            "echo 'Get \"https://github.a8c.com/api/v3/repos/Automattic/missioncontrol/pulls/164318\": dial tcp 192.0.80.209:443: i/o timeout' >&2\nexit 1",
        );
        let PullRequestReach::Unreachable(detail) = gh.pull_request_reach(&pull_request()) else {
            panic!("a timing-out host is not reachable");
        };
        assert!(detail.contains("i/o timeout"), "{detail}");
    }

    /// Absence of `gh` says nothing about access: an MCP provider or a pasted
    /// diff may still carry the pull request, so the run is never blocked on it.
    #[test]
    fn a_missing_gh_concludes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let gh = GhClient::with_executable(temp.path().join("no-such-gh"));
        assert_eq!(
            gh.pull_request_reach(&pull_request()),
            PullRequestReach::Unknown
        );
    }
}

#[cfg(test)]
mod remote_repository_tests {
    use super::*;

    fn jetpack() -> PullRequestRef {
        PullRequestRef {
            host: "github.com".to_owned(),
            owner: "Automattic".to_owned(),
            repository: "jetpack".to_owned(),
            number: 52_322,
        }
    }

    #[test]
    fn parses_an_https_remote_with_or_without_the_git_suffix_or_trailing_slash() {
        for url in [
            "https://github.com/Automattic/jetpack",
            "https://github.com/Automattic/jetpack.git",
            "https://github.com/Automattic/jetpack/",
            "https://github.com/Automattic/jetpack.git/",
        ] {
            let remote = RemoteRepository::parse(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(
                remote,
                RemoteRepository {
                    host: "github.com".to_owned(),
                    owner: "Automattic".to_owned(),
                    repository: "jetpack".to_owned(),
                },
                "{url}"
            );
            assert!(remote.names(&jetpack()), "{url}");
        }
    }

    #[test]
    fn parses_an_http_remote() {
        let remote = RemoteRepository::parse("http://github.com/Automattic/jetpack.git")
            .expect("an http remote");
        assert!(remote.names(&jetpack()));
    }

    #[test]
    fn parses_the_scp_style_shorthand() {
        let remote = RemoteRepository::parse("git@github.com:Automattic/jetpack.git")
            .expect("an scp-style remote");
        assert!(remote.names(&jetpack()));
    }

    #[test]
    fn parses_an_explicit_ssh_url() {
        let remote = RemoteRepository::parse("ssh://git@github.com/Automattic/jetpack.git")
            .expect("an ssh remote");
        assert!(remote.names(&jetpack()));
    }

    /// GitHub treats host, owner, and repository names case-insensitively, so
    /// a remote must still be recognised when its casing differs from the
    /// pull request URL's own.
    #[test]
    fn matching_ignores_case() {
        let remote = RemoteRepository::parse("https://GitHub.com/automattic/JetPack.git")
            .expect("a differently-cased remote");
        assert!(remote.names(&jetpack()));
    }

    #[test]
    fn a_remote_naming_a_different_repository_does_not_match() {
        let remote = RemoteRepository::parse("https://github.com/Automattic/wp-calypso").unwrap();
        assert!(!remote.names(&jetpack()));
    }

    #[test]
    fn unrecognised_shapes_parse_to_none() {
        for url in [
            "../relative/path",
            "/absolute/path",
            "not a url at all",
            "https://github.com/only-owner",
        ] {
            assert!(RemoteRepository::parse(url).is_none(), "{url}");
        }
    }
}
