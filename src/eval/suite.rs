use std::collections::HashSet;
use std::path::{Component, Path};

use sha2::{Digest, Sha256};
use thiserror::Error;

use super::case::{
    EvalCase, ROLE_CORE_CASES, ROLE_CORE_CASES_V2, ROLE_CORE_CASES_V3, ROLE_CORE_SUITE_VERSION,
    ROLE_CORE_SUITE_VERSION_V2, ROLE_CORE_SUITE_VERSION_V3,
};

pub const ROLE_CORE_SUITE_NAME: &str = "role_core";

#[derive(Clone, Debug)]
pub struct EvalSuite {
    name: &'static str,
    version: &'static str,
    cases: Vec<EvalCase>,
    fingerprint: String,
}

impl EvalSuite {
    /// Creates a suite after validating stable identities and safe fixture paths.
    ///
    /// # Errors
    /// Rejects invalid identity, duplicate cases, mismatched versions, or unsafe fixture paths.
    pub fn new(
        name: &'static str,
        version: &'static str,
        cases: Vec<EvalCase>,
    ) -> Result<Self, EvalSuiteError> {
        if name.trim().is_empty() || version.trim().is_empty() || cases.is_empty() {
            return Err(EvalSuiteError::InvalidIdentity);
        }
        let mut ids = HashSet::new();
        for case in &cases {
            if case.suite_version != version || !valid_id(case.id) {
                return Err(EvalSuiteError::InvalidCase(case.id.to_owned()));
            }
            if !ids.insert(case.id) {
                return Err(EvalSuiteError::DuplicateCase(case.id.to_owned()));
            }
            let mut paths = HashSet::new();
            for file in case.fixture {
                let path = Path::new(file.path);
                if path.is_absolute()
                    || path.components().any(|part| {
                        matches!(
                            part,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    })
                    || !paths.insert(file.path)
                {
                    return Err(EvalSuiteError::InvalidFixturePath {
                        case_id: case.id.to_owned(),
                        path: file.path.to_owned(),
                    });
                }
            }
        }
        let fingerprint = suite_fingerprint(name, version, &cases);
        Ok(Self {
            name,
            version,
            cases,
            fingerprint,
        })
    }

    /// Loads one source-controlled suite version.
    ///
    /// # Errors
    /// Rejects unknown suite versions or invalid source-controlled definitions.
    pub fn load(version: &str) -> Result<Self, EvalSuiteError> {
        match version {
            ROLE_CORE_SUITE_VERSION => Self::new(
                ROLE_CORE_SUITE_NAME,
                ROLE_CORE_SUITE_VERSION,
                ROLE_CORE_CASES.to_vec(),
            ),
            ROLE_CORE_SUITE_VERSION_V2 => Self::new(
                ROLE_CORE_SUITE_NAME,
                ROLE_CORE_SUITE_VERSION_V2,
                ROLE_CORE_CASES_V2.to_vec(),
            ),
            ROLE_CORE_SUITE_VERSION_V3 => Self::new(
                ROLE_CORE_SUITE_NAME,
                ROLE_CORE_SUITE_VERSION_V3,
                ROLE_CORE_CASES_V3.to_vec(),
            ),
            other => Err(EvalSuiteError::UnknownSuite(other.to_owned())),
        }
    }

    #[must_use]
    pub const fn name(&self) -> &str {
        self.name
    }

    #[must_use]
    pub const fn version(&self) -> &str {
        self.version
    }

    #[must_use]
    pub fn cases(&self) -> &[EvalCase] {
        &self.cases
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    #[must_use]
    pub fn fixture_fingerprint(case: &EvalCase) -> String {
        let mut hasher = Sha256::new();
        hash_piece(&mut hasher, case.id.as_bytes());
        hash_piece(&mut hasher, case.suite_version.as_bytes());
        hash_piece(&mut hasher, case.task.as_bytes());
        hash_piece(&mut hasher, format!("{:?}", case.target_role).as_bytes());
        hash_piece(&mut hasher, format!("{:?}", case.workflow).as_bytes());
        hash_piece(&mut hasher, format!("{:?}", case.scorer).as_bytes());
        for file in case.fixture {
            hash_piece(&mut hasher, file.path.as_bytes());
            hash_piece(&mut hasher, file.contents.as_bytes());
        }
        hex(hasher.finalize().as_ref())
    }
}

fn suite_fingerprint(name: &str, version: &str, cases: &[EvalCase]) -> String {
    let mut hasher = Sha256::new();
    hash_piece(&mut hasher, name.as_bytes());
    hash_piece(&mut hasher, version.as_bytes());
    for case in cases {
        hash_piece(&mut hasher, EvalSuite::fixture_fingerprint(case).as_bytes());
    }
    hex(hasher.finalize().as_ref())
}

fn hash_piece(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(
        u64::try_from(bytes.len())
            .expect("fixture length fits u64")
            .to_be_bytes(),
    );
    hasher.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EvalSuiteError {
    #[error("unknown eval suite {0:?}")]
    UnknownSuite(String),
    #[error("eval suite identity or case list is invalid")]
    InvalidIdentity,
    #[error("invalid eval case {0:?}")]
    InvalidCase(String),
    #[error("duplicate eval case {0:?}")]
    DuplicateCase(String),
    #[error("eval case {case_id:?} has unsafe or duplicate fixture path {path:?}")]
    InvalidFixturePath { case_id: String, path: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_core_loads_with_stable_unique_case_ids_and_fixtures() {
        let suite = EvalSuite::load(ROLE_CORE_SUITE_VERSION).unwrap();
        assert_eq!(suite.cases().len(), 7);
        assert_eq!(suite.fingerprint().len(), 64);
        assert_eq!(
            suite.fingerprint(),
            "40d035a14aa5c5e8adaa41bcc3dbe7cb927fd0d47e122808f5a1a4b9ff6f843d"
        );
        assert_eq!(suite.cases()[0].id, "implementer_basic_bugfix");
        assert!(suite.cases().iter().all(|case| !case.fixture.is_empty()));
    }

    #[test]
    fn role_core_v2_loads_separately_with_same_case_ids() {
        let v1 = EvalSuite::load(ROLE_CORE_SUITE_VERSION).unwrap();
        let v2 = EvalSuite::load(ROLE_CORE_SUITE_VERSION_V2).unwrap();
        assert_eq!(v2.cases().len(), 7);
        assert_ne!(v2.fingerprint(), v1.fingerprint());
        assert_eq!(
            v1.cases().iter().map(|case| case.id).collect::<Vec<_>>(),
            v2.cases().iter().map(|case| case.id).collect::<Vec<_>>()
        );
        assert!(
            v2.cases()
                .iter()
                .all(|case| case.suite_version == ROLE_CORE_SUITE_VERSION_V2)
        );
        assert_eq!(
            v1.cases()[3]
                .fixture
                .iter()
                .map(|file| file.path)
                .collect::<Vec<_>>(),
            vec!["src/lib.rs"]
        );
        assert!(
            v2.cases()[3]
                .fixture
                .iter()
                .any(|file| file.path == "Cargo.toml")
        );
        assert_eq!(
            v2.fingerprint(),
            "0215fc2979fbd8c09947ffb8c43f703cd7132b595b3516a10f37eacc51c6710b"
        );
    }

    #[test]
    fn role_core_v3_loads_with_same_case_ids_and_distinct_hygienic_fixtures() {
        let v2 = EvalSuite::load(ROLE_CORE_SUITE_VERSION_V2).unwrap();
        let v3 = EvalSuite::load(ROLE_CORE_SUITE_VERSION_V3).unwrap();
        assert_eq!(v3.cases().len(), 7);
        assert_ne!(v3.fingerprint(), v2.fingerprint());
        assert_eq!(
            v3.fingerprint(),
            "cb9856d2c8edbc4cb0a59520aa140ef4567dce3b650b14f0436d42c4b11c375b"
        );
        assert_eq!(
            v2.cases().iter().map(|case| case.id).collect::<Vec<_>>(),
            v3.cases().iter().map(|case| case.id).collect::<Vec<_>>()
        );
        assert!(
            v3.cases()
                .iter()
                .all(|case| case.suite_version == ROLE_CORE_SUITE_VERSION_V3)
        );
        assert!(
            v3.cases()[0]
                .fixture
                .iter()
                .all(|file| file.path != "Cargo.lock")
        );
        assert!(
            v3.cases()[3]
                .fixture
                .iter()
                .any(|file| file.path == "Cargo.lock")
        );
    }

    #[test]
    fn duplicate_case_is_rejected() {
        let case = ROLE_CORE_CASES[0];
        assert!(matches!(
            EvalSuite::new("duplicate", ROLE_CORE_SUITE_VERSION, vec![case, case]),
            Err(EvalSuiteError::DuplicateCase(_))
        ));
    }
}
