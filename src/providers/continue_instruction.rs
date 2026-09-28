//! Immutable run-private storage for one continue cycle's operator
//! instruction.
//!
//! A follow-up stage's initial prompt needs the operator's free-text
//! instruction, but nothing about it may reach the provider through argv —
//! visible to `ps` — or become part of the domain event stream, which is
//! replayed as semantic history on every future read. Native adapters
//! already solve exactly this problem for a permission or question response:
//! the text is written once to a plain run-private file and stitched into the
//! next native invocation, never into argv or `SQLite`. This is the same
//! mechanism, addressed by run and stage rather than by provider session and
//! attention request, because the instruction exists before either of those
//! does — the run service persists it before the follow-up stage is even
//! appended, so whichever adapter later composes that stage's initial prompt,
//! on the first attempt or after a restart, finds the same text under the
//! same key.
//!
//! Restart determinism follows from the write being idempotent and the key
//! being deterministic: [`crate::domain::next_follow_up_stage_id`] predicts
//! the exact stage identity the cycle will use before it exists, so a
//! retried request after a crash writes the identical bytes under the
//! identical path rather than orphaning one file per attempt.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::domain::{RunId, StageId};

const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum ContinueInstructionError {
    #[error("continue instruction exceeds {0} bytes")]
    TooLarge(usize),
    #[error("continue instruction cannot be empty")]
    Empty,
    #[error("continue instruction path has no parent directory")]
    NoParent,
    #[error("continue instruction already exists with different content: {0}")]
    Conflict(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Where one follow-up stage's instruction lives, given the shared process
/// root every provider adapter already resolves its own run-private state
/// under.
fn path(root: &Path, run_id: RunId, stage_id: &StageId) -> PathBuf {
    root.join(run_id.to_string())
        .join("continue-instructions")
        .join(format!("{stage_id}.txt"))
}

/// Persists the operator's instruction for one follow-up stage, once.
///
/// Writing twice with the same bytes is a no-op; writing different bytes
/// under an identity that already has content is refused rather than
/// silently overwritten, the same write-once discipline every other
/// run-private artifact in this codebase uses.
///
/// # Errors
/// Returns [`ContinueInstructionError::Empty`] for blank input,
/// [`ContinueInstructionError::TooLarge`] over the byte ceiling,
/// [`ContinueInstructionError::Conflict`] for a mismatched existing file, and
/// wrapped I/O failures.
pub(crate) fn write_once(
    root: &Path,
    run_id: RunId,
    stage_id: &StageId,
    instruction: &str,
) -> Result<(), ContinueInstructionError> {
    use std::io::Write as _;

    if instruction.trim().is_empty() {
        return Err(ContinueInstructionError::Empty);
    }
    let bytes = instruction.as_bytes();
    if bytes.len() > MAX_INSTRUCTION_BYTES {
        return Err(ContinueInstructionError::TooLarge(MAX_INSTRUCTION_BYTES));
    }
    let target = path(root, run_id, stage_id);
    let directory = target.parent().ok_or(ContinueInstructionError::NoParent)?;
    std::fs::create_dir_all(directory)?;
    if target.exists() {
        return if std::fs::read(&target)? == bytes {
            Ok(())
        } else {
            Err(ContinueInstructionError::Conflict(target))
        };
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&target) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(&target)? != bytes {
                return Err(ContinueInstructionError::Conflict(target));
            }
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(())
}

/// Removes one follow-up stage's staged instruction, if present.
///
/// Walks back [`write_once`] when the domain transition it was staged for
/// never became durable — a refused or lost commit must not leave content
/// behind that a retry, possibly with different text, would then conflict
/// against. Removing a file that was never written is not an error: the
/// caller may not know whether its own write actually landed before the
/// failure it is unwinding.
///
/// # Errors
/// Returns wrapped I/O failures other than the file's expected absence.
pub(crate) fn discard(
    root: &Path,
    run_id: RunId,
    stage_id: &StageId,
) -> Result<(), ContinueInstructionError> {
    match std::fs::remove_file(path(root, run_id, stage_id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Reads back one follow-up stage's persisted instruction, if any invocation
/// of it has ever been asked for. `None` for any stage that never had one —
/// every stage kind other than `FollowUp` — so callers read unconditionally
/// only where they already know the answer can be absent.
///
/// # Errors
/// Returns wrapped I/O failures other than the file's expected absence.
pub(crate) fn read(
    root: &Path,
    run_id: RunId,
    stage_id: &StageId,
) -> Result<Option<String>, ContinueInstructionError> {
    match std::fs::read_to_string(path(root, run_id, stage_id)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_id() -> RunId {
        RunId::from_u128(1)
    }

    fn stage_id() -> StageId {
        StageId::new("followup_1").unwrap()
    }

    #[test]
    fn a_written_instruction_reads_back_verbatim() {
        let temp = tempfile::TempDir::new().unwrap();
        assert_eq!(read(temp.path(), run_id(), &stage_id()).unwrap(), None);

        write_once(
            temp.path(),
            run_id(),
            &stage_id(),
            "add tests for the edge case",
        )
        .unwrap();

        assert_eq!(
            read(temp.path(), run_id(), &stage_id()).unwrap().as_deref(),
            Some("add tests for the edge case")
        );
    }

    /// Restart determinism: a retried write with the same bytes succeeds
    /// rather than failing on the file it already wrote.
    #[test]
    fn writing_the_same_instruction_twice_is_idempotent() {
        let temp = tempfile::TempDir::new().unwrap();
        write_once(temp.path(), run_id(), &stage_id(), "same text").unwrap();
        write_once(temp.path(), run_id(), &stage_id(), "same text").unwrap();

        assert_eq!(
            read(temp.path(), run_id(), &stage_id()).unwrap().as_deref(),
            Some("same text")
        );
    }

    #[test]
    fn a_conflicting_rewrite_under_the_same_identity_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        write_once(temp.path(), run_id(), &stage_id(), "first instruction").unwrap();

        assert!(matches!(
            write_once(temp.path(), run_id(), &stage_id(), "different instruction"),
            Err(ContinueInstructionError::Conflict(_))
        ));
    }

    /// The failure-path cleanup this exists for: a write that never became
    /// durable domain state must not permanently block a retry that carries
    /// different text.
    #[test]
    fn a_discarded_instruction_lets_a_retry_with_different_text_succeed() {
        let temp = tempfile::TempDir::new().unwrap();
        write_once(temp.path(), run_id(), &stage_id(), "first instruction").unwrap();

        discard(temp.path(), run_id(), &stage_id()).unwrap();

        assert_eq!(read(temp.path(), run_id(), &stage_id()).unwrap(), None);
        write_once(temp.path(), run_id(), &stage_id(), "different instruction").unwrap();
        assert_eq!(
            read(temp.path(), run_id(), &stage_id()).unwrap().as_deref(),
            Some("different instruction")
        );
    }

    #[test]
    fn discarding_an_instruction_that_was_never_written_is_not_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        assert!(discard(temp.path(), run_id(), &stage_id()).is_ok());
    }

    #[test]
    fn blank_instructions_are_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        assert!(matches!(
            write_once(temp.path(), run_id(), &stage_id(), "   \n\t "),
            Err(ContinueInstructionError::Empty)
        ));
    }

    #[test]
    fn an_oversized_instruction_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let huge = "a".repeat(MAX_INSTRUCTION_BYTES + 1);
        assert!(matches!(
            write_once(temp.path(), run_id(), &stage_id(), &huge),
            Err(ContinueInstructionError::TooLarge(_))
        ));
    }
}
