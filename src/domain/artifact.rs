use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ArtifactId, ModelId, ProviderId, Role, RunId, StageId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Research,
    Architecture,
    Implementation,
    Simplification,
    CodeQualityReview,
    SpecReview,
    /// Legacy/general review artifact retained for persisted records.
    Review,
    Decision,
    Fix,
    Synthesis,
    FollowUp,
    /// One turn of a mission lead's conversation.
    Lead,
    /// The Markdown record of one verification pass: every command run,
    /// its exit code, and its bounded output.
    Verify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Complete,
    Skipped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    id: ArtifactId,
    run_id: RunId,
    stage_id: StageId,
    kind: ArtifactKind,
    role: Role,
    status: ArtifactStatus,
    provider_id: Option<ProviderId>,
    model_id: Option<ModelId>,
    created_at: DateTime<Utc>,
    base_commit: Option<String>,
}

impl ArtifactMetadata {
    #[must_use]
    pub const fn new(
        id: ArtifactId,
        run_id: RunId,
        stage_id: StageId,
        kind: ArtifactKind,
        role: Role,
        status: ArtifactStatus,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            run_id,
            stage_id,
            kind,
            role,
            status,
            provider_id: None,
            model_id: None,
            created_at,
            base_commit: None,
        }
    }

    #[must_use]
    pub fn with_provider(mut self, provider_id: ProviderId, model_id: Option<ModelId>) -> Self {
        self.provider_id = Some(provider_id);
        self.model_id = model_id;
        self
    }

    #[must_use]
    pub fn with_base_commit(mut self, base_commit: impl Into<String>) -> Self {
        self.base_commit = Some(base_commit.into());
        self
    }

    #[must_use]
    pub const fn id(&self) -> ArtifactId {
        self.id
    }

    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub const fn stage_id(&self) -> &StageId {
        &self.stage_id
    }

    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        self.kind
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub const fn status(&self) -> ArtifactStatus {
        self.status
    }

    #[must_use]
    pub const fn provider_id(&self) -> Option<&ProviderId> {
        self.provider_id.as_ref()
    }

    #[must_use]
    pub const fn model_id(&self) -> Option<&ModelId> {
        self.model_id.as_ref()
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    #[must_use]
    pub fn base_commit(&self) -> Option<&str> {
        self.base_commit.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn unresolved_provider_metadata_remains_optional_and_serializable() {
        let metadata = ArtifactMetadata::new(
            ArtifactId::from_u128(1),
            RunId::from_u128(2),
            StageId::new("architecture").unwrap(),
            ArtifactKind::Architecture,
            Role::Architect,
            ArtifactStatus::Complete,
            Utc.with_ymd_and_hms(2026, 8, 14, 8, 0, 0).single().unwrap(),
        );
        let encoded = serde_json::to_string(&metadata).unwrap();
        let decoded: ArtifactMetadata = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, metadata);
        assert_eq!(decoded.provider_id(), None);
        assert_eq!(decoded.model_id(), None);
    }

    #[test]
    fn specialized_and_legacy_review_artifact_kinds_are_stable() {
        assert_eq!(
            serde_json::to_string(&ArtifactKind::CodeQualityReview).unwrap(),
            "\"code_quality_review\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::SpecReview).unwrap(),
            "\"spec_review\""
        );
        assert_eq!(
            serde_json::from_str::<ArtifactKind>("\"review\"").unwrap(),
            ArtifactKind::Review
        );
    }

    /// Additive like `FollowUp` before it: the new variant round-trips
    /// through the same inspectable snake-case shape.
    #[test]
    fn verify_artifact_kind_round_trips_through_snake_case() {
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Verify).unwrap(),
            "\"verify\""
        );
        assert_eq!(
            serde_json::from_str::<ArtifactKind>("\"verify\"").unwrap(),
            ArtifactKind::Verify
        );
    }

    /// Additive like `Fix` before it: the new variant round-trips through
    /// the same inspectable snake-case shape.
    #[test]
    fn lead_artifact_kind_round_trips_through_snake_case() {
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Lead).unwrap(),
            "\"lead\""
        );
        assert_eq!(
            serde_json::from_str::<ArtifactKind>("\"lead\"").unwrap(),
            ArtifactKind::Lead
        );
    }

    #[test]
    fn follow_up_artifact_kind_round_trips_through_snake_case() {
        assert_eq!(
            serde_json::to_string(&ArtifactKind::FollowUp).unwrap(),
            "\"follow_up\""
        );
        assert_eq!(
            serde_json::from_str::<ArtifactKind>("\"follow_up\"").unwrap(),
            ArtifactKind::FollowUp
        );
    }
}
