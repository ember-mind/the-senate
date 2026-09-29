use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

use crate::domain::ModelId;

use super::OpencodeProviderError;
use super::command::{self, OpencodeSandbox};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeInstallation {
    executable: PathBuf,
    version: String,
    // Native model availability, not a count of entries in auth.json. Native
    // environment authentication, config-defined providers and free/local
    // models need not have any stored credentials.
    authenticated: bool,
    #[cfg(test)]
    fixture: bool,
}

impl OpencodeInstallation {
    #[cfg(test)]
    pub(super) fn fixture(executable: PathBuf) -> Self {
        Self {
            executable,
            version: "opencode fixture".to_owned(),
            authenticated: true,
            fixture: true,
        }
    }

    /// Discovers native opencode through hardened, non-interactive probes.
    ///
    /// # Errors
    /// Returns typed missing, version, configuration or discovery failures.
    pub fn discover() -> Result<Self, OpencodeProviderError> {
        let executable = find_on_path("opencode").ok_or(OpencodeProviderError::NotFound)?;
        Self::probe(executable)
    }

    fn probe(executable: PathBuf) -> Result<Self, OpencodeProviderError> {
        let output = command_output(&executable, &["--version"])
            .map_err(|error| OpencodeProviderError::VersionProbeFailed(error.to_string()))?;
        if !output.status.success() {
            return Err(OpencodeProviderError::VersionProbeFailed(format!(
                "command exited with {}",
                output.status
            )));
        }
        let version = safe_version(&output);
        if version.is_empty() {
            return Err(OpencodeProviderError::VersionProbeFailed(
                "empty version output".to_owned(),
            ));
        }
        let mut installation = Self {
            executable,
            version,
            authenticated: false,
            #[cfg(test)]
            fixture: false,
        };
        installation.validate_permissions()?;

        let auth = command_output(&installation.executable, &["auth", "list"])
            .map_err(|error| OpencodeProviderError::AuthStatusFailed(error.to_string()))?;
        if !auth.status.success() {
            return Err(OpencodeProviderError::AuthStatusFailed(format!(
                "command exited with {}",
                auth.status
            )));
        }
        // Do not infer readiness from human-readable credential counts:
        // "10 credentials" contains "0 credentials", and even a genuine zero
        // does not rule out environment authentication or a local provider.
        installation.authenticated = model_lines(&installation.models()?).next().is_some();
        Ok(installation)
    }

    pub(crate) fn require_authenticated(&self) -> Result<(), OpencodeProviderError> {
        if self.authenticated {
            Ok(())
        } else {
            Err(OpencodeProviderError::NotAuthenticated)
        }
    }

