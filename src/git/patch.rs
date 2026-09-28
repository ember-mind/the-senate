use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tempfile::{NamedTempFile, tempdir};

use super::command::{Git, os};
use super::{GitError, GitRepository};

pub(crate) fn source_is_clean(git: &Git, repository: &GitRepository) -> Result<bool, GitError> {
    tree_is_clean(git, repository.source_path())
}

/// Whether one working tree carries no change of any kind, tracked or not.
///
/// Written against a path rather than a [`GitRepository`] because the callers
/// that matter ask it about a managed worktree, which is a working tree
/// The Senate owns but never discovered as a repository of its own.
pub(crate) fn tree_is_clean(git: &Git, path: &Path) -> Result<bool, GitError> {
    let output = git.checked(
        path,
        &[
            os("status"),
            os("--porcelain=v1"),
            os("-z"),
            os("--untracked-files=all"),
        ],
    )?;
    Ok(output.stdout.is_empty())
}

/// Ephemeral index staging the exact run delta (tracked, untracked, deleted)
/// used by apply, preview, and change-evidence generation. The real worktree
/// index is never touched.
struct DeltaIndex {
    _temporary: tempfile::TempDir,
    environment: Vec<(OsString, OsString)>,
}

fn stage_delta_index(
    git: &Git,
    worktree_path: &Path,
    base_commit: &str,
) -> Result<DeltaIndex, GitError> {
    let temporary = tempdir().map_err(|source| GitError::CommandIo {
        command: "create temporary Git index directory".to_owned(),
        source,
    })?;
    let index_path = temporary.path().join("index");
    let environment = vec![(
        OsString::from("GIT_INDEX_FILE"),
        index_path.as_os_str().to_os_string(),
    )];
    git.checked_with(
        worktree_path,
        &[os("read-tree"), os(base_commit)],
        &environment,
    )?;
    git.checked_with(
        worktree_path,
        &[os("add"), os("-A"), os("--"), os(".")],
        &environment,
    )?;
    Ok(DeltaIndex {
        _temporary: temporary,
        environment,
    })
}

pub(crate) fn generate_patch(
    git: &Git,
    worktree_path: &Path,
    base_commit: &str,
) -> Result<Vec<u8>, GitError> {
    let index = stage_delta_index(git, worktree_path, base_commit)?;
    let output = git.checked_with(
        worktree_path,
        &[
            os("diff"),
            os("--cached"),
            os("--binary"),
            os("--full-index"),
            os(base_commit),
            os("--"),
        ],
        &index.environment,
    )?;
    Ok(output.stdout)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PatchPreview {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
}

pub(crate) fn generate_patch_preview(
    git: &Git,
    worktree_path: &Path,
    base_commit: &str,
    max_bytes: usize,
) -> Result<PatchPreview, GitError> {
    let index = stage_delta_index(git, worktree_path, base_commit)?;
    bounded_cached_diff(
        git,
        worktree_path,
        base_commit,
        &index.environment,
        max_bytes,
        true,
    )
}

/// Streams `git diff --cached` against `base_commit` into a bounded preview.
/// `binary` selects full binary patch content (apply/preview) versus textual
/// "Binary files ... differ" markers (change evidence for prompts).
fn bounded_cached_diff(
    git: &Git,
    worktree_path: &Path,
    base_commit: &str,
    environment: &[(OsString, OsString)],
    max_bytes: usize,
    binary: bool,
) -> Result<PatchPreview, GitError> {
    let mut args = vec![os("diff"), os("--cached")];
    if binary {
        args.push(os("--binary"));
    }
    args.extend([os("--full-index"), os(base_commit), os("--")]);
    let mut output = NamedTempFile::new().map_err(|source| GitError::CommandIo {
        command: "create temporary Git preview file".to_owned(),
        source,
    })?;
    git.checked_to_file(
        worktree_path,
        &args,
        environment,
        output.reopen().map_err(|source| GitError::CommandIo {
            command: "open temporary Git preview file".to_owned(),
            source,
        })?,
    )?;
    let total_bytes = output
        .as_file()
        .metadata()
        .map_err(|source| GitError::CommandIo {
            command: "inspect temporary Git preview file".to_owned(),
            source,
        })?
        .len();
    output
        .seek(SeekFrom::Start(0))
        .map_err(|source| GitError::CommandIo {
            command: "seek temporary Git preview file".to_owned(),
            source,
        })?;
    let limit = u64::try_from(max_bytes).map_err(|_| {
        GitError::InvalidOutput("diff preview byte limit is outside supported range".to_owned())
    })?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    output
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|source| GitError::CommandIo {
            command: "read temporary Git preview file".to_owned(),
            source,
        })?;
    Ok(PatchPreview {
        bytes,
        total_bytes,
        truncated: total_bytes > limit,
    })
}

/// Category of one changed path in the run delta, derived from
/// `git diff --cached --name-status` over the same ephemeral index as apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Other,
}

