use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::result::{EvalMetrics, EvalResultError, EvalResultV1, EvalStatus};

/// Loads and validates every result file under supplied files or directories.
///
/// # Errors
/// Rejects missing paths, empty result sets, unsupported schemas, malformed files, or I/O errors.
pub fn load_results(paths: &[PathBuf]) -> Result<Vec<EvalResultV1>, EvalReportError> {
    if paths.is_empty() {
        return Err(EvalReportError::NoPaths);
    }
    let mut files = Vec::new();
    for path in paths {
        collect_result_files(path, &mut files)?;
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err(EvalReportError::NoResults);
    }
    files
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path)?;
            EvalResultV1::from_json(&bytes)
                .map_err(|source| EvalReportError::InvalidResult { path, source })
        })
        .collect()
}

/// Renders role-oriented target evidence without choosing a winner.
///
/// # Errors
/// Rejects an empty result set.
#[allow(
    clippy::too_many_lines,
    reason = "single formatter keeps role sections and comparison columns visibly aligned"
)]
pub fn render_report(results: &[EvalResultV1]) -> Result<String, EvalReportError> {
    if results.is_empty() {
        return Err(EvalReportError::NoResults);
    }
    let mut groups = BTreeMap::<(String, String), Vec<&EvalResultV1>>::new();
    for result in results {
        groups
            .entry((result.suite_version.clone(), result.target.label()))
            .or_default()
            .push(result);
    }
    let mut output = String::new();
    let mut summaries = Vec::new();
    for ((suite_version, target), group) in &groups {
        let summary = TargetSummary::from_results(group);
        writeln!(output, "SUITE {suite_version}\nTARGET\n{target}")
            .expect("String write cannot fail");
        if group.iter().any(|result| result.synthetic) {
            writeln!(output, "SYNTHETIC — not routing evidence").expect("String write cannot fail");
        }
        writeln!(output, "\nImplementer").expect("String write cannot fail");
        for result in group.iter().filter(|result| {
            matches!(result.metrics, Some(EvalMetrics::Implementer(_)))
                || result.role == crate::domain::Role::Implementer
        }) {
            writeln!(
                output,
                "  {:36} {}",
                result.case_id,
                status_text(result.status)
            )
            .expect("String write cannot fail");
        }
        writeln!(
            output,
            "  pass                          {}/{}",
            summary.implementer_passed, summary.implementer_total
        )
        .expect("String write cannot fail");
        writeln!(
            output,
            "  plan mismatch                 {}/{}",
            summary.plan_mismatch_passed, summary.plan_mismatch_total
        )
        .expect("String write cannot fail");
        writeln!(output, "\nCode Quality Reviewer").expect("String write cannot fail");
        writeln!(
            output,
            "  planted findings              {}/{}",
            summary.quality_found, summary.quality_total
        )
        .expect("String write cannot fail");
        writeln!(
            output,
            "  false positives               {} (must-fix {})",
            summary.quality_false_positives, summary.quality_must_fix_false_positives
        )
        .expect("String write cannot fail");
        if summary.has_quality_v2 {
            writeln!(
                output,
                "  severity agreement           {}/{} detected (underclassified {} overclassified {})",
                summary.quality_severity_matches,
                summary.quality_severity_total,
                summary.quality_underclassified,
                summary.quality_overclassified
            )
            .expect("String write cannot fail");
            writeln!(
                output,
                "  duplicate findings           {}",
                summary.quality_duplicate_findings
            )
            .expect("String write cannot fail");
        }
        writeln!(output, "\nSpecification Reviewer").expect("String write cannot fail");
        writeln!(
            output,
            "  Missing                       {}/{}",
            summary.spec_missing_found, summary.spec_missing_total
        )
        .expect("String write cannot fail");
        writeln!(
            output,
            "  Wrong                         {}/{}",
            summary.spec_wrong_found, summary.spec_wrong_total
        )
        .expect("String write cannot fail");
        writeln!(
            output,
            "  Unrequested                   {}/{}",
            summary.spec_unrequested_found, summary.spec_unrequested_total
        )
        .expect("String write cannot fail");
        writeln!(
            output,
            "  false positives               {}",
            summary.spec_false_positives
        )
        .expect("String write cannot fail");
        if summary.has_spec_v2 {
            writeln!(
                output,
                "  duplicate findings           {}",
                summary.spec_duplicate_findings
            )
            .expect("String write cannot fail");
        }
        writeln!(
            output,
            "\nMedian latency                 {} ms\nMedian usage (provider-native) {} input / {} output",
            summary.median_latency, summary.median_input, summary.median_output,
        )
        .expect("String write cannot fail");
        let optional_units = [
            ("Median cache read units", summary.median_cache_read),
            ("Median cache write units", summary.median_cache_write),
            (
                "Median reasoning output units",
                summary.median_reasoning_output,
            ),
        ];
        for (label, value) in optional_units {
            if let Some(value) = value {
                writeln!(output, "{label}  {value}").expect("String write cannot fail");
            }
        }
        if let Some(bytes) = summary.median_injected_prompt_bytes {
            writeln!(output, "Median injected prompt bytes   {bytes}")
                .expect("String write cannot fail");
        }
        writeln!(
            output,
            "Usage units are provider-native (runtime-specific) and are not cross-provider normalized; never compare them across targets.\nInfrastructure failures        {}\n",
            summary.infrastructure_failures
        )
        .expect("String write cannot fail");
        for result in group
            .iter()
            .filter(|result| result.status != EvalStatus::Passed)
        {
            writeln!(
                output,
                "  {} rep {}: {}{}",
                result.case_id,
                result.repetition,
                status_text(result.status),
                result
                    .detail
                    .as_deref()
                    .map_or(String::new(), |detail| format!(" — {detail}"))
            )
            .expect("String write cannot fail");
        }
        output.push('\n');
        summaries.push((suite_version.clone(), target.clone(), summary));
    }
    if summaries.len() > 1 {
        writeln!(output, "COMPARISON").expect("String write cannot fail");
        writeln!(
            output,
            "Suite | Target | Implementer | Quality recall | Quality FP | Spec recall | Spec FP"
        )
        .expect("String write cannot fail");
        for (suite_version, target, summary) in summaries {
            writeln!(
                output,
                "{} | {} | {}/{} | {}/{} | {} | {}/{} | {}",
                suite_version,
                target,
                summary.implementer_passed,
                summary.implementer_total,
                summary.quality_found,
                summary.quality_total,
                summary.quality_false_positives,
                summary.spec_found(),
                summary.spec_total(),
                summary.spec_false_positives
            )
            .expect("String write cannot fail");
        }
        writeln!(
            output,
            "\nThe comparison intentionally excludes usage units: provider-reported usage is runtime-specific and is not cross-provider normalized. Latency and injected prompt bytes are the cross-target comparable dimensions."
        )
        .expect("String write cannot fail");
    }
    Ok(output)
}

