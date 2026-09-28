use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::ClaudeProviderError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeInstallation {
    executable: PathBuf,
    version: String,
    authenticated: bool,
    auth_method: Option<String>,
}

impl ClaudeInstallation {
    #[cfg(test)]
    pub(super) fn fixture(executable: PathBuf) -> Self {
        Self {
            executable,
            version: "fixture".to_owned(),
            authenticated: true,
            auth_method: Some("fixture".to_owned()),
        }
    }

    /// Discovers native Claude Code and reads only safe version/auth metadata.
    ///
    /// # Errors
    /// Returns typed missing, command, or JSON failures.
    pub fn discover() -> Result<Self, ClaudeProviderError> {
        let executable = find_on_path("claude").ok_or(ClaudeProviderError::NotFound)?;
        let version = command_text(&executable, &["--version"], "version check")?;
        let auth = Command::new(&executable)
            .args(["auth", "status", "--json"])
            .output()
            .map_err(ClaudeProviderError::Io)?;
        let value: Value = serde_json::from_slice(&auth.stdout)?;
        let authenticated = value
            .get("loggedIn")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let auth_method = value
            .get("authMethod")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(Self {
            executable,
            version,
            authenticated,
            auth_method,
        })
    }

    pub(crate) fn require_authenticated(&self) -> Result<(), ClaudeProviderError> {
        if self.authenticated {
            Ok(())
        } else {
            Err(ClaudeProviderError::NotAuthenticated)
        }
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }
    #[must_use]
    pub const fn authenticated(&self) -> bool {
        self.authenticated
    }
    #[must_use]
    pub fn auth_method(&self) -> Option<&str> {
        self.auth_method.as_deref()
    }
}

fn command_text(
    executable: &Path,
    args: &[&str],
    operation: &'static str,
) -> Result<String, ClaudeProviderError> {
    let output = Command::new(executable).args(args).output()?;
    if !output.status.success() {
        return Err(ClaudeProviderError::Command {
            operation,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[must_use]
pub fn suspicious_secret_environment() -> Vec<String> {
    const NAMES: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ];
    NAMES
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .map(|name| (*name).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_path_has_no_false_positive() {
        assert_eq!(find_on_path("senate-definitely-missing-executable"), None);
    }
}
