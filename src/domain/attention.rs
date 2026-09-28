use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{AttentionRequestId, RunId, StageId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Permission,
    Decision,
    Question,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "at", rename_all = "snake_case")]
pub enum AttentionStatus {
    Pending,
    Resolved(DateTime<Utc>),
    Cancelled(DateTime<Utc>),
}

impl AttentionStatus {
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AttentionRequestSnapshot")]
pub struct AttentionRequest {
    id: AttentionRequestId,
    run_id: RunId,
    stage_id: StageId,
    kind: AttentionKind,
    summary: String,
    status: AttentionStatus,
    created_at: DateTime<Utc>,
}

impl AttentionRequest {
    /// Creates one unresolved human-attention request.
    ///
    /// # Errors
    /// Returns [`AttentionError::EmptySummary`] for an empty message.
    pub fn new(
        id: AttentionRequestId,
        run_id: RunId,
        stage_id: StageId,
        kind: AttentionKind,
        summary: impl Into<String>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, AttentionError> {
        let summary = summary.into();
        if summary.trim().is_empty() {
            return Err(AttentionError::EmptySummary);
        }
        Ok(Self {
            id,
            run_id,
            stage_id,
            kind,
            summary,
            status: AttentionStatus::Pending,
            created_at,
        })
    }

    #[must_use]
    pub const fn id(&self) -> AttentionRequestId {
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
    pub const fn kind(&self) -> AttentionKind {
        self.kind
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub const fn status(&self) -> &AttentionStatus {
        &self.status
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    pub(crate) fn resolve(&mut self, at: DateTime<Utc>) -> Result<(), AttentionError> {
        if !self.status.is_pending() {
            return Err(AttentionError::AlreadyClosed(self.id));
        }
        self.validate_closure_time(at)?;
        self.status = AttentionStatus::Resolved(at);
        Ok(())
    }

    pub(crate) fn cancel(&mut self, at: DateTime<Utc>) -> Result<(), AttentionError> {
        if !self.status.is_pending() {
            return Err(AttentionError::AlreadyClosed(self.id));
        }
        self.validate_closure_time(at)?;
        self.status = AttentionStatus::Cancelled(at);
        Ok(())
    }

    fn validate_closure_time(&self, at: DateTime<Utc>) -> Result<(), AttentionError> {
        if at < self.created_at {
            return Err(AttentionError::ClosedBeforeCreation {
                id: self.id,
                created_at: self.created_at,
                closed_at: at,
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct AttentionRequestSnapshot {
    id: AttentionRequestId,
    run_id: RunId,
    stage_id: StageId,
    kind: AttentionKind,
    summary: String,
    status: AttentionStatus,
    created_at: DateTime<Utc>,
}

impl TryFrom<AttentionRequestSnapshot> for AttentionRequest {
    type Error = AttentionError;

    fn try_from(snapshot: AttentionRequestSnapshot) -> Result<Self, Self::Error> {
        let mut request = Self::new(
            snapshot.id,
            snapshot.run_id,
            snapshot.stage_id,
            snapshot.kind,
            snapshot.summary,
            snapshot.created_at,
        )?;
        match snapshot.status {
            AttentionStatus::Pending => {}
            AttentionStatus::Resolved(at) => request.resolve(at)?,
            AttentionStatus::Cancelled(at) => request.cancel(at)?,
        }
        Ok(request)
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AttentionError {
    #[error("attention summary must not be empty")]
    EmptySummary,
    #[error("attention request {0} is already resolved or cancelled")]
    AlreadyClosed(AttentionRequestId),
    #[error("attention request {id} cannot close at {closed_at} before creation at {created_at}")]
    ClosedBeforeCreation {
        id: AttentionRequestId,
        created_at: DateTime<Utc>,
        closed_at: DateTime<Utc>,
    },
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 14, 8, 0, second)
            .single()
            .unwrap()
    }

    #[test]
    fn request_creation_resolution_and_double_resolution_are_explicit() {
        let mut request = AttentionRequest::new(
            AttentionRequestId::from_u128(1),
            RunId::from_u128(2),
            StageId::new("implementation").unwrap(),
            AttentionKind::Permission,
            "Run database reset",
            at(0),
        )
        .unwrap();

        request.resolve(at(1)).unwrap();
        assert_eq!(request.status(), &AttentionStatus::Resolved(at(1)));
        assert_eq!(
            request.resolve(at(2)),
            Err(AttentionError::AlreadyClosed(request.id()))
        );
    }

    #[test]
    fn attention_request_round_trips_through_json() {
        let request = AttentionRequest::new(
            AttentionRequestId::from_u128(3),
            RunId::from_u128(4),
            StageId::new("decision").unwrap(),
            AttentionKind::Decision,
            "Choose migration strategy",
            at(0),
        )
        .unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: AttentionRequest = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn deserialization_cannot_bypass_attention_invariants() {
        let request = AttentionRequest::new(
            AttentionRequestId::from_u128(5),
            RunId::from_u128(6),
            StageId::new("review").unwrap(),
            AttentionKind::Question,
            "Need evidence",
            at(1),
        )
        .unwrap();
        let mut value = serde_json::to_value(request).unwrap();
        value["summary"] = serde_json::Value::String("   ".to_owned());
        assert!(serde_json::from_value::<AttentionRequest>(value).is_err());

        let mut request = AttentionRequest::new(
            AttentionRequestId::from_u128(7),
            RunId::from_u128(8),
            StageId::new("review").unwrap(),
            AttentionKind::Question,
            "Need evidence",
            at(1),
        )
        .unwrap();
        assert!(matches!(
            request.resolve(at(0)),
            Err(AttentionError::ClosedBeforeCreation { .. })
        ));
    }
}
