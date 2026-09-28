use std::collections::{BTreeSet, HashSet};

use serde::Deserialize;
use thiserror::Error;

use super::case::{
    EvalCase, EvalScorer, FindingLocation, QualityGroundTruth, QualityGroundTruthV2,
    QualitySeverity, SpecCategory, SpecGroundTruth, SpecGroundTruthV2,
};
use super::result::{
    EvalMetrics, ImplementerMetrics, QualityMetrics, QualityMetricsV2, SpecMetrics, SpecMetricsV2,
};

#[derive(Clone, Copy)]
pub struct ScoreInput<'a> {
    pub artifact: Option<&'a str>,
    pub diff: &'a str,
    pub validation_pass: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScoredOutcome {
    pub passed: bool,
    pub metrics: EvalMetrics,
    pub detail: Option<String>,
}

pub fn score(case: &EvalCase, input: ScoreInput<'_>) -> Result<ScoredOutcome, ScoringError> {
    match case.scorer {
        EvalScorer::Implementer {
            required_paths,
            allowed_paths,
            validation,
            require_plan_mismatch,
            forbid_public_additions,
        } => Ok(score_implementer(
            required_paths,
            allowed_paths,
            !validation.is_empty(),
            require_plan_mismatch,
            forbid_public_additions,
            input,
        )),
        EvalScorer::Quality {
            ground_truth,
            max_false_positives,
        } => score_quality(ground_truth, max_false_positives, input),
        EvalScorer::Specification {
            ground_truth,
            max_false_positives,
        } => score_spec(ground_truth, max_false_positives, input),
        EvalScorer::QualityV2 {
            ground_truth,
            max_false_positives,
        } => score_quality_v2(ground_truth, max_false_positives, input),
        EvalScorer::SpecificationV2 {
            ground_truth,
            max_false_positives,
        } => score_spec_v2(ground_truth, max_false_positives, input),
    }
}

