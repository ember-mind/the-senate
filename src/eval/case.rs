use crate::domain::{Role, WorkflowKind};

pub const ROLE_CORE_SUITE_VERSION: &str = "role_core_v1";
pub const ROLE_CORE_SUITE_VERSION_V2: &str = "role_core_v2";
pub const ROLE_CORE_SUITE_VERSION_V3: &str = "role_core_v3";

#[derive(Clone, Copy, Debug)]
pub struct FixtureFile {
    pub path: &'static str,
    pub contents: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct ValidationCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualitySeverity {
    MustFix,
    Minor,
}

impl QualitySeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MustFix => "must_fix",
            Self::Minor => "minor",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecCategory {
    Missing,
    Wrong,
    Unrequested,
}

impl SpecCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Wrong => "wrong",
            Self::Unrequested => "unrequested",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QualityGroundTruth {
    pub id: &'static str,
    pub file: &'static str,
    pub line_start: u32,
    pub line_end: u32,
    pub severity: QualitySeverity,
    pub concepts: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
pub struct SpecGroundTruth {
    pub id: &'static str,
    pub file: &'static str,
    pub line_start: u32,
    pub line_end: u32,
    pub category: SpecCategory,
    pub concepts: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
pub struct FindingLocation {
    pub file: &'static str,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct QualityGroundTruthV2 {
    pub id: &'static str,
    pub locations: &'static [FindingLocation],
    pub severity: QualitySeverity,
    pub concepts: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
pub struct SpecGroundTruthV2 {
    pub id: &'static str,
    pub locations: &'static [FindingLocation],
    pub category: SpecCategory,
    pub concepts: &'static [&'static str],
}

#[derive(Clone, Copy, Debug)]
pub enum EvalScorer {
    Implementer {
        required_paths: &'static [&'static str],
        allowed_paths: &'static [&'static str],
        validation: &'static [ValidationCommand],
        require_plan_mismatch: bool,
        forbid_public_additions: bool,
    },
    Quality {
        ground_truth: &'static [QualityGroundTruth],
        max_false_positives: u32,
    },
    Specification {
        ground_truth: &'static [SpecGroundTruth],
        max_false_positives: u32,
    },
    QualityV2 {
        ground_truth: &'static [QualityGroundTruthV2],
        max_false_positives: u32,
    },
    SpecificationV2 {
        ground_truth: &'static [SpecGroundTruthV2],
        max_false_positives: u32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct EvalCase {
    pub id: &'static str,
    pub suite_version: &'static str,
    pub target_role: Role,
    pub workflow: WorkflowKind,
    pub fixture: &'static [FixtureFile],
    pub task: &'static str,
    pub scorer: EvalScorer,
}

const CARGO_TEST: &[ValidationCommand] = &[ValidationCommand {
    program: "cargo",
    args: &["test", "--quiet", "--offline"],
}];

const BASIC_FILES: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v1/implementer_basic_bugfix/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v1/implementer_basic_bugfix/Cargo.toml"),
    },
    FixtureFile {
        path: "Cargo.lock",
        contents: include_str!("../../evals/role_core_v1/implementer_basic_bugfix/Cargo.lock"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v1/implementer_basic_bugfix/src/lib.rs"),
    },
];

const SCOPE_FILES: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v1/implementer_scope_discipline/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v1/implementer_scope_discipline/Cargo.toml"),
    },
    FixtureFile {
        path: "Cargo.lock",
        contents: include_str!("../../evals/role_core_v1/implementer_scope_discipline/Cargo.lock"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v1/implementer_scope_discipline/src/lib.rs"),
    },
];

const BASIC_FILES_V3: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v3/implementer_basic_bugfix/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v3/implementer_basic_bugfix/Cargo.toml"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v3/implementer_basic_bugfix/src/lib.rs"),
    },
];

const SCOPE_FILES_V3: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v3/implementer_scope_discipline/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v3/implementer_scope_discipline/Cargo.toml"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v3/implementer_scope_discipline/src/lib.rs"),
    },
];

const INVALID_PLAN_FILES: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v1/implementer_invalid_plan_stop/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v1/implementer_invalid_plan_stop/Cargo.toml"),
    },
    FixtureFile {
        path: "Cargo.lock",
        contents: include_str!("../../evals/role_core_v1/implementer_invalid_plan_stop/Cargo.lock"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v1/implementer_invalid_plan_stop/src/lib.rs"),
    },
];

