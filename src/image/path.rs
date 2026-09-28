//! Where a generated image may land: only inside the managed worktree of the
//! run that asked for it, and never on top of something that already exists.
//!
//! `output_path` is agent input. It is checked before any vendor call so a
//! bad path costs nothing: syntactically (relative, plain components, not
//! under `.git`, `.png`), then physically against the deepest ancestor that
//! already exists, canonicalized, which is what defeats a symlink pointing
//! out of the worktree. The write repeats the physical check after creating
//! missing parents, and links the temporary file to the final name so an
//! existing file is never replaced.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

/// Why an output path was refused. Wording is agent-facing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OutputPathError {
    #[error("output_path must be a relative path inside the project")]
    NotRelative,
    #[error("output_path must not contain `..`, `.`, or empty components")]
    Traversal,
    #[error("output_path must not point inside `.git`")]
    GitDirectory,
    #[error("output_path must end with `.png`; v1 writes PNG only")]
    NotPng,
    #[error("output_path resolves outside the run's managed worktree")]
    OutsideWorktree,
    #[error("output_path already exists; generation never overwrites a project file")]
    AlreadyExists,
    #[error("output_path could not be written: {0}")]
    Io(String),
}

/// A validated destination: the canonical worktree, the worktree-relative
/// form for evidence, and the intended absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedOutput {
    worktree: PathBuf,
    pub relative: String,
    pub absolute: PathBuf,
}

/// Validates `output_path` against `worktree` without creating anything.
///
/// # Errors
/// Returns the first rule the path breaks.
pub fn validate_output_path(
    worktree: &Path,
    output_path: &str,
) -> Result<ValidatedOutput, OutputPathError> {
    if output_path.is_empty() || output_path.contains('\0') {
        return Err(OutputPathError::Traversal);
    }
    let candidate = Path::new(output_path);
    if candidate.is_absolute() || output_path.starts_with('/') || output_path.starts_with('~') {
        return Err(OutputPathError::NotRelative);
    }
    // `Path::components` folds `.` and `//` away; the raw string is checked
    // first so the agent's spelling is refused, not silently normalized.
    if output_path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(OutputPathError::Traversal);
    }
    let mut relative = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                if part == ".git" {
                    return Err(OutputPathError::GitDirectory);
                }
                relative.push(part);
            }
            _ => return Err(OutputPathError::Traversal),
        }
    }
    if relative.as_os_str().is_empty() || output_path.ends_with('/') {
        return Err(OutputPathError::Traversal);
    }
    let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
        return Err(OutputPathError::Traversal);
    };
    // Lowercase `.png` only, on purpose: one canonical extension for one
    // output type, so `.PNG` is refused rather than silently accepted.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    if !name.ends_with(".png") || name == ".png" {
        return Err(OutputPathError::NotPng);
    }
    let worktree =
        fs::canonicalize(worktree).map_err(|error| OutputPathError::Io(error.to_string()))?;
    let absolute = worktree.join(&relative);
    check_containment(&worktree, &absolute)?;
    if fs::symlink_metadata(&absolute).is_ok() {
        return Err(OutputPathError::AlreadyExists);
    }
    Ok(ValidatedOutput {
        worktree,
        relative: relative.to_string_lossy().into_owned(),
        absolute,
    })
}

/// The deepest existing ancestor of `absolute`, canonicalized, must still be
/// inside the canonical worktree. Missing directories below it are plain
/// names and cannot escape.
fn check_containment(worktree: &Path, absolute: &Path) -> Result<(), OutputPathError> {
    let mut probe = absolute
        .parent()
        .ok_or(OutputPathError::Traversal)?
        .to_path_buf();
    while fs::symlink_metadata(&probe).is_err() {
        probe = probe
            .parent()
            .ok_or(OutputPathError::OutsideWorktree)?
            .to_path_buf();
    }
    let canonical =
        fs::canonicalize(&probe).map_err(|error| OutputPathError::Io(error.to_string()))?;
    if canonical.starts_with(worktree) {
        Ok(())
    } else {
        Err(OutputPathError::OutsideWorktree)
    }
}

