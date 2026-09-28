use std::ffi::OsString;
use std::path::PathBuf;

use super::StoreError;

const DATABASE_FILE: &str = "senate.db";
const WORKTREE_DIRECTORY: &str = "worktrees";
const RUN_DIRECTORY: &str = "runs";
const UPDATE_CACHE_FILE: &str = "update.json";
const INSTALL_RECEIPT_FILE: &str = "install.json";
const WORLD_STATE_FILE: &str = "world.json";

/// Resolves default database path without creating directories or files.
///
/// # Errors
/// Returns [`StoreError::DataPathUnavailable`] when neither override nor home
/// directory is available.
pub fn database_file() -> Result<PathBuf, StoreError> {
    Ok(data_directory_with(|name| std::env::var_os(name))?.join(DATABASE_FILE))
}

/// Resolves central managed worktree root without creating it.
///
/// # Errors
/// Returns [`StoreError::DataPathUnavailable`] when neither override nor home
/// directory is available.
pub fn worktree_root() -> Result<PathBuf, StoreError> {
    Ok(data_directory_with(|name| std::env::var_os(name))?.join(WORKTREE_DIRECTORY))
}

/// Resolves the update-check cache path without creating it.
///
/// Update state is application cache state, so it lives beside the run store
/// rather than inside it, and never inside a user's repository.
///
/// # Errors
/// Returns [`StoreError::DataPathUnavailable`] when neither override nor home
/// directory is available.
pub fn update_cache_file() -> Result<PathBuf, StoreError> {
    Ok(data_directory_with(|name| std::env::var_os(name))?.join(UPDATE_CACHE_FILE))
}

/// Resolves the install-receipt path without creating it.
///
/// # Errors
/// Returns [`StoreError::DataPathUnavailable`] when neither override nor home
/// directory is available.
pub fn install_receipt_file() -> Result<PathBuf, StoreError> {
    Ok(data_directory_with(|name| std::env::var_os(name))?.join(INSTALL_RECEIPT_FILE))
}

/// Resolves durable per-run infrastructure root without creating it.
///
/// # Errors
/// Returns [`StoreError::DataPathUnavailable`] when neither override nor home
/// directory is available.
pub fn process_root() -> Result<PathBuf, StoreError> {
    Ok(data_directory_with(|name| std::env::var_os(name))?.join(RUN_DIRECTORY))
}

/// Resolves the single-instance marker the Senate server writes while it
/// runs, without creating it.
///
/// # Errors
/// Returns [`StoreError::DataPathUnavailable`] when neither override nor home
/// directory is available.
pub fn world_state_file() -> Result<PathBuf, StoreError> {
    Ok(data_directory_with(|name| std::env::var_os(name))?.join(WORLD_STATE_FILE))
}

#[cfg(test)]
fn database_file_with(
    mut get_var: impl FnMut(&str) -> Option<OsString>,
) -> Result<PathBuf, StoreError> {
    Ok(data_directory_with(&mut get_var)?.join(DATABASE_FILE))
}

fn data_directory_with(
    mut get_var: impl FnMut(&str) -> Option<OsString>,
) -> Result<PathBuf, StoreError> {
    if let Some(directory) = non_empty(get_var("SENATE_DATA_DIR")) {
        return Ok(PathBuf::from(directory));
    }
    let home = non_empty(get_var("HOME")).ok_or(StoreError::DataPathUnavailable)?;
    Ok(PathBuf::from(home).join(".senate"))
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_and_resolution_has_no_side_effects() {
        let path = database_file_with(|name| match name {
            "SENATE_DATA_DIR" => Some(OsString::from("/tmp/senate-data")),
            "HOME" => Some(OsString::from("/home/ignored")),
            _ => None,
        })
        .unwrap();
        assert_eq!(path, PathBuf::from("/tmp/senate-data/senate.db"));
    }

    #[test]
    fn home_fallback_matches_contract() {
        let path =
            database_file_with(|name| (name == "HOME").then(|| OsString::from("/home/user")))
                .unwrap();
        assert_eq!(path, PathBuf::from("/home/user/.senate/senate.db"));
    }
}