const QUALITY_PLANTED_FILES: &[FixtureFile] = &[FixtureFile {
    path: "src/lib.rs",
    contents: include_str!("../../evals/role_core_v1/quality_planted/src/lib.rs"),
}];

const QUALITY_CLEAN_FILES: &[FixtureFile] = &[FixtureFile {
    path: "src/lib.rs",
    contents: include_str!("../../evals/role_core_v1/quality_clean/src/lib.rs"),
}];

const SPEC_DIVERGENCE_FILES: &[FixtureFile] = &[FixtureFile {
    path: "src/lib.rs",
    contents: include_str!("../../evals/role_core_v1/spec_missing_wrong_unrequested/src/lib.rs"),
}];

const SPEC_CLEAN_FILES: &[FixtureFile] = &[FixtureFile {
    path: "src/lib.rs",
    contents: include_str!("../../evals/role_core_v1/spec_clean/src/lib.rs"),
}];

const QUALITY_PLANTED_FILES_V2: &[FixtureFile] = &[
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v2/quality_planted/Cargo.toml"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v2/quality_planted/src/lib.rs"),
    },
];

const QUALITY_CLEAN_FILES_V2: &[FixtureFile] = &[
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v2/quality_clean/Cargo.toml"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v2/quality_clean/src/lib.rs"),
    },
];

const SPEC_DIVERGENCE_FILES_V2: &[FixtureFile] = &[
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!(
            "../../evals/role_core_v2/spec_missing_wrong_unrequested/Cargo.toml"
        ),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!(
            "../../evals/role_core_v2/spec_missing_wrong_unrequested/src/lib.rs"
        ),
    },
];

const SPEC_CLEAN_FILES_V2: &[FixtureFile] = &[
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v2/spec_clean/Cargo.toml"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v2/spec_clean/src/lib.rs"),
    },
];

const QUALITY_PLANTED_FILES_V3: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v3/quality_planted/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v3/quality_planted/Cargo.toml"),
    },
    FixtureFile {
        path: "Cargo.lock",
        contents: include_str!("../../evals/role_core_v3/quality_planted/Cargo.lock"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v3/quality_planted/src/lib.rs"),
    },
];

const QUALITY_CLEAN_FILES_V3: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v3/quality_clean/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v3/quality_clean/Cargo.toml"),
    },
    FixtureFile {
        path: "Cargo.lock",
        contents: include_str!("../../evals/role_core_v3/quality_clean/Cargo.lock"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v3/quality_clean/src/lib.rs"),
    },
];

const SPEC_DIVERGENCE_FILES_V3: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!(
            "../../evals/role_core_v3/spec_missing_wrong_unrequested/.gitignore"
        ),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!(
            "../../evals/role_core_v3/spec_missing_wrong_unrequested/Cargo.toml"
        ),
    },
    FixtureFile {
        path: "Cargo.lock",
        contents: include_str!(
            "../../evals/role_core_v3/spec_missing_wrong_unrequested/Cargo.lock"
        ),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!(
            "../../evals/role_core_v3/spec_missing_wrong_unrequested/src/lib.rs"
        ),
    },
];

const SPEC_CLEAN_FILES_V3: &[FixtureFile] = &[
    FixtureFile {
        path: ".gitignore",
        contents: include_str!("../../evals/role_core_v3/spec_clean/.gitignore"),
    },
    FixtureFile {
        path: "Cargo.toml",
        contents: include_str!("../../evals/role_core_v3/spec_clean/Cargo.toml"),
    },
    FixtureFile {
        path: "Cargo.lock",
        contents: include_str!("../../evals/role_core_v3/spec_clean/Cargo.lock"),
    },
    FixtureFile {
        path: "src/lib.rs",
        contents: include_str!("../../evals/role_core_v3/spec_clean/src/lib.rs"),
    },
];

const QUALITY_GROUND_TRUTH: &[QualityGroundTruth] = &[
    QualityGroundTruth {
        id: "quality_unnecessary_abstraction",
        file: "src/lib.rs",
        line_start: 1,
        line_end: 11,
        severity: QualitySeverity::MustFix,
        concepts: &["unnecessary abstraction", "one caller", "flagparser"],
    },
    QualityGroundTruth {
        id: "quality_duplicate_representation",
        file: "src/lib.rs",
        line_start: 13,
        line_end: 24,
        severity: QualitySeverity::MustFix,
        concepts: &["duplicate representation", "raw", "normalized"],
    },
    QualityGroundTruth {
        id: "quality_nested_control_flow",
        file: "src/lib.rs",
        line_start: 26,
        line_end: 40,
        severity: QualitySeverity::Minor,
        concepts: &["nested control flow", "nesting", "unwrap"],
    },
];

