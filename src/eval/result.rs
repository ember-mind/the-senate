use std::fmt;
use std::io::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{ModelId, ProviderId, Role};

pub const EVAL_RESULT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalProvider {
    Claude,
    Codex,
    Fake,
}

impl EvalProvider {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Fake => "fake",
        }
    }

    #[must_use]
    pub const fn is_native(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }
}

impl fmt::Display for EvalProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for EvalProvider {
    type Error = EvalResultError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "fake" => Ok(Self::Fake),
            other => Err(EvalResultError::InvalidTarget(format!(
                "unsupported provider {other:?}; expected claude, codex, or fake"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalTarget {
    pub provider: EvalProvider,
    pub configured_model: Option<String>,
}

impl EvalTarget {
    /// Creates and validates one explicit provider/model candidate.
    ///
    /// # Errors
    /// Rejects invalid provider or model identifiers.
    pub fn new(
        provider: EvalProvider,
        configured_model: Option<String>,
    ) -> Result<Self, EvalResultError> {
        ProviderId::new(provider.as_str())
            .map_err(|error| EvalResultError::InvalidTarget(error.to_string()))?;
        if let Some(model) = &configured_model {
            ModelId::new(model.clone())
                .map_err(|error| EvalResultError::InvalidTarget(error.to_string()))?;
        }
        Ok(Self {
            provider,
            configured_model,
        })
    }

    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{} / {}",
            self.provider,
            self.configured_model.as_deref().unwrap_or("native_default")
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    Passed,
    Failed,
    InfrastructureFailure,
}

/// Provider-native usage recorded for the evaluated target stage.
///
/// Units are runtime-specific and never normalized across providers; results
/// from different targets must not be compared unit-for-unit. Optional
/// dimensions are additive schema-V1 extensions: absent in results written
/// before resource observability existed, decoded as unavailable (`None`)
/// rather than zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalUsage {
    pub input_units: u64,
    pub output_units: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_units: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_units: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_units: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementerMetrics {
    pub behavioral_pass: bool,
    pub scope_pass: bool,
    pub plan_mismatch_behavior: Option<bool>,
    pub validation_pass: Option<bool>,
    pub unexpected_files: Vec<String>,
    pub deletions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityMetrics {
    pub defects_found: u32,
    pub defects_total: u32,
    pub recall: f64,
    pub false_positives: u32,
    pub must_fix_false_positives: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityMetricsV2 {
    pub defects_found: u32,
    pub defects_total: u32,
    pub recall: f64,
    pub false_positives: u32,
    pub must_fix_false_positives: u32,
    pub severity_matches: u32,
    pub severity_total: u32,
    pub underclassified: u32,
    pub overclassified: u32,
    pub duplicate_findings: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecMetrics {
    pub missing_found: u32,
    pub missing_total: u32,
    pub wrong_found: u32,
    pub wrong_total: u32,
    pub unrequested_found: u32,
    pub unrequested_total: u32,
    pub false_positives: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecMetricsV2 {
    pub missing_found: u32,
    pub missing_total: u32,
    pub wrong_found: u32,
    pub wrong_total: u32,
    pub unrequested_found: u32,
    pub unrequested_total: u32,
    pub false_positives: u32,
    pub duplicate_findings: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "values", rename_all = "snake_case")]
pub enum EvalMetrics {
    Implementer(ImplementerMetrics),
    CodeQualityReviewer(QualityMetrics),
    SpecReviewer(SpecMetrics),
    CodeQualityReviewerV2(QualityMetricsV2),
    SpecReviewerV2(SpecMetricsV2),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalResultV1 {
    pub schema_version: u32,
    pub suite: String,
    pub suite_version: String,
    pub suite_fingerprint: String,
    pub case_id: String,
    pub repetition: u32,
    pub target: EvalTarget,
    pub confirmed_model: Option<String>,
    pub provider_cli_version: Option<String>,
    pub role: Role,
    pub status: EvalStatus,
    pub metrics: Option<EvalMetrics>,
    pub usage: EvalUsage,
    /// Exact bytes The Senate piped into the target stage's native invocations
    /// (initial prompt plus continuations). Cross-provider comparable, unlike
    /// usage units. Absent in pre-observability results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected_prompt_bytes: Option<u64>,
    /// Requested native-runtime effort for the candidate stage. Absent in
    /// pre-effort-policy results, which ran under the runtime's native
    /// configured default (`NativeDefault` semantics, never `Medium`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<crate::domain::EffortSetting>,
    pub latency_ms: u64,
    pub artifact_hash: Option<String>,
    pub diff_hash: String,
    pub fixture_hash: String,
    pub synthetic: bool,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub detail: Option<String>,
}

impl EvalResultV1 {
    /// Decodes and validates schema V1 without accepting future versions.
    ///
    /// # Errors
    /// Rejects malformed JSON, unsupported schemas, or invariant violations.
    pub fn from_json(bytes: &[u8]) -> Result<Self, EvalResultError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or(EvalResultError::MissingSchemaVersion)?;
        if version != u64::from(EVAL_RESULT_SCHEMA_VERSION) {
            return Err(EvalResultError::UnsupportedSchema(version));
        }
        let result: Self = serde_json::from_value(value)?;
        result.validate()?;
        Ok(result)
    }

    /// Writes one immutable result file atomically.
    ///
    /// # Errors
    /// Rejects invalid results, existing destinations, or filesystem failures.
    pub fn write(&self, path: &Path) -> Result<(), EvalResultError> {
        self.validate()?;
        let parent = path.parent().ok_or(EvalResultError::InvalidPath)?;
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temporary, self)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(path)
            .map_err(|error| EvalResultError::Io(error.error))?;
        Ok(())
    }

    fn validate(&self) -> Result<(), EvalResultError> {
        if self.schema_version != EVAL_RESULT_SCHEMA_VERSION {
            return Err(EvalResultError::UnsupportedSchema(u64::from(
                self.schema_version,
            )));
        }
        EvalTarget::new(self.target.provider, self.target.configured_model.clone())?;
        if let Some(model) = &self.confirmed_model {
            ModelId::new(model.clone())
                .map_err(|error| EvalResultError::InvalidTarget(error.to_string()))?;
        }
        if self.suite.trim().is_empty()
            || self.suite_version.trim().is_empty()
            || self.case_id.trim().is_empty()
            || self.repetition == 0
            || self.finished_at < self.started_at
            || !valid_hash(&self.suite_fingerprint)
            || !valid_hash(&self.diff_hash)
            || !valid_hash(&self.fixture_hash)
            || self
                .artifact_hash
                .as_ref()
                .is_some_and(|hash| !valid_hash(hash))
            || self.synthetic != (self.target.provider == EvalProvider::Fake)
        {
            return Err(EvalResultError::InvalidResult);
        }
        if self.status == EvalStatus::Passed && self.metrics.is_none() {
            return Err(EvalResultError::InvalidResult);
        }
        if self.status == EvalStatus::InfrastructureFailure && self.metrics.is_some() {
            return Err(EvalResultError::InvalidResult);
        }
        match &self.metrics {
            Some(EvalMetrics::Implementer(_)) if self.role == Role::Implementer => {}
            Some(EvalMetrics::CodeQualityReviewer(metrics))
                if self.role == Role::CodeQualityReviewer
                    && metrics.defects_found <= metrics.defects_total
                    && metrics.must_fix_false_positives <= metrics.false_positives
                    && metrics.recall.is_finite()
                    && (0.0..=1.0).contains(&metrics.recall) => {}
            Some(EvalMetrics::SpecReviewer(metrics))
                if self.role == Role::SpecReviewer
                    && metrics.missing_found <= metrics.missing_total
                    && metrics.wrong_found <= metrics.wrong_total
                    && metrics.unrequested_found <= metrics.unrequested_total => {}
            Some(EvalMetrics::CodeQualityReviewerV2(metrics))
                if self.role == Role::CodeQualityReviewer
                    && metrics.defects_found <= metrics.defects_total
                    && metrics.must_fix_false_positives <= metrics.false_positives
                    && metrics.severity_matches <= metrics.severity_total
                    && metrics.severity_total == metrics.defects_found
                    && metrics.underclassified <= metrics.severity_total
                    && metrics.overclassified <= metrics.severity_total
                    && metrics
                        .severity_matches
                        .saturating_add(metrics.underclassified)
                        .saturating_add(metrics.overclassified)
                        == metrics.severity_total
                    && metrics
                        .underclassified
                        .saturating_add(metrics.overclassified)
                        <= metrics.severity_total
                    && metrics.recall.is_finite()
                    && (0.0..=1.0).contains(&metrics.recall) => {}
            Some(EvalMetrics::SpecReviewerV2(metrics))
                if self.role == Role::SpecReviewer
                    && metrics.missing_found <= metrics.missing_total
                    && metrics.wrong_found <= metrics.wrong_total
                    && metrics.unrequested_found <= metrics.unrequested_total => {}
            None => {}
            Some(_) => return Err(EvalResultError::InvalidResult),
        }
        Ok(())
    }
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Error)]
pub enum EvalResultError {
    #[error("eval result has no schema_version")]
    MissingSchemaVersion,
    #[error("unsupported eval result schema {0}")]
    UnsupportedSchema(u64),
    #[error("invalid eval target: {0}")]
    InvalidTarget(String),
    #[error("eval result violates schema invariants")]
    InvalidResult,
    #[error("eval result path has no parent")]
    InvalidPath,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use tempfile::tempdir;

    use super::*;
    use crate::eval::case::ROLE_CORE_CASES_V2;
    use crate::eval::scorer::{ScoreInput, score};

    fn result() -> EvalResultV1 {
        let at = Utc
            .with_ymd_and_hms(2026, 8, 17, 12, 0, 0)
            .single()
            .unwrap();
        EvalResultV1 {
            requested_effort: None,
            schema_version: EVAL_RESULT_SCHEMA_VERSION,
            suite: "role_core".to_owned(),
            suite_version: "role_core_v1".to_owned(),
            suite_fingerprint: "a".repeat(64),
            case_id: "quality_clean".to_owned(),
            repetition: 1,
            target: EvalTarget::new(EvalProvider::Codex, None).unwrap(),
            confirmed_model: None,
            provider_cli_version: Some("fixture-1".to_owned()),
            role: Role::CodeQualityReviewer,
            status: EvalStatus::Passed,
            metrics: Some(EvalMetrics::CodeQualityReviewer(QualityMetrics {
                defects_found: 0,
                defects_total: 0,
                recall: 1.0,
                false_positives: 0,
                must_fix_false_positives: 0,
            })),
            usage: EvalUsage::default(),
            injected_prompt_bytes: None,
            latency_ms: 25,
            artifact_hash: Some("b".repeat(64)),
            diff_hash: "c".repeat(64),
            fixture_hash: "d".repeat(64),
            synthetic: false,
            started_at: at,
            finished_at: at,
            detail: None,
        }
    }

    #[test]
    fn result_round_trip_preserves_native_default_and_confirmed_model_distinction() {
        let original = result();
        let bytes = serde_json::to_vec(&original).unwrap();
        let restored = EvalResultV1::from_json(&bytes).unwrap();
        assert_eq!(restored, original);
        assert_eq!(restored.target.configured_model, None);
        assert_eq!(restored.confirmed_model, None);
    }

    #[test]
    fn pre_observability_result_json_decodes_with_unavailable_resource_fields() {
        // Byte-shape of a result.json written before M13a: no cache/reasoning
        // usage dimensions and no injected_prompt_bytes. Must stay readable,
        // with the additions decoding as unavailable rather than zero.
        let legacy = serde_json::json!({
            "schema_version": 1,
            "suite": "role_core",
            "suite_version": "role_core_v3",
            "suite_fingerprint": "a".repeat(64),
            "case_id": "implementer_basic_bugfix",
            "repetition": 1,
            "target": {"provider": "claude", "configured_model": null},
            "confirmed_model": null,
            "provider_cli_version": "1.0.0",
            "role": "implementer",
            "status": "passed",
            "metrics": {"kind": "implementer", "values": {
                "behavioral_pass": true, "scope_pass": true,
                "plan_mismatch_behavior": null, "validation_pass": true,
                "unexpected_files": [], "deletions": []
            }},
            "usage": {"input_units": 18, "output_units": 83},
            "latency_ms": 29113,
            "artifact_hash": null,
            "diff_hash": "c".repeat(64),
            "fixture_hash": "d".repeat(64),
            "synthetic": false,
            "started_at": "2026-08-14T08:00:00Z",
            "finished_at": "2026-08-14T08:01:00Z",
            "detail": null
        });
        let restored = EvalResultV1::from_json(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(restored.usage.input_units, 18);
        assert_eq!(restored.usage.cache_read_units, None);
        assert_eq!(restored.usage.cache_write_units, None);
        assert_eq!(restored.usage.reasoning_output_units, None);
        assert_eq!(restored.injected_prompt_bytes, None);
        // Re-encoding keeps unavailable dimensions absent.
        let encoded = serde_json::to_string(&restored).unwrap();
        assert!(!encoded.contains("cache_read_units"));
        assert!(!encoded.contains("injected_prompt_bytes"));
    }

    #[test]
    fn future_result_schema_is_rejected_before_decode() {
        let mut value = serde_json::to_value(result()).unwrap();
        value["schema_version"] = serde_json::json!(2);
        assert!(matches!(
            EvalResultV1::from_json(&serde_json::to_vec(&value).unwrap()),
            Err(EvalResultError::UnsupportedSchema(2))
        ));
    }

    #[test]
    fn impossible_or_role_mismatched_metrics_are_rejected() {
        let mut invalid_recall = result();
        if let Some(EvalMetrics::CodeQualityReviewer(metrics)) = &mut invalid_recall.metrics {
            metrics.recall = 1.5;
        }
        assert!(matches!(
            EvalResultV1::from_json(&serde_json::to_vec(&invalid_recall).unwrap()),
            Err(EvalResultError::InvalidResult)
        ));

        let mut wrong_role = result();
        wrong_role.role = Role::Implementer;
        assert!(matches!(
            EvalResultV1::from_json(&serde_json::to_vec(&wrong_role).unwrap()),
            Err(EvalResultError::InvalidResult)
        ));
    }

    #[test]
    fn v2_metrics_round_trip_without_changing_result_envelope_schema() {
        let mut original = result();
        original.suite_version = "role_core_v2".to_owned();
        original.metrics = Some(EvalMetrics::CodeQualityReviewerV2(QualityMetricsV2 {
            defects_found: 2,
            defects_total: 3,
            recall: 2.0 / 3.0,
            false_positives: 0,
            must_fix_false_positives: 0,
            severity_matches: 1,
            severity_total: 2,
            underclassified: 1,
            overclassified: 0,
            duplicate_findings: 1,
        }));
        let bytes = serde_json::to_vec(&original).unwrap();
        let restored = EvalResultV1::from_json(&bytes).unwrap();
        assert_eq!(restored, original);
        assert_eq!(restored.schema_version, EVAL_RESULT_SCHEMA_VERSION);
    }

    #[test]
    fn partial_v2_quality_score_writes_and_reloads_as_failed_benchmark() {
        let artifact = "```json\n{\"eval_version\":1,\"findings\":[\n\
            {\"severity\":\"must_fix\",\"file\":\"src/lib.rs\",\"line\":3,\"summary\":\"Unnecessary abstraction with one caller\"},\n\
            {\"severity\":\"minor\",\"file\":\"src/lib.rs\",\"line\":28,\"summary\":\"Nested control flow obscures classification\"}\n]}\n```";
        let scored = score(
            &ROLE_CORE_CASES_V2[3],
            ScoreInput {
                artifact: Some(artifact),
                diff: "",
                validation_pass: None,
            },
        )
        .unwrap();
        assert!(!scored.passed);
        let EvalMetrics::CodeQualityReviewerV2(metrics) = &scored.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert_eq!(metrics.defects_found, 2);
        assert_eq!(metrics.defects_total, 3);
        assert_eq!(metrics.severity_total, 2);
        assert_eq!(
            metrics.severity_matches + metrics.underclassified + metrics.overclassified,
            2
        );

        let mut result = result();
        result.suite_version = "role_core_v2".to_owned();
        result.case_id = "quality_planted".to_owned();
        result.status = EvalStatus::Failed;
        result.metrics = Some(scored.metrics);
        let directory = tempdir().unwrap();
        let path = directory.path().join("result.json");
        result.write(&path).unwrap();
        let restored = EvalResultV1::from_json(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(restored.status, EvalStatus::Failed);
        let Some(EvalMetrics::CodeQualityReviewerV2(restored_metrics)) = restored.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert_eq!(restored_metrics.defects_found, 2);
        assert_eq!(restored_metrics.defects_total, 3);
        assert!((restored_metrics.recall - (2.0 / 3.0)).abs() < f64::EPSILON);
        assert_eq!(restored_metrics.severity_total, 2);
    }
}
