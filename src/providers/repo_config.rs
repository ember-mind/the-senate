//! Which checkout answers for a run's repository settings.
//!
//! `.senate.toml` is the repository's own file — its `[verify]` table and
//! its `[permissions]` table — and the run's worktree is where it is read
//! from, so a change the run itself makes to it is what takes effect.
//!
//! That is the whole story only for a repository The Senate may commit to. A
//! run against a checkout of someone else's repository has nowhere to put
//! the file: the worktree is a fresh checkout of a tracked tree, so an
//! untracked file added beside it does not exist there, and committing one
//! upstream to configure a local tool is not a trade anybody should have to
//! make. Without an answer such a repository verifies by build-file guess
//! forever — `package.json` implies `npm test`, which for a monorepo is the
//! wrong suite at the wrong scope and can be red for reasons no change
//! caused.
//!
//! So the source repository the worktree was cut from answers second. An
//! untracked `.senate.toml` there configures every run of that repository
//! without touching a tracked file. The worktree still wins when it has one,
//! which keeps the original property intact, and every reader reports which
//! of the two answered so a stage can be read back to the file that
//! configured it.

use std::path::{Path, PathBuf};

/// The per-repository configuration file, shared by every table's reader.
pub const CONFIG_FILE: &str = ".senate.toml";

/// Which checkout the file was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigOrigin {
    /// The run's own worktree; the run can change it.
    Worktree,
    /// The repository the worktree was cut from; usually untracked there.
    SourceRepo,
}

impl ConfigOrigin {
    /// How the origin is named in an artifact or an error, so a reader can
    /// find the file that produced the behaviour they are looking at.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::SourceRepo => "source repository",
        }
    }
}

/// A configuration file that exists, and where it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigLocation {
    pub path: PathBuf,
    pub origin: ConfigOrigin,
}

/// Finds the configuration file that applies to one run.
///
/// The worktree first, then the source repository it was cut from. `None`
/// for a source repository that is not known — the caller could not load the
/// workspace row — which leaves the behaviour exactly what it was before
/// this fallback existed.
pub(crate) fn locate(worktree: &Path, source_repo: Option<&Path>) -> Option<ConfigLocation> {
    let in_worktree = worktree.join(CONFIG_FILE);
    if in_worktree.is_file() {
        return Some(ConfigLocation {
            path: in_worktree,
            origin: ConfigOrigin::Worktree,
        });
    }
    // A source repository equal to the worktree is the same file under
    // another name; it was already checked and it was not there.
    let source_repo = source_repo.filter(|path| *path != worktree)?;
    let in_source = source_repo.join(CONFIG_FILE);
    in_source.is_file().then_some(ConfigLocation {
        path: in_source,
        origin: ConfigOrigin::SourceRepo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(directory: &Path, text: &str) {
        std::fs::write(directory.join(CONFIG_FILE), text).expect("write config");
    }

    #[test]
    fn the_worktree_file_wins_so_a_run_can_change_what_applies_to_it() {
        let worktree = tempfile::tempdir().expect("worktree");
        let source = tempfile::tempdir().expect("source repo");
        write_config(worktree.path(), "[verify]\ncommands = [\"in worktree\"]\n");
        write_config(source.path(), "[verify]\ncommands = [\"in source\"]\n");

        let found = locate(worktree.path(), Some(source.path())).expect("a file");

        assert_eq!(found.path, worktree.path().join(CONFIG_FILE));
        assert_eq!(found.origin, ConfigOrigin::Worktree);
    }

    #[test]
    fn the_source_repository_answers_when_the_worktree_has_no_file() {
        let worktree = tempfile::tempdir().expect("worktree");
        let source = tempfile::tempdir().expect("source repo");
        write_config(source.path(), "[verify]\ncommands = [\"in source\"]\n");

        let found = locate(worktree.path(), Some(source.path())).expect("a file");

        assert_eq!(found.path, source.path().join(CONFIG_FILE));
        assert_eq!(found.origin, ConfigOrigin::SourceRepo);
    }

    #[test]
    fn no_file_in_either_checkout_is_no_location() {
        let worktree = tempfile::tempdir().expect("worktree");
        let source = tempfile::tempdir().expect("source repo");

        assert_eq!(locate(worktree.path(), Some(source.path())), None);
    }

    #[test]
    fn an_unknown_source_repository_leaves_only_the_worktree() {
        let worktree = tempfile::tempdir().expect("worktree");
        assert_eq!(locate(worktree.path(), None), None);

        write_config(worktree.path(), "[verify]\ncommands = [\"in worktree\"]\n");
        let found = locate(worktree.path(), None).expect("a file");
        assert_eq!(found.origin, ConfigOrigin::Worktree);
    }

    #[test]
    fn a_source_repository_that_is_the_worktree_is_not_read_twice() {
        let directory = tempfile::tempdir().expect("worktree");

        assert_eq!(locate(directory.path(), Some(directory.path())), None);

        write_config(directory.path(), "[verify]\ncommands = [\"here\"]\n");
        let found = locate(directory.path(), Some(directory.path())).expect("a file");
        assert_eq!(found.origin, ConfigOrigin::Worktree);
    }
}