impl ValidatedOutput {
    /// Writes `bytes` to the validated destination without ever overwriting:
    /// missing parents are created, containment is re-checked against the
    /// canonical parent, a temporary file in that directory is written and
    /// fsynced, then hard-linked to the final name (which fails if the name
    /// now exists) and the temporary unlinked. Any failure leaves no partial
    /// file at the destination.
    ///
    /// # Errors
    /// Returns the I/O failure; `AlreadyExists` when the destination raced
    /// into existence; `OutsideWorktree` if a parent now escapes.
    pub(crate) fn write_no_overwrite(&self, bytes: &[u8]) -> Result<PathBuf, OutputPathError> {
        let parent = self.absolute.parent().ok_or(OutputPathError::Traversal)?;
        fs::create_dir_all(parent).map_err(|error| OutputPathError::Io(error.to_string()))?;
        let canonical_parent =
            fs::canonicalize(parent).map_err(|error| OutputPathError::Io(error.to_string()))?;
        if !canonical_parent.starts_with(&self.worktree) {
            return Err(OutputPathError::OutsideWorktree);
        }
        let name = self
            .absolute
            .file_name()
            .ok_or(OutputPathError::Traversal)?;
        let destination = canonical_parent.join(name);
        let temporary = canonical_parent.join(format!(".senate-image-{}.tmp", ulid::Ulid::new()));
        let result = (|| {
            let mut file = File::options()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| OutputPathError::Io(error.to_string()))?;
            file.write_all(bytes)
                .map_err(|error| OutputPathError::Io(error.to_string()))?;
            file.sync_all()
                .map_err(|error| OutputPathError::Io(error.to_string()))?;
            drop(file);
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(OutputPathError::AlreadyExists)
                }
                Err(error) => Err(OutputPathError::Io(error.to_string())),
            }
        })();
        let _ = fs::remove_file(&temporary);
        result.map(|()| destination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn worktree() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("assets")).unwrap();
        dir
    }

    #[test]
    fn plain_relative_png_is_accepted_and_nothing_is_created_until_write() {
        let dir = worktree();
        let output = validate_output_path(dir.path(), "assets/img/hero.png").unwrap();
        assert_eq!(output.relative, "assets/img/hero.png");
        assert!(!dir.path().join("assets/img").exists());
        let written = output.write_no_overwrite(b"png").unwrap();
        assert_eq!(
            written,
            fs::canonicalize(dir.path())
                .unwrap()
                .join("assets/img/hero.png")
        );
        assert_eq!(fs::read(&written).unwrap(), b"png");
        assert_eq!(
            fs::read_dir(dir.path().join("assets/img")).unwrap().count(),
            1
        );
    }

    #[test]
    fn absolute_and_home_paths_are_rejected() {
        let dir = worktree();
        let absolute = dir.path().join("assets/hero.png");
        for path in [absolute.to_str().unwrap(), "/etc/hero.png", "~/hero.png"] {
            assert_eq!(
                validate_output_path(dir.path(), path).unwrap_err(),
                OutputPathError::NotRelative,
                "{path:?}"
            );
        }
    }

    #[test]
    fn traversal_and_git_paths_are_rejected() {
        let dir = worktree();
        for path in [
            "../hero.png",
            "assets/../../hero.png",
            "assets/./hero.png",
            "./hero.png",
            "assets//hero.png",
            "",
            "assets/",
        ] {
            assert_eq!(
                validate_output_path(dir.path(), path).unwrap_err(),
                OutputPathError::Traversal,
                "{path:?}"
            );
        }
        assert_eq!(
            validate_output_path(dir.path(), ".git/hero.png").unwrap_err(),
            OutputPathError::GitDirectory
        );
        assert_eq!(
            validate_output_path(dir.path(), "assets/.git/hero.png").unwrap_err(),
            OutputPathError::GitDirectory
        );
    }

    #[test]
    fn only_png_is_accepted() {
        let dir = worktree();
        for path in [
            "assets/hero.jpg",
            "assets/hero",
            "assets/.png",
            "assets/hero.PNG",
        ] {
            assert_eq!(
                validate_output_path(dir.path(), path).unwrap_err(),
                OutputPathError::NotPng,
                "{path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_escaping_the_worktree_is_rejected_even_when_deep() {
        let dir = worktree();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
        for path in ["linked/hero.png", "linked/deeper/not/yet/hero.png"] {
            assert_eq!(
                validate_output_path(dir.path(), path).unwrap_err(),
                OutputPathError::OutsideWorktree,
                "{path:?}"
            );
        }
        assert!(!outside.path().join("hero.png").exists());
        // A symlink inside the worktree pointing inside it is fine.
        std::os::unix::fs::symlink(dir.path().join("assets"), dir.path().join("alias")).unwrap();
        let ok = validate_output_path(dir.path(), "alias/hero.png").unwrap();
        let written = ok.write_no_overwrite(b"x").unwrap();
        assert!(written.starts_with(fs::canonicalize(dir.path()).unwrap()));
        assert!(dir.path().join("assets/hero.png").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_in_between_validation_and_write_is_still_caught() {
        let dir = worktree();
        let outside = TempDir::new().unwrap();
        let output = validate_output_path(dir.path(), "later/hero.png").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("later")).unwrap();
        assert_eq!(
            output.write_no_overwrite(b"x").unwrap_err(),
            OutputPathError::OutsideWorktree
        );
        assert!(!outside.path().join("hero.png").exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_file_or_dangling_symlink_is_never_overwritten() {
        let dir = worktree();
        fs::write(dir.path().join("assets/hero.png"), b"project bytes").unwrap();
        assert_eq!(
            validate_output_path(dir.path(), "assets/hero.png").unwrap_err(),
            OutputPathError::AlreadyExists
        );
        std::os::unix::fs::symlink(
            "/nonexistent/target.png",
            dir.path().join("assets/dangling.png"),
        )
        .unwrap();
        assert_eq!(
            validate_output_path(dir.path(), "assets/dangling.png").unwrap_err(),
            OutputPathError::AlreadyExists
        );
        // And the writer refuses to clobber a file that appeared late.
        let output = validate_output_path(dir.path(), "assets/late.png").unwrap();
        fs::write(dir.path().join("assets/late.png"), b"late").unwrap();
        assert_eq!(
            output.write_no_overwrite(b"generated").unwrap_err(),
            OutputPathError::AlreadyExists
        );
        assert_eq!(
            fs::read(dir.path().join("assets/late.png")).unwrap(),
            b"late"
        );
        assert!(
            fs::read_dir(dir.path().join("assets"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );
    }
}