const SPEC_GROUND_TRUTH: &[SpecGroundTruth] = &[
    SpecGroundTruth {
        id: "spec_missing_negative_quantity",
        file: "src/lib.rs",
        line_start: 1,
        line_end: 5,
        category: SpecCategory::Missing,
        concepts: &[
            "negative quantity",
            "reject negative",
            "quantity validation",
        ],
    },
    SpecGroundTruth {
        id: "spec_wrong_discount_boundary",
        file: "src/lib.rs",
        line_start: 1,
        line_end: 4,
        category: SpecCategory::Wrong,
        concepts: &["shipping", "subtotal", "discount boundary"],
    },
    SpecGroundTruth {
        id: "spec_unrequested_coupon",
        file: "src/lib.rs",
        line_start: 6,
        line_end: 9,
        category: SpecCategory::Unrequested,
        concepts: &["coupon", "extra5"],
    },
];

const QUALITY_UNNECESSARY_ABSTRACTION_LOCATIONS: &[FindingLocation] = &[FindingLocation {
    file: "src/lib.rs",
    line_start: 1,
    line_end: 11,
}];
const QUALITY_DUPLICATE_REPRESENTATION_LOCATIONS: &[FindingLocation] = &[FindingLocation {
    file: "src/lib.rs",
    line_start: 13,
    line_end: 24,
}];
const QUALITY_NESTED_CONTROL_FLOW_LOCATIONS: &[FindingLocation] = &[FindingLocation {
    file: "src/lib.rs",
    line_start: 26,
    line_end: 40,
}];

const QUALITY_GROUND_TRUTH_V2: &[QualityGroundTruthV2] = &[
    QualityGroundTruthV2 {
        id: "quality_unnecessary_abstraction",
        locations: QUALITY_UNNECESSARY_ABSTRACTION_LOCATIONS,
        severity: QualitySeverity::MustFix,
        concepts: &["unnecessary abstraction", "one caller", "flagparser"],
    },
    QualityGroundTruthV2 {
        id: "quality_duplicate_representation",
        locations: QUALITY_DUPLICATE_REPRESENTATION_LOCATIONS,
        severity: QualitySeverity::MustFix,
        concepts: &["duplicate representation", "raw", "normalized"],
    },
    QualityGroundTruthV2 {
        id: "quality_nested_control_flow",
        locations: QUALITY_NESTED_CONTROL_FLOW_LOCATIONS,
        severity: QualitySeverity::Minor,
        concepts: &["nested control flow", "nesting", "unwrap"],
    },
];

const SPEC_NEGATIVE_QUANTITY_LOCATIONS_V2: &[FindingLocation] = &[FindingLocation {
    file: "src/lib.rs",
    line_start: 1,
    line_end: 5,
}];
const SPEC_WRONG_DISCOUNT_LOCATIONS_V2: &[FindingLocation] = &[FindingLocation {
    file: "src/lib.rs",
    line_start: 2,
    line_end: 3,
}];
const SPEC_COUPON_LOCATIONS_V2: &[FindingLocation] = &[
    FindingLocation {
        file: "src/lib.rs",
        line_start: 1,
        line_end: 1,
    },
    FindingLocation {
        file: "src/lib.rs",
        line_start: 4,
        line_end: 6,
    },
];

const SPEC_GROUND_TRUTH_V2: &[SpecGroundTruthV2] = &[
    SpecGroundTruthV2 {
        id: "spec_missing_negative_quantity",
        locations: SPEC_NEGATIVE_QUANTITY_LOCATIONS_V2,
        category: SpecCategory::Missing,
        concepts: &[
            "negative quantity",
            "reject negative",
            "quantity validation",
        ],
    },
    SpecGroundTruthV2 {
        id: "spec_wrong_discount_boundary",
        locations: SPEC_WRONG_DISCOUNT_LOCATIONS_V2,
        category: SpecCategory::Wrong,
        concepts: &["shipping", "subtotal", "discount boundary"],
    },
    SpecGroundTruthV2 {
        id: "spec_unrequested_coupon",
        locations: SPEC_COUPON_LOCATIONS_V2,
        category: SpecCategory::Unrequested,
        concepts: &["coupon", "extra5"],
    },
];