impl ChangeKind {
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type-changed",
            Self::Other => "changed",
        }
    }

    fn from_status(status: &str) -> Self {
        match status.as_bytes().first() {
            Some(b'A') => Self::Added,
            Some(b'M') => Self::Modified,
            Some(b'D') => Self::Deleted,
            Some(b'R') => Self::Renamed,
            Some(b'C') => Self::Copied,
            Some(b'T') => Self::TypeChanged,
            _ => Self::Other,
        }
    }
}

/// One changed path in the run delta (worktree vs immutable run base).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangedFileRecord {
    pub kind: ChangeKind,
    pub path: String,
    pub previous_path: Option<String>,
    pub binary: bool,
}

/// Deterministic change evidence sharing exact apply/preview delta semantics:
/// full changed-file inventory plus a bounded textual diff that never embeds
/// binary contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeEvidence {
    pub files: Vec<ChangedFileRecord>,
    pub diff: PatchPreview,
}

pub(crate) fn generate_change_evidence(
    git: &Git,
    worktree_path: &Path,
    base_commit: &str,
    max_diff_bytes: usize,
) -> Result<ChangeEvidence, GitError> {
    let index = stage_delta_index(git, worktree_path, base_commit)?;
    let name_status = git.checked_with(
        worktree_path,
        &[
            os("diff"),
            os("--cached"),
            os("--name-status"),
            os("-z"),
            os(base_commit),
            os("--"),
        ],
        &index.environment,
    )?;
    let numstat = git.checked_with(
        worktree_path,
        &[
            os("diff"),
            os("--cached"),
            os("--numstat"),
            os("-z"),
            os(base_commit),
            os("--"),
        ],
        &index.environment,
    )?;
    let binary_paths = parse_binary_paths(&numstat.stdout)?;
    let mut files = parse_name_status(&name_status.stdout)?;
    for file in &mut files {
        file.binary = binary_paths.contains(&file.path);
    }
    let diff = bounded_cached_diff(
        git,
        worktree_path,
        base_commit,
        &index.environment,
        max_diff_bytes,
        false,
    )?;
    Ok(ChangeEvidence { files, diff })
}

fn parse_name_status(stdout: &[u8]) -> Result<Vec<ChangedFileRecord>, GitError> {
    let mut records = Vec::new();
    let mut tokens = stdout
        .split(|byte| *byte == 0)
        .filter(|token| !token.is_empty())
        .map(|token| String::from_utf8_lossy(token).into_owned());
    while let Some(status) = tokens.next() {
        let kind = ChangeKind::from_status(&status);
        let (path, previous_path) = if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
            let old = tokens.next().ok_or_else(|| {
                GitError::InvalidOutput(
                    "name-status rename entry is missing source path".to_owned(),
                )
            })?;
            let new = tokens.next().ok_or_else(|| {
                GitError::InvalidOutput(
                    "name-status rename entry is missing destination path".to_owned(),
                )
            })?;
            (new, Some(old))
        } else {
            let path = tokens.next().ok_or_else(|| {
                GitError::InvalidOutput("name-status entry is missing path".to_owned())
            })?;
            (path, None)
        };
        records.push(ChangedFileRecord {
            kind,
            path,
            previous_path,
            binary: false,
        });
    }
    Ok(records)
}

/// Parses `--numstat -z` output; binary changes report `-` counters.
/// Rename entries use an empty inline path followed by two NUL-separated paths.
fn parse_binary_paths(stdout: &[u8]) -> Result<std::collections::BTreeSet<String>, GitError> {
    let mut binary = std::collections::BTreeSet::new();
    let mut tokens = stdout
        .split(|byte| *byte == 0)
        .filter(|token| !token.is_empty())
        .map(|token| String::from_utf8_lossy(token).into_owned());
    while let Some(entry) = tokens.next() {
        let mut fields = entry.splitn(3, '\t');
        let added = fields.next().unwrap_or_default().to_owned();
        let deleted = fields.next().unwrap_or_default().to_owned();
        let inline_path = fields.next().map(str::to_owned);
        let path = match inline_path {
            Some(path) if !path.is_empty() => path,
            _ => {
                let _old = tokens.next().ok_or_else(|| {
                    GitError::InvalidOutput(
                        "numstat rename entry is missing source path".to_owned(),
                    )
                })?;
                tokens.next().ok_or_else(|| {
                    GitError::InvalidOutput(
                        "numstat rename entry is missing destination path".to_owned(),
                    )
                })?
            }
        };
        if added == "-" && deleted == "-" {
            binary.insert(path);
        }
    }
    Ok(binary)
}

pub(crate) fn check_patch(
    git: &Git,
    repository: &GitRepository,
    patch: &[u8],
    reverse: bool,
) -> Result<bool, GitError> {
    let patch_file = temporary_patch(patch)?;
    let mut args = vec![os("apply"), os("--check"), os("--binary")];
    if reverse {
        args.push(os("--reverse"));
    }
    args.push(patch_file.path().as_os_str().to_os_string());
    let output = git.output(repository.source_path(), &args, &[])?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(output.into_failure())
    }
}