fn score_implementer(
    required_paths: &[&str],
    allowed_paths: &[&str],
    has_validation: bool,
    require_plan_mismatch: bool,
    forbid_public_additions: bool,
    input: ScoreInput<'_>,
) -> ScoredOutcome {
    let facts = DiffFacts::parse(input.diff);
    let allowed = allowed_paths.iter().copied().collect::<HashSet<_>>();
    let unexpected_files = facts
        .paths
        .iter()
        .filter(|path| !allowed.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let required_present = required_paths
        .iter()
        .all(|required| facts.paths.contains(*required));
    let no_public_addition = !forbid_public_additions || !facts.public_surface_added;
    let scope_pass = required_present
        && unexpected_files.is_empty()
        && facts.deletions.is_empty()
        && no_public_addition
        && (!require_plan_mismatch || facts.paths.is_empty());
    let plan_mismatch_behavior =
        require_plan_mismatch.then(|| parse_plan_outcome(input.artifact).unwrap_or(false));
    let validation_pass = has_validation.then_some(input.validation_pass.unwrap_or(false));
    let behavioral_pass = if require_plan_mismatch {
        plan_mismatch_behavior == Some(true) && facts.paths.is_empty()
    } else {
        validation_pass == Some(true) && !facts.paths.is_empty()
    };
    let metrics = ImplementerMetrics {
        behavioral_pass,
        scope_pass,
        plan_mismatch_behavior,
        validation_pass,
        unexpected_files,
        deletions: facts.deletions.into_iter().collect(),
    };
    let passed = metrics.behavioral_pass && metrics.scope_pass;
    ScoredOutcome {
        passed,
        metrics: EvalMetrics::Implementer(metrics),
        detail: (!passed).then(|| "behavior or scope criterion failed".to_owned()),
    }
}

fn score_quality(
    expected: &[QualityGroundTruth],
    max_false_positives: u32,
    input: ScoreInput<'_>,
) -> Result<ScoredOutcome, ScoringError> {
    if !input.diff.trim().is_empty() {
        return Err(ScoringError::ReviewerModifiedRepository);
    }
    let response: QualityResponse = parse_response(input.artifact)?;
    if response.eval_version != 1 {
        return Err(ScoringError::UnsupportedResponseVersion(
            response.eval_version,
        ));
    }
    let mut matched = HashSet::new();
    let mut false_positives = 0_u32;
    let mut must_fix_false_positives = 0_u32;
    for finding in &response.findings {
        let candidate = expected
            .iter()
            .enumerate()
            .find(|(index, truth)| !matched.contains(index) && quality_matches(finding, truth));
        if let Some((index, _)) = candidate {
            matched.insert(index);
        } else {
            false_positives = false_positives.saturating_add(1);
            if finding.severity == QualitySeverityDto::MustFix {
                must_fix_false_positives = must_fix_false_positives.saturating_add(1);
            }
        }
    }
    let defects_found = u32::try_from(matched.len()).expect("finding count fits u32");
    let defects_total = u32::try_from(expected.len()).expect("ground truth count fits u32");
    let recall = if defects_total == 0 {
        1.0
    } else {
        f64::from(defects_found) / f64::from(defects_total)
    };
    let all_must_fix_found = expected.iter().enumerate().all(|(index, truth)| {
        truth.severity != QualitySeverity::MustFix || matched.contains(&index)
    });
    let passed = all_must_fix_found && false_positives <= max_false_positives;
    Ok(ScoredOutcome {
        passed,
        metrics: EvalMetrics::CodeQualityReviewer(QualityMetrics {
            defects_found,
            defects_total,
            recall,
            false_positives,
            must_fix_false_positives,
        }),
        detail: (!passed).then(|| "quality recall or false-positive criterion failed".to_owned()),
    })
}

fn score_spec(
    expected: &[SpecGroundTruth],
    max_false_positives: u32,
    input: ScoreInput<'_>,
) -> Result<ScoredOutcome, ScoringError> {
    if !input.diff.trim().is_empty() {
        return Err(ScoringError::ReviewerModifiedRepository);
    }
    let response: SpecResponse = parse_response(input.artifact)?;
    if response.eval_version != 1 {
        return Err(ScoringError::UnsupportedResponseVersion(
            response.eval_version,
        ));
    }
    let mut matched = HashSet::new();
    let mut false_positives = 0_u32;
    for finding in &response.findings {
        let candidate = expected
            .iter()
            .enumerate()
            .find(|(index, truth)| !matched.contains(index) && spec_matches(finding, truth));
        if let Some((index, _)) = candidate {
            matched.insert(index);
        } else {
            false_positives = false_positives.saturating_add(1);
        }
    }
    let count = |category: SpecCategory| {
        expected
            .iter()
            .enumerate()
            .filter(|(_, truth)| truth.category == category)
            .fold((0_u32, 0_u32), |(found, total), (index, _)| {
                (
                    found + u32::from(matched.contains(&index)),
                    total.saturating_add(1),
                )
            })
    };
    let (missing_found, missing_total) = count(SpecCategory::Missing);
    let (wrong_found, wrong_total) = count(SpecCategory::Wrong);
    let (unrequested_found, unrequested_total) = count(SpecCategory::Unrequested);
    let passed = missing_found == missing_total
        && wrong_found == wrong_total
        && unrequested_found == unrequested_total
        && false_positives <= max_false_positives;
    Ok(ScoredOutcome {
        passed,
        metrics: EvalMetrics::SpecReviewer(SpecMetrics {
            missing_found,
            missing_total,
            wrong_found,
            wrong_total,
            unrequested_found,
            unrequested_total,
            false_positives,
        }),
        detail: (!passed)
            .then(|| "spec category recall or false-positive criterion failed".to_owned()),
    })
}

fn score_quality_v2(
    expected: &[QualityGroundTruthV2],
    max_false_positives: u32,
    input: ScoreInput<'_>,
) -> Result<ScoredOutcome, ScoringError> {
    if !input.diff.trim().is_empty() {
        return Err(ScoringError::ReviewerModifiedRepository);
    }
    let response: QualityResponse = parse_response(input.artifact)?;
    if response.eval_version != 1 {
        return Err(ScoringError::UnsupportedResponseVersion(
            response.eval_version,
        ));
    }
    let mut matched = HashSet::new();
    let mut false_positives = 0_u32;
    let mut must_fix_false_positives = 0_u32;
    let mut duplicate_findings = 0_u32;
    let mut severity_matches = 0_u32;
    let mut underclassified = 0_u32;
    let mut overclassified = 0_u32;
    for finding in &response.findings {
        let candidate = expected.iter().enumerate().find(|(index, truth)| {
            !matched.contains(index) && quality_identity_matches_v2(finding, truth)
        });
        if let Some((index, truth)) = candidate {
            matched.insert(index);
            if finding.severity.as_str() == truth.severity.as_str() {
                severity_matches = severity_matches.saturating_add(1);
            } else if truth.severity == QualitySeverity::MustFix
                && finding.severity == QualitySeverityDto::Minor
            {
                underclassified = underclassified.saturating_add(1);
            } else {
                overclassified = overclassified.saturating_add(1);
            }
        } else if expected.iter().enumerate().any(|(index, truth)| {
            matched.contains(&index) && quality_identity_matches_v2(finding, truth)
        }) {
            duplicate_findings = duplicate_findings.saturating_add(1);
        } else {
            false_positives = false_positives.saturating_add(1);
            if finding.severity == QualitySeverityDto::MustFix {
                must_fix_false_positives = must_fix_false_positives.saturating_add(1);
            }
        }
    }
    let defects_found = u32::try_from(matched.len()).expect("finding count fits u32");
    let defects_total = u32::try_from(expected.len()).expect("ground truth count fits u32");
    let recall = if defects_total == 0 {
        1.0
    } else {
        f64::from(defects_found) / f64::from(defects_total)
    };
    let all_must_fix_found = expected.iter().enumerate().all(|(index, truth)| {
        truth.severity != QualitySeverity::MustFix || matched.contains(&index)
    });
    let passed = all_must_fix_found && false_positives <= max_false_positives;
    Ok(ScoredOutcome {
        passed,
        metrics: EvalMetrics::CodeQualityReviewerV2(QualityMetricsV2 {
            defects_found,
            defects_total,
            recall,
            false_positives,
            must_fix_false_positives,
            severity_matches,
            severity_total: defects_found,
            underclassified,
            overclassified,
            duplicate_findings,
        }),
        detail: (!passed).then(|| "quality recall or false-positive criterion failed".to_owned()),
    })
}

fn score_spec_v2(
    expected: &[SpecGroundTruthV2],
    max_false_positives: u32,
    input: ScoreInput<'_>,
) -> Result<ScoredOutcome, ScoringError> {
    if !input.diff.trim().is_empty() {
        return Err(ScoringError::ReviewerModifiedRepository);
    }
    let response: SpecResponse = parse_response(input.artifact)?;
    if response.eval_version != 1 {
        return Err(ScoringError::UnsupportedResponseVersion(
            response.eval_version,
        ));
    }
    let mut matched = HashSet::new();
    let mut false_positives = 0_u32;
    let mut duplicate_findings = 0_u32;
    for finding in &response.findings {
        let candidate = expected.iter().enumerate().find(|(index, truth)| {
            !matched.contains(index) && spec_identity_matches_v2(finding, truth)
        });
        if let Some((index, _)) = candidate {
            matched.insert(index);
        } else if expected.iter().enumerate().any(|(index, truth)| {
            matched.contains(&index) && spec_identity_matches_v2(finding, truth)
        }) {
            duplicate_findings = duplicate_findings.saturating_add(1);
        } else {
            false_positives = false_positives.saturating_add(1);
        }
    }
    let count = |category: SpecCategory| {
        expected
            .iter()
            .enumerate()
            .filter(|(_, truth)| truth.category == category)
            .fold((0_u32, 0_u32), |(found, total), (index, _)| {
                (
                    found + u32::from(matched.contains(&index)),
                    total.saturating_add(1),
                )
            })
    };
    let (missing_found, missing_total) = count(SpecCategory::Missing);
    let (wrong_found, wrong_total) = count(SpecCategory::Wrong);
    let (unrequested_found, unrequested_total) = count(SpecCategory::Unrequested);
    let passed = missing_found == missing_total
        && wrong_found == wrong_total
        && unrequested_found == unrequested_total
        && false_positives <= max_false_positives;
    Ok(ScoredOutcome {
        passed,
        metrics: EvalMetrics::SpecReviewerV2(SpecMetricsV2 {
            missing_found,
            missing_total,
            wrong_found,
            wrong_total,
            unrequested_found,
            unrequested_total,
            false_positives,
            duplicate_findings,
        }),
        detail: (!passed)
            .then(|| "spec category recall or false-positive criterion failed".to_owned()),
    })
}

fn quality_matches(finding: &QualityFinding, truth: &QualityGroundTruth) -> bool {
    finding.file == truth.file
        && near_line(finding.line, truth.line_start, truth.line_end)
        && finding.severity.as_str() == truth.severity.as_str()
        && contains_concept(&finding.summary, truth.concepts)
}

fn spec_matches(finding: &SpecFinding, truth: &SpecGroundTruth) -> bool {
    finding.file == truth.file
        && near_line(finding.line, truth.line_start, truth.line_end)
        && finding.category.as_str() == truth.category.as_str()
        && contains_concept(&finding.summary, truth.concepts)
}

fn quality_identity_matches_v2(finding: &QualityFinding, truth: &QualityGroundTruthV2) -> bool {
    truth
        .locations
        .iter()
        .any(|location| location_matches(finding.file.as_str(), finding.line, location))
        && contains_concept(&finding.summary, truth.concepts)
}

fn spec_identity_matches_v2(finding: &SpecFinding, truth: &SpecGroundTruthV2) -> bool {
    truth
        .locations
        .iter()
        .any(|location| location_matches(finding.file.as_str(), finding.line, location))
        && finding.category.as_str() == truth.category.as_str()
        && contains_concept(&finding.summary, truth.concepts)
}

fn location_matches(file: &str, line: u32, location: &FindingLocation) -> bool {
    file == location.file && near_line(line, location.line_start, location.line_end)
}

fn near_line(line: u32, start: u32, end: u32) -> bool {
    line >= start.saturating_sub(2) && line <= end.saturating_add(2)
}

fn contains_concept(summary: &str, concepts: &[&str]) -> bool {
    let summary = summary.to_lowercase();
    concepts
        .iter()
        .any(|concept| summary.contains(&concept.to_lowercase()))
}

fn parse_plan_outcome(artifact: Option<&str>) -> Result<bool, ScoringError> {
    #[derive(Deserialize)]
    struct Outcome {
        eval_outcome: String,
    }
    let outcome: Outcome = parse_response(artifact)?;
    Ok(outcome.eval_outcome == "plan_mismatch")
}

fn parse_response<T: for<'de> Deserialize<'de>>(artifact: Option<&str>) -> Result<T, ScoringError> {
    let artifact = artifact.ok_or(ScoringError::MissingArtifact)?;
    for block in json_blocks(artifact) {
        if let Ok(response) = serde_json::from_str(block) {
            return Ok(response);
        }
    }
    Err(ScoringError::MissingStructuredResponse)
}

fn json_blocks(markdown: &str) -> impl Iterator<Item = &str> {
    markdown.split("```json").skip(1).filter_map(|tail| {
        tail.split_once("```")
            .map(|(block, _)| block.trim())
            .filter(|block| !block.is_empty())
    })
}

#[derive(Default)]
struct DiffFacts {
    paths: BTreeSet<String>,
    deletions: BTreeSet<String>,
    public_surface_added: bool,
}

impl DiffFacts {
    fn parse(diff: &str) -> Self {
        let mut facts = Self::default();
        let mut current = None;
        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix("diff --git a/") {
                let path = rest
                    .split_once(" b/")
                    .map_or(rest, |(_, destination)| destination)
                    .to_owned();
                facts.paths.insert(path.clone());
                current = Some(path);
            } else if line == "deleted file mode 100644" {
                if let Some(path) = &current {
                    facts.deletions.insert(path.clone());
                }
            } else if line.starts_with('+')
                && !line.starts_with("+++")
                && line
                    .trim_start_matches('+')
                    .trim_start()
                    .starts_with("pub ")
            {
                facts.public_surface_added = true;
            }
        }
        facts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum QualitySeverityDto {
    MustFix,
    Minor,
}

impl QualitySeverityDto {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MustFix => "must_fix",
            Self::Minor => "minor",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualityFinding {
    severity: QualitySeverityDto,
    file: String,
    line: u32,
    summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualityResponse {
    eval_version: u32,
    findings: Vec<QualityFinding>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SpecCategoryDto {
    Missing,
    Wrong,
    Unrequested,
}

impl SpecCategoryDto {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Wrong => "wrong",
            Self::Unrequested => "unrequested",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecFinding {
    category: SpecCategoryDto,
    file: String,
    line: u32,
    summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecResponse {
    eval_version: u32,
    findings: Vec<SpecFinding>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ScoringError {
    #[error("candidate produced no verified artifact")]
    MissingArtifact,
    #[error("artifact has no valid fenced eval JSON response")]
    MissingStructuredResponse,
    #[error("unsupported structured eval response version {0}")]
    UnsupportedResponseVersion(u32),
    #[error("read-only reviewer modified repository")]
    ReviewerModifiedRepository,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::case::{ROLE_CORE_CASES, ROLE_CORE_CASES_V2};

    fn input(artifact: &str) -> ScoreInput<'_> {
        ScoreInput {
            artifact: Some(artifact),
            diff: "",
            validation_pass: None,
        }
    }

    #[test]
    fn quality_matcher_is_location_concept_severity_and_one_to_one() {
        let case = &ROLE_CORE_CASES[3];
        let artifact = r#"```json
{"eval_version":1,"findings":[
{"severity":"must_fix","file":"src/lib.rs","line":12,"summary":"Unnecessary abstraction with one caller"},
{"severity":"must_fix","file":"src/lib.rs","line":20,"summary":"Duplicate representation stores raw and normalized"},
{"severity":"minor","file":"src/lib.rs","line":28,"summary":"Nested control flow obscures classification"}
]}
```"#;
        let scored = score(case, input(artifact)).unwrap();
        assert!(scored.passed);
        let EvalMetrics::CodeQualityReviewer(metrics) = scored.metrics else {
            panic!("quality metrics expected")
        };
        assert_eq!(metrics.defects_found, 3);
        assert_eq!(metrics.false_positives, 0);
    }

    #[test]
    fn quality_wrong_file_duplicate_and_invented_finding_are_false_positives() {
        let case = &ROLE_CORE_CASES[3];
        let artifact = r#"```json
{"eval_version":1,"findings":[
{"severity":"must_fix","file":"wrong.rs","line":5,"summary":"Unnecessary abstraction"},
{"severity":"must_fix","file":"src/lib.rs","line":5,"summary":"Unnecessary abstraction"},
{"severity":"must_fix","file":"src/lib.rs","line":6,"summary":"Unnecessary abstraction"},
{"severity":"minor","file":"src/lib.rs","line":99,"summary":"Invented issue"}
]}
```"#;
        let scored = score(case, input(artifact)).unwrap();
        let EvalMetrics::CodeQualityReviewer(metrics) = scored.metrics else {
            panic!("quality metrics expected")
        };
        assert_eq!(metrics.defects_found, 1);
        assert_eq!(metrics.false_positives, 3);
        assert_eq!(metrics.must_fix_false_positives, 2);
    }

    #[test]
    fn spec_categories_never_cross_match_and_clean_output_has_no_false_positives() {
        let divergence = &ROLE_CORE_CASES[5];
        let wrong_category = r#"```json
{"eval_version":1,"findings":[{"category":"wrong","file":"src/lib.rs","line":2,"summary":"Negative quantity validation missing"}]}
```"#;
        let scored = score(divergence, input(wrong_category)).unwrap();
        let EvalMetrics::SpecReviewer(metrics) = scored.metrics else {
            panic!("spec metrics expected")
        };
        assert_eq!(metrics.missing_found, 0);
        assert_eq!(metrics.false_positives, 1);

        let clean = score(
            &ROLE_CORE_CASES[6],
            input("```json\n{\"eval_version\":1,\"findings\":[]}\n```"),
        )
        .unwrap();
        assert!(clean.passed);
    }

    #[test]
    fn invalid_plan_requires_empty_diff_and_exact_structured_outcome() {
        let case = &ROLE_CORE_CASES[2];
        let artifact = "```json\n{\"eval_outcome\":\"plan_mismatch\"}\n```";
        let passed = score(case, input(artifact)).unwrap();
        assert!(passed.passed);
        let failed = score(
            case,
            ScoreInput {
                artifact: Some(artifact),
                diff: "diff --git a/new.rs b/new.rs\nnew file mode 100644\n",
                validation_pass: None,
            },
        )
        .unwrap();
        assert!(!failed.passed);
    }

    #[test]
    fn quality_v2_tracks_severity_without_turning_identity_match_into_false_positive() {
        let case = &ROLE_CORE_CASES_V2[3];
        let artifact = r#"```json
{"eval_version":1,"findings":[
{"severity":"minor","file":"src/lib.rs","line":12,"summary":"Unnecessary abstraction with one caller"},
{"severity":"must_fix","file":"src/lib.rs","line":12,"summary":"Unnecessary abstraction with one caller"},
{"severity":"must_fix","file":"src/lib.rs","line":20,"summary":"Duplicate representation stores raw and normalized"},
{"severity":"minor","file":"src/lib.rs","line":28,"summary":"Nested control flow obscures classification"}
]}
```"#;
        let scored = score(case, input(artifact)).unwrap();
        assert!(scored.passed);
        let EvalMetrics::CodeQualityReviewerV2(metrics) = scored.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert_eq!(metrics.defects_found, 3);
        assert_eq!(metrics.false_positives, 0);
        assert_eq!(metrics.duplicate_findings, 1);
        assert_eq!(metrics.severity_matches, 2);
        assert_eq!(metrics.severity_total, 3);
        assert_eq!(metrics.underclassified, 1);
    }

    #[test]
    fn spec_v2_uses_multi_location_identity_and_counts_duplicates_separately() {
        let case = &ROLE_CORE_CASES_V2[5];
        let artifact = r#"```json
{"eval_version":1,"findings":[
{"category":"missing","file":"src/lib.rs","line":1,"summary":"Negative quantity validation is missing; reject negative quantity"},
{"category":"wrong","file":"src/lib.rs","line":3,"summary":"Discount incorrectly includes shipping instead of subtotal-only boundary"},
{"category":"unrequested","file":"src/lib.rs","line":5,"summary":"Unrequested coupon feature EXTRA5"},
{"category":"unrequested","file":"src/lib.rs","line":5,"summary":"Duplicate report of coupon EXTRA5 feature"}
]}
```"#;
        let scored = score(case, input(artifact)).unwrap();
        assert!(scored.passed);
        let EvalMetrics::SpecReviewerV2(metrics) = scored.metrics else {
            panic!("v2 spec metrics expected")
        };
        assert_eq!(metrics.missing_found, 1);
        assert_eq!(metrics.wrong_found, 1);
        assert_eq!(metrics.unrequested_found, 1);
        assert_eq!(metrics.false_positives, 0);
        assert_eq!(metrics.duplicate_findings, 1);
    }

    #[test]
    fn quality_v2_severity_total_counts_only_detected_defects() {
        let planted = &ROLE_CORE_CASES_V2[3];
        let partial = input(
            "```json\n{\"eval_version\":1,\"findings\":[\n\
             {\"severity\":\"must_fix\",\"file\":\"src/lib.rs\",\"line\":3,\"summary\":\"Unnecessary abstraction with one caller\"},\n\
             {\"severity\":\"minor\",\"file\":\"src/lib.rs\",\"line\":28,\"summary\":\"Nested control flow obscures classification\"}\n]}\n```",
        );
        let scored = score(planted, partial).unwrap();
        assert!(!scored.passed);
        let EvalMetrics::CodeQualityReviewerV2(metrics) = scored.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert_eq!(metrics.defects_found, 2);
        assert_eq!(metrics.defects_total, 3);
        assert!((metrics.recall - (2.0 / 3.0)).abs() < f64::EPSILON);
        assert_eq!(metrics.severity_total, 2);
        assert_eq!(
            metrics.severity_matches + metrics.underclassified + metrics.overclassified,
            2
        );

        let zero = score(
            planted,
            input("```json\n{\"eval_version\":1,\"findings\":[]}\n```"),
        )
        .unwrap();
        let EvalMetrics::CodeQualityReviewerV2(zero_metrics) = zero.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert_eq!(
            (
                zero_metrics.defects_found,
                zero_metrics.defects_total,
                zero_metrics.severity_total
            ),
            (0, 3, 0)
        );

        let clean = score(
            &ROLE_CORE_CASES_V2[4],
            input("```json\n{\"eval_version\":1,\"findings\":[]}\n```"),
        )
        .unwrap();
        let EvalMetrics::CodeQualityReviewerV2(clean_metrics) = clean.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert_eq!(
            (
                clean_metrics.defects_found,
                clean_metrics.defects_total,
                clean_metrics.severity_total
            ),
            (0, 0, 0)
        );

        let invented = score(
            &ROLE_CORE_CASES_V2[4],
            input(
                "```json\n{\"eval_version\":1,\"findings\":[{\"severity\":\"minor\",\"file\":\"src/lib.rs\",\"line\":1,\"summary\":\"Invented issue\"}]}\n```",
            ),
        )
        .unwrap();
        let EvalMetrics::CodeQualityReviewerV2(invented_metrics) = invented.metrics else {
            panic!("v2 quality metrics expected")
        };
        assert!(!invented.passed);
        assert_eq!(invented_metrics.false_positives, 1);
        assert_eq!(invented_metrics.severity_total, 0);
    }
}