    /// Rejects native agent/mode overrides before every managed invocation.
    ///
    /// In opencode 1.18.33 agent permissions are appended *after* the global
    /// permission object, including OPENCODE_PERMISSION. Inspect the resolved
    /// configuration (which includes global Markdown agents and legacy modes)
    /// rather than claiming that the global environment override wins. Check
    /// every agent, not just the primary agent: task subagents must not bypass
    /// the stage policy either. No native configuration is modified or saved.
    ///
    /// # Errors
    /// Refuses an unavailable/unreadable audit or any nonempty agent-specific
    /// permission/tools override. Errors never include native config contents.
    pub(crate) fn validate_permissions(&self) -> Result<(), OpencodeProviderError> {
        #[cfg(test)]
        if self.fixture {
            // Engine tests use a ProcessBackend double, not a native CLI.
            // Discovery/integration tests construct real probed installations.
            return Ok(());
        }
        let output = command_output(&self.executable, &["debug", "config"])
            .map_err(|_| permission_error("could not inspect native configuration"))?;
        if !output.status.success() {
            return Err(permission_error("native configuration inspection failed"));
        }
        let config: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| permission_error("native configuration inspection returned invalid JSON"))?;
        validate_permission_config(&config)
    }

    /// Fails before launch if the model is absent from the hardened catalogue.
    ///
    /// # Errors
    /// Returns a typed listing failure or an unknown-model error.
    pub(crate) fn validate_model(&self, model: &ModelId) -> Result<(), OpencodeProviderError> {
        if model_lines(&self.models()?).any(|line| line == model.as_str()) {
            Ok(())
        } else {
            Err(OpencodeProviderError::UnknownModel(model.clone()))
        }
    }

    fn models(&self) -> Result<String, OpencodeProviderError> {
        let output = command_output(&self.executable, &["models"])
            .map_err(|error| OpencodeProviderError::ModelProbeFailed(error.to_string()))?;
        if !output.status.success() {
            return Err(OpencodeProviderError::ModelProbeFailed(format!(
                "command exited with {}",
                output.status
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Whether native opencode reports an available model. This is a readiness
    /// observation, not verification of a stored credential or a vendor call.
    #[must_use]
    pub const fn authenticated(&self) -> bool {
        self.authenticated
    }
}

fn permission_error(reason: &str) -> OpencodeProviderError {
    OpencodeProviderError::PermissionsConfig(format!("opencode permission preflight: {reason}"))
}

fn validate_permission_config(config: &Value) -> Result<(), OpencodeProviderError> {
    if !config.is_object() {
        return Err(permission_error("resolved configuration is not an object"));
    }
    for table in ["agent", "mode"] {
        let Some(value) = config.get(table) else {
            continue;
        };
        let agents = value
            .as_object()
            .ok_or_else(|| permission_error("resolved agent/mode configuration is not an object"))?;
        for agent in agents.values() {
            let settings = agent
                .as_object()
                .ok_or_else(|| permission_error("resolved agent configuration is not an object"))?;
            for field in ["permission", "tools"] {
                if let Some(overrides) = settings.get(field)
                    && !overrides.as_object().is_some_and(serde_json::Map::is_empty)
                {
                    return Err(permission_error(
                        "agent/mode permission or legacy tools overrides can supersede the stage policy; remove those per-agent overrides from native configuration before running Senate",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn model_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| {
        line.split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
    })
}

fn command_output(executable: &Path, args: &[&str]) -> std::io::Result<Output> {
    // `models` initializes providers and their plugins. It needs the same
    // defenses as `run`, not merely a claim that discovery is read-only.
    // Use a private neutral directory as well: no probe runs in the source
    // checkout, and stdin cannot accidentally consume an operator's input.
    let directory = tempfile::TempDir::new()?;
    let config_path = directory.path().join("opencode.json");
    let config = OpencodeSandbox::ReadOnly.permission_config(&BTreeMap::new());
    std::fs::write(&config_path, config.to_string())?;
    let environment = command::environment(&config_path, &config["permission"]);
    crate::exec::retry_busy(|| {
        Command::new(executable)
            .args(args)
            .arg("--pure")
            .envs(&environment)
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .output()
    })
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

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        // Probes change directory, so PATH entries such as ./bin must be
        // resolved before starting the native executable.
        .and_then(|candidate| candidate.canonicalize().ok())
}

/// Names of native configuration overrides ignored by the stage policy.
/// Per-vendor authentication variables remain native opencode's concern;
/// their values are never inspected, persisted or reported here.
#[must_use]
pub fn suspicious_opencode_environment() -> Vec<String> {
    const NAMES: &[&str] = &[
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "OPENCODE_CONFIG_CONTENT",
        "OPENCODE_PERMISSION",
        "OPENCODE_API_KEY",
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

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const MODELS: &str = "opencode-go/deepseek-v4-pro\nopencode-go/kimi-k3";

    #[test]
    fn missing_path_has_no_false_positive() {
        assert_eq!(find_on_path("senate-definitely-missing-opencode"), None);
    }

    #[test]
    fn credential_counts_never_override_native_model_availability() {
        for count in [0, 1, 2, 10, 20] {
            let temp = TempDir::new().unwrap();
            let executable = fixture(&temp, &format!("{count} credentials"), 0, MODELS);
            let installation = OpencodeInstallation::probe(executable).unwrap();
            assert!(installation.authenticated(), "{count} credentials");
            installation.require_authenticated().unwrap();
        }
    }

    #[test]
    fn an_empty_native_catalogue_is_not_ready() {
        let temp = TempDir::new().unwrap();
        let installation =
            OpencodeInstallation::probe(fixture(&temp, "0 credentials", 0, "")).unwrap();
        assert!(!installation.authenticated());
        assert!(matches!(
            installation.require_authenticated(),
            Err(OpencodeProviderError::NotAuthenticated)
        ));
    }

    #[test]
    fn unexpected_auth_failure_is_typed() {
        let temp = TempDir::new().unwrap();
        assert!(matches!(
            OpencodeInstallation::probe(fixture(&temp, "credentials unavailable", 42, MODELS)),
            Err(OpencodeProviderError::AuthStatusFailed(_))
        ));
    }

    #[test]
    fn empty_version_is_typed() {
        let temp = TempDir::new().unwrap();
        let executable = script_file(&temp, "opencode", "exit 0");
        assert!(matches!(
            OpencodeInstallation::probe(executable),
            Err(OpencodeProviderError::VersionProbeFailed(_))
        ));
    }

    #[test]
    fn model_validation_accepts_a_listed_model_and_rejects_an_unlisted_one() {
        let temp = TempDir::new().unwrap();
        let installation =
            OpencodeInstallation::probe(fixture(&temp, "0 credentials", 0, MODELS)).unwrap();
        installation
            .validate_model(&ModelId::new("opencode-go/deepseek-v4-pro").unwrap())
            .unwrap();
        assert!(matches!(
            installation.validate_model(&ModelId::new("nobody/nothing").unwrap()),
            Err(OpencodeProviderError::UnknownModel(_))
        ));
    }

    #[test]
    fn primary_subagent_and_legacy_mode_overrides_are_all_refused() {
        for table in ["agent", "mode"] {
            for name in ["build", "general", "explore", "custom"] {
                for overrides in [json!("allow"), json!({"edit":"allow"}), json!({"bash":"allow"})] {
                    let mut config = json!({});
                    config[table] = json!({});
                    config[table][name] = json!({"permission": overrides});
                    assert!(validate_permission_config(&config).is_err());
                }
            }
        }
        assert!(
            validate_permission_config(&json!({"agent":{"general":{"tools":{"write":true}}}}))
                .is_err()
        );
    }

    #[test]
    fn agents_without_permission_overrides_keep_native_prompts_and_models() {
        validate_permission_config(&json!({
            "agent": {"custom": {"prompt":"native prompt", "model":"provider/model", "permission": {}}},
            "mode": {}
        }))
        .unwrap();
    }

    #[test]
    fn malformed_configuration_fails_closed_without_echoing_contents() {
        for config in [json!(null), json!({"agent": []}), json!({"agent":{"SECRET": "SECRET"}})] {
            let error = validate_permission_config(&config).unwrap_err().to_string();
            assert!(!error.contains("SECRET"));
        }
    }

    #[test]
    fn every_probe_is_pure_isolated_and_non_interactive() {
        let temp = TempDir::new().unwrap();
        // The fixture refuses to execute unless every defense is present.
        // Check model validation separately from initial discovery too.
        let installation =
            OpencodeInstallation::probe(fixture(&temp, "10 credentials", 0, MODELS)).unwrap();
        installation.validate_permissions().unwrap();
        installation
            .validate_model(&ModelId::new("opencode-go/kimi-k3").unwrap())
            .unwrap();
    }

    fn fixture(temp: &TempDir, auth: &str, auth_exit: i32, models: &str) -> PathBuf {
        let body = format!(
            "[ \"$OPENCODE_DISABLE_PROJECT_CONFIG\" = 1 ] || exit 91\n\
             [ -f \"$OPENCODE_CONFIG\" ] || exit 92\n\
             [ -z \"$OPENCODE_CONFIG_DIR\" ] || exit 93\n\
             [ -z \"$OPENCODE_CONFIG_CONTENT\" ] || exit 94\n\
             case \"$OPENCODE_PERMISSION\" in *'\"bash\":\"deny\"'*) ;; *) exit 95;; esac\n\
             [ \"$PWD\" != '{}' ] || exit 96\n\
             if IFS= read -r unexpected; then exit 97; fi\n\
             case \"$*\" in\n\
               \"--version --pure\") echo '1.18.33';;\n\
               \"debug config --pure\") echo '{{}}';;\n\
               \"auth list --pure\") echo '{auth}'; exit {auth_exit};;\n\
               \"models --pure\") printf '%s\\n' '{models}';;\n\
               *) exit 64;;\n\
             esac",
            std::env::current_dir().unwrap().display()
        );
        script_file(temp, "opencode", &body)
    }

    fn script_file(temp: &TempDir, name: &str, body: &str) -> PathBuf {
        let path = temp.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}
