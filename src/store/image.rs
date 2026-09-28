//! Insert-only evidence for the image-generation tool.

use chrono::{DateTime, Utc};
use rusqlite::{TransactionBehavior, params};

use crate::domain::{RunId, StageId};

use super::sqlite::{format_timestamp, i64_to_u64, parse_timestamp, u64_to_i64};
use super::{SqliteStore, StoreError};

/// One image the tool produced for one run. The PNG itself is an ordinary
/// worktree file; this row is the local answer to who asked, in which stage,
/// which backend/model answered, where it went, and what bytes resulted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageGenerationRecord {
    pub id: String,
    pub run_id: RunId,
    pub stage_id: StageId,
    pub attempt: u32,
    /// 1-based position among this run's generations; the bound counts these.
    pub ordinal: u32,
    pub backend: String,
    pub model: String,
    /// Worktree-relative path of the written PNG.
    pub output_path: String,
    pub output_sha256: String,
    pub output_size: u64,
    /// SHA-256 of the exact prompt bytes; the prompt text stays in the
    /// run-private evidence file, never in the database.
    pub prompt_sha256: String,
    pub response_id: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

impl SqliteStore {
    /// Records one completed generation. Fails if the run's ordinal is taken,
    /// which is how two concurrent writers cannot both claim the same slot.
    ///
    /// # Errors
    /// Returns `SQLite` failures, including the uniqueness violation.
    pub fn insert_image_generation(
        &mut self,
        record: &ImageGenerationRecord,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO image_generations (
                id, run_id, stage_id, attempt, ordinal, backend, model, output_path,
                output_sha256, output_size, prompt_sha256, response_id,
                requested_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.id,
                record.run_id.to_string(),
                record.stage_id.as_str(),
                i64::from(record.attempt),
                i64::from(record.ordinal),
                record.backend,
                record.model,
                record.output_path,
                record.output_sha256,
                u64_to_i64(record.output_size, "output_size")?,
                record.prompt_sha256,
                record.response_id,
                format_timestamp(&record.requested_at),
                format_timestamp(&record.completed_at),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Every generation of one run in ordinal order.
    ///
    /// # Errors
    /// Returns `SQLite` or projection failures.
    pub fn list_image_generations(
        &self,
        run_id: RunId,
    ) -> Result<Vec<ImageGenerationRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, stage_id, attempt, ordinal, backend, model, output_path,
                    output_sha256, output_size, prompt_sha256, response_id,
                    requested_at, completed_at
             FROM image_generations WHERE run_id = ?1 ORDER BY ordinal",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (
                id,
                stage_id,
                attempt,
                ordinal,
                backend,
                model,
                output_path,
                output_sha256,
                output_size,
                prompt_sha256,
                response_id,
                requested_at,
                completed_at,
            ) = row?;
            records.push(ImageGenerationRecord {
                id,
                run_id,
                stage_id: StageId::new(stage_id)
                    .map_err(|error| StoreError::InvalidImageGeneration(error.to_string()))?,
                attempt: u32::try_from(attempt)
                    .map_err(|_| StoreError::InvalidImageGeneration("attempt".to_owned()))?,
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| StoreError::InvalidImageGeneration("ordinal".to_owned()))?,
                backend,
                model,
                output_path,
                output_sha256,
                output_size: i64_to_u64(output_size, "output_size")?,
                prompt_sha256,
                response_id,
                requested_at: parse_timestamp(&requested_at)?,
                completed_at: parse_timestamp(&completed_at)?,
            });
        }
        Ok(records)
    }

    /// How many generations one run has recorded.
    ///
    /// # Errors
    /// Returns `SQLite` failures.
    pub fn count_image_generations(&self, run_id: RunId) -> Result<u32, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM image_generations WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        u32::try_from(count).map_err(|_| StoreError::InvalidImageGeneration("count".to_owned()))
    }
}