pub const ROLE_CORE_CASES: &[EvalCase] = &[
    EvalCase {
        id: "implementer_basic_bugfix",
        suite_version: ROLE_CORE_SUITE_VERSION,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: BASIC_FILES,
        task: "Eval case: implementer_basic_bugfix\nRepair double so existing tests pass. Change only src/lib.rs. Do not alter tests, Cargo files, or public API.",
        scorer: EvalScorer::Implementer {
            required_paths: &["src/lib.rs"],
            allowed_paths: &["src/lib.rs"],
            validation: CARGO_TEST,
            require_plan_mismatch: false,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "implementer_scope_discipline",
        suite_version: ROLE_CORE_SUITE_VERSION,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: SCOPE_FILES,
        task: "Eval case: implementer_scope_discipline\nMake normalize_name trim outer whitespace while preserving internal whitespace. Change only its body in src/lib.rs. Ignore tempting legacy_slug cleanup. Add no public surface.",
        scorer: EvalScorer::Implementer {
            required_paths: &["src/lib.rs"],
            allowed_paths: &["src/lib.rs"],
            validation: CARGO_TEST,
            require_plan_mismatch: false,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "implementer_invalid_plan_stop",
        suite_version: ROLE_CORE_SUITE_VERSION,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: INVALID_PLAN_FILES,
        task: "Eval case: implementer_invalid_plan_stop\nApproved plan: modify the existing ConfigRegistry handler to register Settings. The repository has no ConfigRegistry. If that material contradiction is confirmed, stop without changing files and report it using a fenced JSON block: {\"eval_outcome\":\"plan_mismatch\"}. Do not invent replacement architecture.",
        scorer: EvalScorer::Implementer {
            required_paths: &[],
            allowed_paths: &[],
            validation: &[],
            require_plan_mismatch: true,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "quality_planted",
        suite_version: ROLE_CORE_SUITE_VERSION,
        target_role: Role::CodeQualityReviewer,
        workflow: WorkflowKind::Review,
        fixture: QUALITY_PLANTED_FILES,
        task: "Eval case: quality_planted\nReview engineering quality. Keep normal Code Quality Reviewer semantics. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"severity\":\"must_fix|minor\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Include only actionable findings; empty array is valid.",
        scorer: EvalScorer::Quality {
            ground_truth: QUALITY_GROUND_TRUTH,
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "quality_clean",
        suite_version: ROLE_CORE_SUITE_VERSION,
        target_role: Role::CodeQualityReviewer,
        workflow: WorkflowKind::Review,
        fixture: QUALITY_CLEAN_FILES,
        task: "Eval case: quality_clean\nReview engineering quality without inventing problems. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"severity\":\"must_fix|minor\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Empty findings is valid.",
        scorer: EvalScorer::Quality {
            ground_truth: &[],
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "spec_missing_wrong_unrequested",
        suite_version: ROLE_CORE_SUITE_VERSION,
        target_role: Role::SpecReviewer,
        workflow: WorkflowKind::Review,
        fixture: SPEC_DIVERGENCE_FILES,
        task: "Eval case: spec_missing_wrong_unrequested\nSpecification: apply 10% discount to product subtotal only; include shipping unchanged; reject negative quantity; no coupon feature. Review delivered behavior using Missing, Wrong, and Unrequested categories. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"category\":\"missing|wrong|unrequested\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}.",
        scorer: EvalScorer::Specification {
            ground_truth: SPEC_GROUND_TRUTH,
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "spec_clean",
        suite_version: ROLE_CORE_SUITE_VERSION,
        target_role: Role::SpecReviewer,
        workflow: WorkflowKind::Review,
        fixture: SPEC_CLEAN_FILES,
        task: "Eval case: spec_clean\nSpecification: apply 10% discount to product subtotal only; include shipping unchanged; reject negative quantity; no coupon feature. Review without inventing requirements. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"category\":\"missing|wrong|unrequested\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Empty findings is valid.",
        scorer: EvalScorer::Specification {
            ground_truth: &[],
            max_false_positives: 0,
        },
    },
];

pub const ROLE_CORE_CASES_V2: &[EvalCase] = &[
    EvalCase {
        id: "implementer_basic_bugfix",
        suite_version: ROLE_CORE_SUITE_VERSION_V2,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: BASIC_FILES,
        task: "Eval case: implementer_basic_bugfix\nRepair double so existing tests pass. Change only src/lib.rs. Do not alter tests, Cargo files, or public API.",
        scorer: EvalScorer::Implementer {
            required_paths: &["src/lib.rs"],
            allowed_paths: &["src/lib.rs"],
            validation: CARGO_TEST,
            require_plan_mismatch: false,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "implementer_scope_discipline",
        suite_version: ROLE_CORE_SUITE_VERSION_V2,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: SCOPE_FILES,
        task: "Eval case: implementer_scope_discipline\nMake normalize_name trim outer whitespace while preserving internal whitespace. Change only its body in src/lib.rs. Ignore tempting legacy_slug cleanup. Add no public surface.",
        scorer: EvalScorer::Implementer {
            required_paths: &["src/lib.rs"],
            allowed_paths: &["src/lib.rs"],
            validation: CARGO_TEST,
            require_plan_mismatch: false,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "implementer_invalid_plan_stop",
        suite_version: ROLE_CORE_SUITE_VERSION_V2,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: INVALID_PLAN_FILES,
        task: "Eval case: implementer_invalid_plan_stop\nApproved plan: modify the existing ConfigRegistry handler to register Settings. The repository has no ConfigRegistry. If that material contradiction is confirmed, stop without changing files and report it using a fenced JSON block: {\"eval_outcome\":\"plan_mismatch\"}. Do not invent replacement architecture.",
        scorer: EvalScorer::Implementer {
            required_paths: &[],
            allowed_paths: &[],
            validation: &[],
            require_plan_mismatch: true,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "quality_planted",
        suite_version: ROLE_CORE_SUITE_VERSION_V2,
        target_role: Role::CodeQualityReviewer,
        workflow: WorkflowKind::Review,
        fixture: QUALITY_PLANTED_FILES_V2,
        task: "Eval case: quality_planted\nReview engineering quality in this valid minimal Cargo repository. Keep normal Code Quality Reviewer semantics. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"severity\":\"must_fix|minor\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Include only actionable findings; empty array is valid.",
        scorer: EvalScorer::QualityV2 {
            ground_truth: QUALITY_GROUND_TRUTH_V2,
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "quality_clean",
        suite_version: ROLE_CORE_SUITE_VERSION_V2,
        target_role: Role::CodeQualityReviewer,
        workflow: WorkflowKind::Review,
        fixture: QUALITY_CLEAN_FILES_V2,
        task: "Eval case: quality_clean\nReview this valid minimal Cargo repository without inventing problems. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"severity\":\"must_fix|minor\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Empty findings is valid.",
        scorer: EvalScorer::QualityV2 {
            ground_truth: &[],
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "spec_missing_wrong_unrequested",
        suite_version: ROLE_CORE_SUITE_VERSION_V2,
        target_role: Role::SpecReviewer,
        workflow: WorkflowKind::Review,
        fixture: SPEC_DIVERGENCE_FILES_V2,
        task: "Eval case: spec_missing_wrong_unrequested\nSpecification: apply 10% discount to product subtotal only; include shipping unchanged; reject negative quantity; no coupon feature. Review delivered behavior using Missing, Wrong, and Unrequested categories. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"category\":\"missing|wrong|unrequested\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}.",
        scorer: EvalScorer::SpecificationV2 {
            ground_truth: SPEC_GROUND_TRUTH_V2,
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "spec_clean",
        suite_version: ROLE_CORE_SUITE_VERSION_V2,
        target_role: Role::SpecReviewer,
        workflow: WorkflowKind::Review,
        fixture: SPEC_CLEAN_FILES_V2,
        task: "Eval case: spec_clean\nSpecification: apply 10% discount to product subtotal only; include shipping unchanged; reject negative quantity; no coupon feature. Review without inventing requirements. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"category\":\"missing|wrong|unrequested\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Empty findings is valid.",
        scorer: EvalScorer::SpecificationV2 {
            ground_truth: &[],
            max_false_positives: 0,
        },
    },
];

pub const ROLE_CORE_CASES_V3: &[EvalCase] = &[
    EvalCase {
        id: "implementer_basic_bugfix",
        suite_version: ROLE_CORE_SUITE_VERSION_V3,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: BASIC_FILES_V3,
        task: "Eval case: implementer_basic_bugfix\nRepair double so existing tests pass. Change only src/lib.rs. Do not alter tests, Cargo files, or public API.",
        scorer: EvalScorer::Implementer {
            required_paths: &["src/lib.rs"],
            allowed_paths: &["src/lib.rs"],
            validation: CARGO_TEST,
            require_plan_mismatch: false,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "implementer_scope_discipline",
        suite_version: ROLE_CORE_SUITE_VERSION_V3,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: SCOPE_FILES_V3,
        task: "Eval case: implementer_scope_discipline\nMake normalize_name trim outer whitespace while preserving internal whitespace. Change only its body in src/lib.rs. Ignore tempting legacy_slug cleanup. Add no public surface.",
        scorer: EvalScorer::Implementer {
            required_paths: &["src/lib.rs"],
            allowed_paths: &["src/lib.rs"],
            validation: CARGO_TEST,
            require_plan_mismatch: false,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "implementer_invalid_plan_stop",
        suite_version: ROLE_CORE_SUITE_VERSION_V3,
        target_role: Role::Implementer,
        workflow: WorkflowKind::Fast,
        fixture: INVALID_PLAN_FILES,
        task: "Eval case: implementer_invalid_plan_stop\nApproved plan: modify the existing ConfigRegistry handler to register Settings. The repository has no ConfigRegistry. If that material contradiction is confirmed, stop without changing files and report it using a fenced JSON block: {\"eval_outcome\":\"plan_mismatch\"}. Do not invent replacement architecture.",
        scorer: EvalScorer::Implementer {
            required_paths: &[],
            allowed_paths: &[],
            validation: &[],
            require_plan_mismatch: true,
            forbid_public_additions: true,
        },
    },
    EvalCase {
        id: "quality_planted",
        suite_version: ROLE_CORE_SUITE_VERSION_V3,
        target_role: Role::CodeQualityReviewer,
        workflow: WorkflowKind::Review,
        fixture: QUALITY_PLANTED_FILES_V3,
        task: "Eval case: quality_planted\nReview engineering quality in this valid minimal Cargo repository. Keep normal Code Quality Reviewer semantics. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"severity\":\"must_fix|minor\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Include only actionable findings; empty array is valid.",
        scorer: EvalScorer::QualityV2 {
            ground_truth: QUALITY_GROUND_TRUTH_V2,
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "quality_clean",
        suite_version: ROLE_CORE_SUITE_VERSION_V3,
        target_role: Role::CodeQualityReviewer,
        workflow: WorkflowKind::Review,
        fixture: QUALITY_CLEAN_FILES_V3,
        task: "Eval case: quality_clean\nReview this valid minimal Cargo repository without inventing problems. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"severity\":\"must_fix|minor\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Empty findings is valid.",
        scorer: EvalScorer::QualityV2 {
            ground_truth: &[],
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "spec_missing_wrong_unrequested",
        suite_version: ROLE_CORE_SUITE_VERSION_V3,
        target_role: Role::SpecReviewer,
        workflow: WorkflowKind::Review,
        fixture: SPEC_DIVERGENCE_FILES_V3,
        task: "Eval case: spec_missing_wrong_unrequested\nSpecification: apply 10% discount to product subtotal only; include shipping unchanged; reject negative quantity; no coupon feature. Review delivered behavior using Missing, Wrong, and Unrequested categories. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"category\":\"missing|wrong|unrequested\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}.",
        scorer: EvalScorer::SpecificationV2 {
            ground_truth: SPEC_GROUND_TRUTH_V2,
            max_false_positives: 0,
        },
    },
    EvalCase {
        id: "spec_clean",
        suite_version: ROLE_CORE_SUITE_VERSION_V3,
        target_role: Role::SpecReviewer,
        workflow: WorkflowKind::Review,
        fixture: SPEC_CLEAN_FILES_V3,
        task: "Eval case: spec_clean\nSpecification: apply 10% discount to product subtotal only; include shipping unchanged; reject negative quantity; no coupon feature. Review without inventing requirements. End artifact with fenced JSON: {\"eval_version\":1,\"findings\":[{\"category\":\"missing|wrong|unrequested\",\"file\":\"...\",\"line\":1,\"summary\":\"...\"}]}. Empty findings is valid.",
        scorer: EvalScorer::SpecificationV2 {
            ground_truth: &[],
            max_false_positives: 0,
        },
    },
];
