//! `SQLite` persistence boundary for validated domain aggregates.

mod config_snapshot;
mod error;
mod image;
mod migrations;
mod mission;
mod path;
mod process;
mod provider;
mod run_input;
mod snapshot;
mod sqlite;
mod workspace;

pub use config_snapshot::ResolvedConfigSnapshot;
pub use error::StoreError;
pub use image::ImageGenerationRecord;
pub use migrations::DATABASE_SCHEMA_VERSION;
pub use mission::{
    LoadedMission, MISSION_INPUT_SCHEMA_VERSION, MISSION_SNAPSHOT_SCHEMA_VERSION,
    MissionHandoffRecord, MissionInput, MissionInputError, MissionLeadBinding, MissionRevision,
    MissionRunBinding, MissionSummary, SequencedMissionEvent, contract_sha256, sha256_hex,
};
pub use path::{
    database_file, install_receipt_file, process_root, update_cache_file, worktree_root,
    world_state_file,
};
pub use run_input::{RUN_INPUT_SCHEMA_VERSION, RunInput, RunInputError};
pub use snapshot::RUN_SNAPSHOT_SCHEMA_VERSION;
pub use sqlite::{CommitResult, LoadedRun, RunRevision, RunSummary, SequencedEvent, SqliteStore};
