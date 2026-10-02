use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::CodexProviderError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexInstallation {
    executable: PathBuf,
    version: String,
    authenticated: bool,
    auth_method: Option<String>,
}

impl CodexInstallation {
    #[cfg(test)]
    pub(super) fn fixture(executable: PathBuf) -> Self {
        Self {
            executable,
            version: "codex-cli fixture".to_owned(),
            authenticated: true,
            auth_method: Some("fixture".to_owned()),
        }
    }

    /// Discovers native Codex using only read-only CLI probes.
    ///
    /// # Errors
    /// Returns typed missing, version, authentication, or capability failures.
    pub fn discover() -> Result<Self, CodexProviderError> {
        let executable = find_on_path("codex").ok_or(CodexProviderError::NotFound)?;
        Self::probe(executable)
    }

    fn probe(executable: PathBuf) -> Result<Self, CodexProviderError> {
        let version = command_output(&executable, &["--version"])
            .map_err(|error| CodexProviderError::VersionProbeFailed(error.to_string()))?;
        let version = safe_version(&version);
        if version.is_empty() {
            return Err(CodexProviderError::VersionProbeFailed(
                "empty version output".to_owned(),
            ));
        }

        let auth = command_output(&executable, &["login", "status"])
            .map_err(|error| CodexProviderError::AuthStatusFailed(error.to_string()))?;
        let auth_text = output_text(&auth);
        let authenticated = auth.status.success();
        if !authenticated && !is_logged_out_message(&auth_text) {
            return Err(CodexProviderError::AuthStatusFailed(format!(
                "command exited with {}",
                auth.status
            )));
        }

        require_capability(
            &executable,
            &["exec", "--help"],
            &["--json", "--output-last-message"],
        )?;
        require_capability(&executable, &["exec", "resume", "--help"], &["SESSION_ID"])?;

        Ok(Self {
            executable,
            version,
            authenticated,
            auth_method: authenticated.then(|| safe_auth_method(&auth_text)),
        })
    }

    pub(crate) fn require_authenticated(&self) -> Result<(), CodexProviderError> {
        if self.authenticated {
            Ok(())
        } else {
            Err(CodexProviderError::NotAuthenticated)
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

fn command_output(executable: &Path, args: &[&str]) -> std::io::Result<Output> {
    // Tests exec a freshly written stub `codex` immediately, which can race
    // another test thread's fork still holding its write fd open
    // (`ETXTBSY`); retry_busy clears that window without hiding a real
    // failure.
    crate::exec::retry_busy(|| {
        crate::exec::without_jira_credentials(Command::new(executable))
            .args(args)
            .output()
    })
}

fn require_capability(
    executable: &Path,
    args: &[&str],
    required_markers: &[&str],
) -> Result<(), CodexProviderError> {
    let output = command_output(executable, args)
        .map_err(|error| CodexProviderError::UnsupportedCli(error.to_string()))?;
    let text = output_text(&output);
    if !output.status.success() {
        return Err(CodexProviderError::UnsupportedCli(format!(
            "`{}` exited with {}",
            args.join(" "),
            output.status
        )));
    }
    if let Some(missing) = required_markers
        .iter()
        .find(|marker| !text.contains(**marker))
    {
        return Err(CodexProviderError::UnsupportedCli(format!(
            "`{}` help lacks {missing}",
            args.join(" ")
        )));
    }
    Ok(())
}

fn output_text(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{stdout}\n{stderr}").trim().to_owned()
}

fn safe_version(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(256)
        .collect()
}

fn is_logged_out_message(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("not logged in") || normalized.contains("not authenticated")
}

fn safe_auth_method(message: &str) -> String {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("chatgpt") {
        "ChatGPT".to_owned()
    } else if normalized.contains("api key") || normalized.contains("api-key") {
        "API key".to_owned()
    } else {
        "native CLI".to_owned()
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[must_use]
pub fn suspicious_codex_environment() -> Vec<String> {
    const NAMES: &[&str] = &[
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CODEX_HOME",
        "OPENAI_BASE_URL",
    ];
    NAMES
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .map(|name| (*name).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn missing_path_has_no_false_positive() {
        assert_eq!(find_on_path("senate-definitely-missing-codex"), None);
    }

    #[test]
    fn probes_authenticated_fixture_without_preserving_raw_auth_output() {
        let temp = TempDir::new().unwrap();
        let executable = fixture(
            &temp,
            "Logged in using ChatGPT as secret-user@example.invalid",
            0,
        );
        let installation = CodexInstallation::probe(executable).unwrap();
        assert!(installation.authenticated());
        assert_eq!(installation.auth_method(), Some("ChatGPT"));
        assert!(!format!("{installation:?}").contains("secret-user"));
    }

    #[test]
    fn logged_out_fixture_is_detected_without_mutating_login() {
        let temp = TempDir::new().unwrap();
        let executable = fixture(&temp, "Not logged in", 1);
        let installation = CodexInstallation::probe(executable).unwrap();
        assert!(!installation.authenticated());
        assert!(matches!(
            installation.require_authenticated(),
            Err(CodexProviderError::NotAuthenticated)
        ));
    }

    #[test]
    fn unexpected_auth_failure_is_typed() {
        let temp = TempDir::new().unwrap();
        let executable = fixture(&temp, "provider unavailable TOKEN_SHOULD_NOT_SURVIVE", 42);
        assert!(matches!(
            CodexInstallation::probe(executable),
            Err(CodexProviderError::AuthStatusFailed(_))
        ));
    }

    #[test]
    fn empty_version_and_missing_exec_capability_are_typed() {
        let temp = TempDir::new().unwrap();
        let empty_version = script(
            &temp,
            "empty-version",
            "case \"$*\" in\n  \"--version\") exit 0;;\n  *) exit 64;;\nesac",
        );
        assert!(matches!(
            CodexInstallation::probe(empty_version),
            Err(CodexProviderError::VersionProbeFailed(_))
        ));

        let unsupported = script(
            &temp,
            "unsupported",
            "case \"$*\" in\n  \"--version\") echo 'codex-cli 1.2.3';;\n  \"login status\") echo 'Logged in using ChatGPT';;\n  \"exec --help\") echo '--json';;\n  \"exec resume --help\") echo 'SESSION_ID';;\n  *) exit 64;;\nesac",
        );
        assert!(matches!(
            CodexInstallation::probe(unsupported),
            Err(CodexProviderError::UnsupportedCli(_))
        ));
    }

    fn fixture(temp: &TempDir, auth: &str, auth_exit: i32) -> PathBuf {
        let script = format!(
            "case \"$*\" in\n  \"--version\") echo 'codex-cli 1.2.3';;\n  \"login status\") echo '{auth}'; exit {auth_exit};;\n  \"exec --help\") echo '--json --output-last-message';;\n  \"exec resume --help\") echo 'SESSION_ID';;\n  *) exit 64;;\nesac"
        );
        script_file(temp, "codex", &script)
    }

    fn script(temp: &TempDir, name: &str, body: &str) -> PathBuf {
        script_file(temp, name, body)
    }

    fn script_file(temp: &TempDir, name: &str, body: &str) -> PathBuf {
        let path = temp.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }
}