fn collect_result_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), EvalReportError> {
    if path.is_file() {
        output.push(path.to_path_buf());
        return Ok(());
    }
    if !path.is_dir() {
        return Err(EvalReportError::MissingPath(path.to_path_buf()));
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            collect_result_files(&child, output)?;
        } else if child.file_name().is_some_and(|name| name == "result.json") {
            output.push(child);
        }
    }
    Ok(())
}

const fn status_text(status: EvalStatus) -> &'static str {
    match status {
        EvalStatus::Passed => "PASS",
        EvalStatus::Failed => "FAIL",
        EvalStatus::InfrastructureFailure => "INFRA",
    }
}

#[derive(Default)]
struct TargetSummary {
    implementer_passed: u32,
    implementer_total: u32,
    plan_mismatch_passed: u32,
    plan_mismatch_total: u32,
    quality_found: u32,
    quality_total: u32,
    quality_false_positives: u32,
    quality_must_fix_false_positives: u32,
    has_quality_v2: bool,
    quality_severity_matches: u32,
    quality_severity_total: u32,
    quality_underclassified: u32,
    quality_overclassified: u32,
    quality_duplicate_findings: u32,
    spec_missing_found: u32,
    spec_missing_total: u32,
    spec_wrong_found: u32,
    spec_wrong_total: u32,
    spec_unrequested_found: u32,
    spec_unrequested_total: u32,
    spec_false_positives: u32,
    has_spec_v2: bool,
    spec_duplicate_findings: u32,
    median_latency: u64,
    median_input: u64,
    median_output: u64,
    median_cache_read: Option<u64>,
    median_cache_write: Option<u64>,
    median_reasoning_output: Option<u64>,
    median_injected_prompt_bytes: Option<u64>,
    infrastructure_failures: u32,
}

