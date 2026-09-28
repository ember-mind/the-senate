use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

const APP_DIRECTORY: &str = "senate";
const CONFIG_FILE: &str = "config.toml";

/// Returns the user-level configuration file path without creating it.
pub fn config_file() -> Result<PathBuf> {
    config_file_with(|name| std::env::var_os(name))
}

fn config_file_with(mut get_var: impl FnMut(&str) -> Option<OsString>) -> Result<PathBuf> {
    if let Some(directory) = non_empty(get_var("SENATE_CONFIG_DIR")) {
        return Ok(PathBuf::from(directory).join(CONFIG_FILE));
    }

    if let Some(directory) = non_empty(get_var("XDG_CONFIG_HOME")) {
        return Ok(PathBuf::from(directory)
            .join(APP_DIRECTORY)
            .join(CONFIG_FILE));
    }

    let home = non_empty(get_var("HOME"))
        .context("cannot resolve config path: set SENATE_CONFIG_DIR, XDG_CONFIG_HOME, or HOME")?;

    Ok(PathBuf::from(home)
        .join(".config")
        .join(APP_DIRECTORY)
        .join(CONFIG_FILE))
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::config_file_with;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn explicit_senate_directory_has_highest_priority() {
        let path = config_file_with(|name| match name {
            "SENATE_CONFIG_DIR" => Some(OsString::from("/custom/senate")),
            "XDG_CONFIG_HOME" => Some(OsString::from("/xdg")),
            "HOME" => Some(OsString::from("/home/user")),
            _ => None,
        })
        .expect("config path should resolve");

        assert_eq!(path, PathBuf::from("/custom/senate/config.toml"));
    }

    #[test]
    fn xdg_directory_precedes_home_fallback() {
        let path = config_file_with(|name| match name {
            "XDG_CONFIG_HOME" => Some(OsString::from("/xdg")),
            "HOME" => Some(OsString::from("/home/user")),
            _ => None,
        })
        .expect("config path should resolve");

        assert_eq!(path, PathBuf::from("/xdg/senate/config.toml"));
    }

    #[test]
    fn home_fallback_matches_documented_location() {
        let path = config_file_with(|name| (name == "HOME").then(|| OsString::from("/home/user")))
            .expect("config path should resolve");

        assert_eq!(path, PathBuf::from("/home/user/.config/senate/config.toml"));
    }

    #[test]
    fn missing_environment_returns_error() {
        let error = config_file_with(|_| None).expect_err("missing home should fail");

        assert!(error.to_string().contains("cannot resolve config path"));
    }
}