pub(crate) fn apply_patch(
    git: &Git,
    repository: &GitRepository,
    patch: &[u8],
) -> Result<(), GitError> {
    let patch_file = temporary_patch(patch)?;
    git.checked_with(
        repository.source_path(),
        &[
            os("apply"),
            os("--binary"),
            patch_file.path().as_os_str().to_os_string(),
        ],
        &[],
    )?;
    Ok(())
}

fn temporary_patch(patch: &[u8]) -> Result<NamedTempFile, GitError> {
    let mut file = NamedTempFile::new().map_err(|source| GitError::CommandIo {
        command: "create temporary Git patch file".to_owned(),
        source,
    })?;
    file.write_all(patch)
        .and_then(|()| file.flush())
        .map_err(|source| GitError::CommandIo {
            command: "write temporary Git patch file".to_owned(),
            source,
        })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn git_cmd(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn fixture() -> (TempDir, PathBuf, String) {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_cmd(&repo, &["init", "-b", "main"]);
        git_cmd(&repo, &["config", "user.name", "The Senate Test"]);
        git_cmd(&repo, &["config", "user.email", "senate@example.invalid"]);
        git_cmd(&repo, &["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("kept.rs"), "kept\n").unwrap();
        fs::write(repo.join("modified.rs"), "before\n").unwrap();
        fs::write(repo.join("deleted.rs"), "gone soon\n").unwrap();
        fs::write(repo.join("logo.bin"), [0_u8, 159, 146, 150, 0, 1]).unwrap();
        git_cmd(&repo, &["add", "-A"]);
        git_cmd(&repo, &["commit", "-m", "base"]);
        let base = git_cmd(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        (temp, repo, base)
    }

    fn mutate_worktree(repo: &Path) {
        fs::write(repo.join("modified.rs"), "after\n").unwrap();
        fs::remove_file(repo.join("deleted.rs")).unwrap();
        fs::write(repo.join("untracked_new.rs"), "brand new\n").unwrap();
        fs::write(repo.join("logo.bin"), [0_u8, 200, 201, 0, 2, 3, 4]).unwrap();
    }

    #[test]
    fn change_evidence_lists_delta_against_persisted_base_including_untracked() {
        let (_temp, repo, base) = fixture();
        mutate_worktree(&repo);
        // HEAD moving after the base is captured must not change the delta.
        fs::write(repo.join("kept.rs"), "kept\n").unwrap();
        let evidence =
            generate_change_evidence(&Git::default(), &repo, &base, 1024 * 1024).unwrap();
        let mut names: Vec<(&str, ChangeKind, bool)> = evidence
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.kind, file.binary))
            .collect();
        names.sort_unstable_by(|left, right| left.0.cmp(right.0));
        assert_eq!(
            names,
            vec![
                ("deleted.rs", ChangeKind::Deleted, false),
                ("logo.bin", ChangeKind::Modified, true),
                ("modified.rs", ChangeKind::Modified, false),
                ("untracked_new.rs", ChangeKind::Added, false),
            ]
        );
        assert!(!evidence.diff.truncated);
        let text = String::from_utf8_lossy(&evidence.diff.bytes).into_owned();
        assert!(text.contains("+after"));
        assert!(text.contains("+brand new"));
        assert!(text.contains("Binary files a/logo.bin and b/logo.bin differ"));
        assert!(!text.contains("GIT binary patch"));
    }

    #[test]
    fn change_evidence_diff_is_bounded_with_explicit_truncation() {
        let (_temp, repo, base) = fixture();
        mutate_worktree(&repo);
        let evidence = generate_change_evidence(&Git::default(), &repo, &base, 16).unwrap();
        assert!(evidence.diff.truncated);
        assert_eq!(evidence.diff.bytes.len(), 16);
        assert!(evidence.diff.total_bytes > 16);
        // Bounding the diff never bounds the changed-file inventory.
        assert_eq!(evidence.files.len(), 4);
    }

    #[test]
    fn change_evidence_derivation_is_read_only_for_the_worktree() {
        let (_temp, repo, base) = fixture();
        mutate_worktree(&repo);
        let status_before = git_cmd(&repo, &["status", "--porcelain=v1", "-z"]);
        let index_before = git_cmd(&repo, &["diff", "--cached"]);
        generate_change_evidence(&Git::default(), &repo, &base, 1024).unwrap();
        assert_eq!(
            git_cmd(&repo, &["status", "--porcelain=v1", "-z"]),
            status_before
        );
        assert_eq!(git_cmd(&repo, &["diff", "--cached"]), index_before);
    }

    #[test]
    fn change_evidence_matches_apply_patch_delta_semantics() {
        let (_temp, repo, base) = fixture();
        mutate_worktree(&repo);
        let evidence =
            generate_change_evidence(&Git::default(), &repo, &base, 1024 * 1024).unwrap();
        let patch = generate_patch(&Git::default(), &repo, &base).unwrap();
        let patch_text = String::from_utf8_lossy(&patch);
        for file in &evidence.files {
            assert!(
                patch_text.contains(&format!("a/{}", file.path))
                    || patch_text.contains(&format!("b/{}", file.path)),
                "apply patch is missing {}",
                file.path
            );
        }
    }
}
