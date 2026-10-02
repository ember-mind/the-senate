//! Persistence for a mission's restartable drive policy and last pause.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension as _, params};

use crate::domain::MissionId;

use super::sqlite::format_timestamp;
use super::{SqliteStore, StoreError};

/// Persisted driver policy. The validated policy JSON is owned by the
/// application layer; `max_parallel` is duplicated as an indexed projection
/// so package starts can enforce the limit in their binding transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissionDriveRecord {
    pub max_parallel: usize,
    pub policy_json: String,
    pub pause_reason: Option<String>,
}

impl SqliteStore {
    /// Reads a mission's persisted drive policy and its last pause reason.
    ///
    /// # Errors
    /// Returns malformed projection or `SQLite` errors.
    pub fn mission_drive(
        &self,
        mission_id: MissionId,
    ) -> Result<Option<MissionDriveRecord>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT max_parallel, policy_json, pause_reason
                 FROM mission_drives WHERE mission_id = ?1",
                [mission_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(max_parallel, policy_json, pause_reason)| {
            Ok(MissionDriveRecord {
                max_parallel: usize::try_from(max_parallel).map_err(|_| {
                    StoreError::SnapshotProjectionMismatch("mission drive max_parallel")
                })?,
                policy_json,
                pause_reason,
            })
        })
        .transpose()
    }

    /// Creates or replaces a mission's policy and clears its prior pause.
    ///
    /// # Errors
    /// Returns projection-range or `SQLite` errors.
    pub fn save_mission_drive(
        &mut self,
        mission_id: MissionId,
        max_parallel: usize,
        policy_json: &str,
        updated_at: &DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let max_parallel = i64::try_from(max_parallel)
            .map_err(|_| StoreError::SnapshotProjectionMismatch("mission drive max_parallel"))?;
        self.connection.execute(
            "INSERT INTO mission_drives (
                 mission_id, max_parallel, policy_json, pause_reason, updated_at
             ) VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(mission_id) DO UPDATE SET
                 max_parallel = excluded.max_parallel,
                 policy_json = excluded.policy_json,
                 pause_reason = NULL,
                 updated_at = excluded.updated_at",
            params![
                mission_id.to_string(),
                max_parallel,
                policy_json,
                format_timestamp(updated_at),
            ],
        )?;
        Ok(())
    }

    /// Records or clears why the mission driver stopped dispatching.
    ///
    /// A policy row is created before the first drive tick, so updating no
    /// row indicates a caller invariant violation. The mission ID is used in
    /// the existing not-found error to keep this store API dependency-free.
    ///
    /// # Errors
    /// Returns a not-found or `SQLite` error.
    pub fn set_mission_drive_pause(
        &mut self,
        mission_id: MissionId,
        pause_reason: Option<&str>,
        updated_at: &DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE mission_drives SET pause_reason = ?1, updated_at = ?2
             WHERE mission_id = ?3",
            params![
                pause_reason,
                format_timestamp(updated_at),
                mission_id.to_string(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::SnapshotProjectionMismatch(
                "mission drive policy is missing",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{Mission, MissionId};

    use super::*;
    use crate::store::MissionInput;

    fn store_with_mission() -> (SqliteStore, MissionId) {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mission_id = MissionId::new();
        let at = std::time::SystemTime::now().into();
        let mission = Mission::new(mission_id, at);
        let events = mission.created_events();
        let input =
            MissionInput::new(mission_id, "A mission", "A goal", "/repo", "abc123", at).unwrap();
        store.create_mission(&mission, &input, &events).unwrap();
        (store, mission_id)
    }

    #[test]
    fn mission_drive_policy_and_pause_round_trip() {
        let (mut store, mission_id) = store_with_mission();
        let at = std::time::SystemTime::now().into();
        let policy_json = r#"{"schema_version":1,"max_parallel":3,"selection":{"Uniform":"fake"},"effort":"ProfileDefault"}"#;

        store
            .save_mission_drive(mission_id, 3, policy_json, &at)
            .unwrap();
        let saved = store.mission_drive(mission_id).unwrap().unwrap();
        assert_eq!(saved.max_parallel, 3);
        assert_eq!(saved.policy_json, policy_json);
        assert_eq!(saved.pause_reason, None);

        store
            .set_mission_drive_pause(
                mission_id,
                Some("checkout is dirty"),
                &std::time::SystemTime::now().into(),
            )
            .unwrap();
        assert_eq!(
            store
                .mission_drive(mission_id)
                .unwrap()
                .unwrap()
                .pause_reason
                .as_deref(),
            Some("checkout is dirty")
        );

        store
            .save_mission_drive(
                mission_id,
                2,
                policy_json,
                &std::time::SystemTime::now().into(),
            )
            .unwrap();
        let updated = store.mission_drive(mission_id).unwrap().unwrap();
        assert_eq!(updated.max_parallel, 2);
        assert_eq!(updated.pause_reason, None);
    }
}