impl TargetSummary {
    #[allow(
        clippy::too_many_lines,
        reason = "one accumulator keeps V1 and additive V2 metrics in suite-scoped summaries"
    )]
    fn from_results(results: &[&EvalResultV1]) -> Self {
        let mut summary = Self::default();
        let mut latency = Vec::new();
        let mut input = Vec::new();
        let mut output = Vec::new();
        let mut cache_read = Vec::new();
        let mut cache_write = Vec::new();
        let mut reasoning_output = Vec::new();
        let mut injected_prompt_bytes = Vec::new();
        for result in results {
            latency.push(result.latency_ms);
            input.push(result.usage.input_units);
            output.push(result.usage.output_units);
            cache_read.extend(result.usage.cache_read_units);
            cache_write.extend(result.usage.cache_write_units);
            reasoning_output.extend(result.usage.reasoning_output_units);
            injected_prompt_bytes.extend(result.injected_prompt_bytes);
            if result.status == EvalStatus::InfrastructureFailure {
                summary.infrastructure_failures = summary.infrastructure_failures.saturating_add(1);
            }
            match &result.metrics {
                Some(EvalMetrics::Implementer(metrics)) => {
                    summary.implementer_total = summary.implementer_total.saturating_add(1);
                    summary.implementer_passed = summary
                        .implementer_passed
                        .saturating_add(u32::from(result.status == EvalStatus::Passed));
                    if let Some(plan) = metrics.plan_mismatch_behavior {
                        summary.plan_mismatch_total = summary.plan_mismatch_total.saturating_add(1);
                        summary.plan_mismatch_passed =
                            summary.plan_mismatch_passed.saturating_add(u32::from(plan));
                    }
                }
                Some(EvalMetrics::CodeQualityReviewer(metrics)) => {
                    summary.quality_found =
                        summary.quality_found.saturating_add(metrics.defects_found);
                    summary.quality_total =
                        summary.quality_total.saturating_add(metrics.defects_total);
                    summary.quality_false_positives = summary
                        .quality_false_positives
                        .saturating_add(metrics.false_positives);
                    summary.quality_must_fix_false_positives = summary
                        .quality_must_fix_false_positives
                        .saturating_add(metrics.must_fix_false_positives);
                }
                Some(EvalMetrics::CodeQualityReviewerV2(metrics)) => {
                    summary.has_quality_v2 = true;
                    summary.quality_found =
                        summary.quality_found.saturating_add(metrics.defects_found);
                    summary.quality_total =
                        summary.quality_total.saturating_add(metrics.defects_total);
                    summary.quality_false_positives = summary
                        .quality_false_positives
                        .saturating_add(metrics.false_positives);
                    summary.quality_must_fix_false_positives = summary
                        .quality_must_fix_false_positives
                        .saturating_add(metrics.must_fix_false_positives);
                    summary.quality_severity_matches = summary
                        .quality_severity_matches
                        .saturating_add(metrics.severity_matches);
                    summary.quality_severity_total = summary
                        .quality_severity_total
                        .saturating_add(metrics.severity_total);
                    summary.quality_underclassified = summary
                        .quality_underclassified
                        .saturating_add(metrics.underclassified);
                    summary.quality_overclassified = summary
                        .quality_overclassified
                        .saturating_add(metrics.overclassified);
                    summary.quality_duplicate_findings = summary
                        .quality_duplicate_findings
                        .saturating_add(metrics.duplicate_findings);
                }
                Some(EvalMetrics::SpecReviewer(metrics)) => {
                    summary.spec_missing_found = summary
                        .spec_missing_found
                        .saturating_add(metrics.missing_found);
                    summary.spec_missing_total = summary
                        .spec_missing_total
                        .saturating_add(metrics.missing_total);
                    summary.spec_wrong_found =
                        summary.spec_wrong_found.saturating_add(metrics.wrong_found);
                    summary.spec_wrong_total =
                        summary.spec_wrong_total.saturating_add(metrics.wrong_total);
                    summary.spec_unrequested_found = summary
                        .spec_unrequested_found
                        .saturating_add(metrics.unrequested_found);
                    summary.spec_unrequested_total = summary
                        .spec_unrequested_total
                        .saturating_add(metrics.unrequested_total);
                    summary.spec_false_positives = summary
                        .spec_false_positives
                        .saturating_add(metrics.false_positives);
                }
                Some(EvalMetrics::SpecReviewerV2(metrics)) => {
                    summary.has_spec_v2 = true;
                    summary.spec_missing_found = summary
                        .spec_missing_found
                        .saturating_add(metrics.missing_found);
                    summary.spec_missing_total = summary
                        .spec_missing_total
                        .saturating_add(metrics.missing_total);
                    summary.spec_wrong_found =
                        summary.spec_wrong_found.saturating_add(metrics.wrong_found);
                    summary.spec_wrong_total =
                        summary.spec_wrong_total.saturating_add(metrics.wrong_total);
                    summary.spec_unrequested_found = summary
                        .spec_unrequested_found
                        .saturating_add(metrics.unrequested_found);
                    summary.spec_unrequested_total = summary
                        .spec_unrequested_total
                        .saturating_add(metrics.unrequested_total);
                    summary.spec_false_positives = summary
                        .spec_false_positives
                        .saturating_add(metrics.false_positives);
                    summary.spec_duplicate_findings = summary
                        .spec_duplicate_findings
                        .saturating_add(metrics.duplicate_findings);
                }
                None => {}
            }
        }
        summary.median_latency = median(&mut latency);
        summary.median_input = median(&mut input);
        summary.median_output = median(&mut output);
        // Medians over reporting results only; unavailable dimensions stay
        // None rather than pretending zero.
        summary.median_cache_read = optional_median(&mut cache_read);
        summary.median_cache_write = optional_median(&mut cache_write);
        summary.median_reasoning_output = optional_median(&mut reasoning_output);
        summary.median_injected_prompt_bytes = optional_median(&mut injected_prompt_bytes);
        summary
    }

    const fn spec_found(&self) -> u32 {
        self.spec_missing_found + self.spec_wrong_found + self.spec_unrequested_found
    }

    const fn spec_total(&self) -> u32 {
        self.spec_missing_total + self.spec_wrong_total + self.spec_unrequested_total
    }
}

