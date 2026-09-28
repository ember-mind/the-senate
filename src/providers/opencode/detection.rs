use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::domain::ModelId;

use super::OpencodeProviderError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeInstallation {
    executable: PathBuf,
    version: String,
    authenticated: bool,
}

impl OpencodeInstallation {
    #[cfg(test)]
    pub(super) fn fixture(executable: PathBuf) -> Self {
        Self {
            executable,
            version: "opencode fixture".to_owned(),
            authenticated: true,
        }
    }

    /// Discovers native opencode using only read-only CLI probes.
    ///
    /// # Errors
    /// Returns typed missing, version, or authentication-probe failures.
    pub fn discover() -> Result<Self, OpencodeProviderError> {
        let executable = find_on_path("opencode").ok_or(OpencodeProviderError::NotFound)?;
        Self::probe(executable)
    }

    fn probe(executable: PathBuf) -> Result<Self, OpencodeProviderError> {
        let version = command_output(&executable, &["--version"])
            .map_err(|error| OpencodeProviderError::VersionProbeFailed(error.to_string()))?;
        let version = safe_version(&version);
        if version.is_empty() {
            return Err(OpencodeProviderError::VersionProbeFailed(
                "empty version output".to_owned(),
            ));
        }

        let auth = command_output(&executable, &["auth", "list"])
            .map_err(|error| OpencodeProviderError::AuthStatusFailed(error.to_string()))?;
        if !auth.status.success() {
            return Err(OpencodeProviderError::AuthStatusFailed(format!(
                "command exited with {}",
                auth.status
            )));
        }
        let authenticated = !has_no_credentials(&output_text(&auth));

        Ok(Self {
            executable,
            version,
            authenticated,
        })
    }

    pub(crate) fn require_authenticated(&self) -> Result<(), OpencodeProviderError> {
        if self.authenticated {
            Ok(())
        } else {
            Err(OpencodeProviderError::NotAuthenticated)
        }
    }

    /// Fails fast with a clear typed error when `model` is not one
    /// `opencode models` lists, instead of letting an unresolvable model
    /// reach `opencode run` and fail there with the vendor's vague
    /// `UnknownError` ("Unexpected server error").
    ///
    /// # Errors
    /// Returns [`OpencodeProviderError::ModelProbeFailed`] when the listing
    /// itself could not be read, or [`OpencodeProviderError::UnknownModel`]
    /// when it does not name `model`.
    pub(crate) fn validate_model(&self, model: &ModelId) -> Result<(), OpencodeProviderError> {
        let output = command_output(&self.executable, &["models"])
            .map_err(|error| OpencodeProviderError::ModelProbeFailed(error.to_string()))?;
        if !output.status.success() {
            return Err(OpencodeProviderError::ModelProbeFailed(format!(
                "command exited with {}",
                output.status
            )));
        }
        let listed = String::from_utf8_lossy(&output.stdout);
        if listed
            .lines()
            .map(str::trim)
            .any(|line| line == model.as_str())
        {
            Ok(())
        } else {
            Err(OpencodeProviderError::UnknownModel(model.clone()))
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
}

fn command_output(executable: &Path, args: &[&str]) -> std::io::Result<Output> {
    // Mirrors the Codex/Claude probes: a freshly written stub executable in a
    // test can still be mid-write when exec'd from another thread (`ETXTBSY`);
    // retry_busy clears that window without hiding a real failure.
    crate::exec::retry_busy(|| Command::new(executable).args(args).output())
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

/// Whether `opencode auth list` reports zero configured credentials. Real
/// output is a small ANSI-decorated box reporting a count, e.g. `2
/// credentials`; nothing here inspects which providers or reads any secret.
fn has_no_credentials(text: &str) -> bool {
    text.to_ascii_lowercase().contains("0 credentials")
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Environment variable names worth flagging in `senate doctor`. opencode
/// authenticates through `~/.local/share/opencode/auth.json`, not through
/// process environment, so this is a narrower list than Claude's or Codex's:
/// only opencode's own override points, never a per-vendor API key, because
/// opencode fans out to dozens of vendors and enumerating their env var names
/// here would be a guess this module has no way to keep current.
#[must_use]
pub fn suspicious_opencode_environment() -> Vec<String> {
    const NAMES: &[&str] = &["OPENCODE_CONFIG", "OPENCODE_API_KEY"];
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
        assert_eq!(find_on_path("senate-definitely-missing-opencode"), None);
    }

    #[test]
    fn probes_authenticated_fixture() {
        let temp = TempDir::new().unwrap();
        let executable = fixture(&temp, "2 credentials", 0);
        let installation = OpencodeInstallation::probe(executable).unwrap();
        assert!(installation.authenticated());
    }

    #[test]
    fn zero_credentials_is_detected_as_unauthenticated() {
        let temp = TempDir::new().unwrap();
        let executable = fixture(&temp, "0 credentials", 0);
        let installation = OpencodeInstallation::probe(executable).unwrap();
        assert!(!installation.authenticated());
        assert!(matches!(
            installation.require_authenticated(),
            Err(OpencodeProviderError::NotAuthenticated)
        ));
    }

    #[test]
    fn unexpected_auth_failure_is_typed() {
        let temp = TempDir::new().unwrap();
        let executable = fixture(&temp, "credentials unavailable", 42);
        assert!(matches!(
            OpencodeInstallation::probe(executable),
            Err(OpencodeProviderError::AuthStatusFailed(_))
        ));
    }

    #[test]
    fn empty_version_is_typed() {
        let temp = TempDir::new().unwrap();
        let empty_version = script(
            &temp,
            "empty-version",
            "case \"$*\" in\n  \"--version\") exit 0;;\n  *) exit 64;;\nesac",
        );
        assert!(matches!(
            OpencodeInstallation::probe(empty_version),
            Err(OpencodeProviderError::VersionProbeFailed(_))
        ));
    }

    #[test]
    fn model_validation_accepts_a_listed_model_and_rejects_an_unlisted_one() {
        let temp = TempDir::new().unwrap();
        let executable = fixture(&temp, "2 credentials", 0);
        let installation = OpencodeInstallation::probe(executable).unwrap();

        installation
            .validate_model(&ModelId::new("opencode-go/deepseek-v4-pro").unwrap())
            .expect("listed model is accepted");

        assert!(matches!(
            installation.validate_model(&ModelId::new("nobody/nothing").unwrap()),
            Err(OpencodeProviderError::UnknownModel(_))
        ));
    }

    fn fixture(temp: &TempDir, auth: &str, auth_exit: i32) -> PathBuf {
        let script = format!(
            "case \"$*\" in\n  \"--version\") echo '1.18.32';;\n  \"auth list\") echo '{auth}'; exit {auth_exit};;\n  \"models\") printf 'opencode-go/deepseek-v4-pro\\nopencode-go/kimi-k3\\n';;\n  *) exit 64;;\nesac"
        );
        script_file(temp, "opencode", &script)
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
