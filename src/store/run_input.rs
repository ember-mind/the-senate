use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::domain::RunId;

pub const RUN_INPUT_SCHEMA_VERSION: u32 = 1;

/// Immutable user intent bound to one run, separate from lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunInput {
    run_id: RunId,
    schema_version: u32,
    task: String,
    created_at: DateTime<Utc>,
}

impl RunInput {
    /// Normalizes one task while preserving Unicode, line breaks, and content.
    ///
    /// # Errors
    /// Rejects tasks containing only whitespace.
    pub fn new(
        run_id: RunId,
        task: impl Into<String>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, RunInputError> {
        let task = task.into().trim().to_owned();
        if task.is_empty() {
            return Err(RunInputError::EmptyTask);
        }
        Ok(Self {
            run_id,
            schema_version: RUN_INPUT_SCHEMA_VERSION,
            task,
            created_at,
        })
    }

    pub(crate) fn from_stored(
        run_id: RunId,
        schema_version: u32,
        task: String,
        created_at: DateTime<Utc>,
    ) -> Result<Self, RunInputError> {
        if schema_version != RUN_INPUT_SCHEMA_VERSION {
            return Err(RunInputError::UnsupportedSchemaVersion(schema_version));
        }
        Self::new(run_id, task, created_at)
    }

    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    #[must_use]
    pub fn task(&self) -> &str {
        &self.task
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RunInputError {
    #[error("run task must not be empty")]
    EmptyTask,
    #[error("run input schema version {0} is unsupported")]
    UnsupportedSchemaVersion(u32),
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn normalizes_outer_whitespace_without_changing_content() {
        let at = Utc
            .with_ymd_and_hms(2026, 8, 14, 10, 0, 0)
            .single()
            .unwrap();
        let input = RunInput::new(RunId::from_u128(1), "  α\nβ  ", at).unwrap();

        assert_eq!(input.task(), "α\nβ");
    }

    #[test]
    fn rejects_whitespace_only_task() {
        let at = Utc
            .with_ymd_and_hms(2026, 8, 14, 10, 0, 0)
            .single()
            .unwrap();
        assert_eq!(
            RunInput::new(RunId::from_u128(1), " \n\t ", at).unwrap_err(),
            RunInputError::EmptyTask
        );
    }
}