fn optional_median(values: &mut [u64]) -> Option<u64> {
    (!values.is_empty()).then(|| median(values))
}

fn median(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        values[middle - 1].saturating_add(values[middle]) / 2
    }
}

#[derive(Debug, Error)]
pub enum EvalReportError {
    #[error("eval report requires at least one path")]
    NoPaths,
    #[error("no result.json files found")]
    NoResults,
    #[error("eval report path does not exist: {0}")]
    MissingPath(PathBuf),
    #[error("invalid eval result {path}: {source}")]
    InvalidResult {
        path: PathBuf,
        source: EvalResultError,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Role;
    use crate::eval::result::{
        EVAL_RESULT_SCHEMA_VERSION, EvalProvider, EvalTarget, EvalUsage, QualityMetrics,
        QualityMetricsV2,
    };
    use chrono::{TimeZone as _, Utc};

    #[test]
    fn median_handles_even_odd_and_empty_inputs() {
        assert_eq!(median(&mut []), 0);
        assert_eq!(median(&mut [9]), 9);
        assert_eq!(median(&mut [9, 1, 5]), 5);
        assert_eq!(median(&mut [10, 2]), 6);
    }

    #[test]
    fn report_separates_suite_versions_and_exposes_v2_quality_metrics() {
        let target = EvalTarget::new(EvalProvider::Fake, None).unwrap();
        let at = Utc
            .with_ymd_and_hms(2026, 8, 18, 12, 0, 0)
            .single()
            .unwrap();
        let base = |suite_version: &str, metrics: EvalMetrics| EvalResultV1 {
            requested_effort: None,
            schema_version: EVAL_RESULT_SCHEMA_VERSION,
            suite: "role_core".to_owned(),
            suite_version: suite_version.to_owned(),
            suite_fingerprint: "a".repeat(64),
            case_id: "quality_planted".to_owned(),
            repetition: 1,
            target: target.clone(),
            confirmed_model: None,
            provider_cli_version: None,
            role: Role::CodeQualityReviewer,
            status: EvalStatus::Passed,
            metrics: Some(metrics),
            usage: EvalUsage::default(),
            injected_prompt_bytes: None,
            latency_ms: 1,
            artifact_hash: None,
            diff_hash: "b".repeat(64),
            fixture_hash: "c".repeat(64),
            synthetic: true,
            started_at: at,
            finished_at: at,
            detail: None,
        };
        let v1 = base(
            "role_core_v1",
            EvalMetrics::CodeQualityReviewer(QualityMetrics {
                defects_found: 3,
                defects_total: 3,
                recall: 1.0,
                false_positives: 0,
                must_fix_false_positives: 0,
            }),
        );
        let v2 = base(
            "role_core_v2",
            EvalMetrics::CodeQualityReviewerV2(QualityMetricsV2 {
                defects_found: 3,
                defects_total: 3,
                recall: 1.0,
                false_positives: 0,
                must_fix_false_positives: 0,
                severity_matches: 2,
                severity_total: 3,
                underclassified: 1,
                overclassified: 0,
                duplicate_findings: 1,
            }),
        );
        let mut with_resources = base(
            "role_core_v2",
            EvalMetrics::CodeQualityReviewer(QualityMetrics {
                defects_found: 3,
                defects_total: 3,
                recall: 1.0,
                false_positives: 0,
                must_fix_false_positives: 0,
            }),
        );
        with_resources.repetition = 2;
        with_resources.usage.cache_read_units = Some(313_292);
        with_resources.usage.reasoning_output_units = Some(219);
        with_resources.injected_prompt_bytes = Some(18_432);
        let report = render_report(&[v1, v2, with_resources]).unwrap();
        assert_eq!(report.matches("SUITE role_core_").count(), 2);
        assert!(report.contains("severity agreement           2/3 detected"));
        assert!(report.contains("duplicate findings           1"));
        assert!(report.contains("role_core_v1 | fake / native_default"));
        assert!(report.contains("role_core_v2 | fake / native_default"));
        // Provider-native usage is labeled and explicitly marked incomparable
        // in every per-target section and once more under the comparison.
        assert!(report.contains("Median usage (provider-native)"));
        assert_eq!(
            report
                .matches("Usage units are provider-native (runtime-specific) and are not cross-provider normalized")
                .count(),
            2
        );
        assert!(report.contains("The comparison intentionally excludes usage units"));
        // Optional dimensions render only where a result reported them.
        assert!(report.contains("Median cache read units  313292"));
        assert!(report.contains("Median reasoning output units  219"));
        assert!(report.contains("Median injected prompt bytes   18432"));
        assert_eq!(report.matches("Median cache write units").count(), 0);
    }
}
