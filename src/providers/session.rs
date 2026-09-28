use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::domain::{AttentionRequestId, ModelId, ProviderId, ProviderSessionId, RunId, StageId};
use crate::process::ManagedProcessId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderSessionRecordId(Ulid);

impl ProviderSessionRecordId {
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(Ulid::from(value))
    }
}

impl Default for ProviderSessionRecordId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProviderSessionRecordId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ProviderSessionRecordId {
    type Err = ulid::DecodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderSessionRevision(u64);

impl ProviderSessionRevision {
    #[must_use]
    pub const fn initial() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSessionStatus {
    Created,
    Starting,
    Active,
    NeedsUser,
    Completed,
    Failed,
    Interrupted,
}

impl ProviderSessionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::NeedsUser => "needs_user",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self, &'static str> {
        match value {
            "created" => Ok(Self::Created),
            "starting" => Ok(Self::Starting),
            "active" => Ok(Self::Active),
            "needs_user" => Ok(Self::NeedsUser),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            _ => Err("unknown provider session status"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingProviderAttention {
    attention_id: AttentionRequestId,
    process_id: ManagedProcessId,
    record_start: u64,
    record_end: u64,
}

impl PendingProviderAttention {
    pub(crate) fn new(
        attention_id: AttentionRequestId,
        process_id: ManagedProcessId,
        record_start: u64,
        record_end: u64,
    ) -> Result<Self, &'static str> {
        if record_end <= record_start {
            return Err("provider attention output range is empty");
        }
        Ok(Self {
            attention_id,
            process_id,
            record_start,
            record_end,
        })
    }

    #[must_use]
    pub const fn attention_id(&self) -> AttentionRequestId {
        self.attention_id
    }

    #[must_use]
    pub const fn process_id(&self) -> ManagedProcessId {
        self.process_id
    }

    #[must_use]
    pub const fn record_start(&self) -> u64 {
        self.record_start
    }

    #[must_use]
    pub const fn record_end(&self) -> u64 {
        self.record_end
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionRecord {
    id: ProviderSessionRecordId,
    run_id: RunId,
    stage_id: StageId,
    attempt: u32,
    provider_id: ProviderId,
    native_session_id: Option<ProviderSessionId>,
    current_process_id: Option<ManagedProcessId>,
    status: ProviderSessionStatus,
    protocol_version: u32,
    invocation: u32,
    model_id: Option<ModelId>,
    cli_version: Option<String>,
    pending_attention: Option<PendingProviderAttention>,
    revision: ProviderSessionRevision,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl ProviderSessionRecord {
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "constructor captures complete immutable provider-session identity"
    )]
    pub fn new(
        id: ProviderSessionRecordId,
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        provider_id: ProviderId,
        protocol_version: u32,
        cli_version: Option<String>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            run_id,
            stage_id,
            attempt,
            provider_id,
            native_session_id: None,
            current_process_id: None,
            status: ProviderSessionStatus::Created,
            protocol_version,
            invocation: 0,
            model_id: None,
            cli_version,
            pending_attention: None,
            revision: ProviderSessionRevision::initial(),
            created_at: now,
            updated_at: now,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored(
        id: ProviderSessionRecordId,
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        provider_id: ProviderId,
        native_session_id: Option<ProviderSessionId>,
        current_process_id: Option<ManagedProcessId>,
        status: ProviderSessionStatus,
        protocol_version: u32,
        invocation: u32,
        model_id: Option<ModelId>,
        cli_version: Option<String>,
        pending_attention: Option<PendingProviderAttention>,
        revision: ProviderSessionRevision,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, &'static str> {
        let record = Self {
            id,
            run_id,
            stage_id,
            attempt,
            provider_id,
            native_session_id,
            current_process_id,
            status,
            protocol_version,
            invocation,
            model_id,
            cli_version,
            pending_attention,
            revision,
            created_at,
            updated_at,
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.attempt == 0 || self.protocol_version == 0 || self.updated_at < self.created_at {
            return Err("invalid provider session identity or timeline");
        }
        if self.invocation == 0 && self.current_process_id.is_some() {
            return Err("provider session process has no invocation");
        }
        if self.status == ProviderSessionStatus::NeedsUser && self.pending_attention.is_none() {
            return Err("provider session needs user without pending attention");
        }
        if self.status != ProviderSessionStatus::NeedsUser && self.pending_attention.is_some() {
            return Err("provider session has attention outside needs-user status");
        }
        Ok(())
    }

    pub(crate) fn bind_process(
        &mut self,
        process_id: ManagedProcessId,
        invocation: u32,
        now: DateTime<Utc>,
    ) -> Result<(), &'static str> {
        if invocation == 0 || invocation <= self.invocation {
            return Err("provider invocation must advance");
        }
        if matches!(
            self.status,
            ProviderSessionStatus::Completed | ProviderSessionStatus::Failed
        ) {
            return Err("finished provider session cannot start another process");
        }
        self.current_process_id = Some(process_id);
        self.invocation = invocation;
        self.status = ProviderSessionStatus::Starting;
        self.pending_attention = None;
        self.updated_at = now.max(self.updated_at);
        self.validate()
    }

    pub(crate) fn activate(
        &mut self,
        native_session_id: ProviderSessionId,
        model_id: Option<ModelId>,
        now: DateTime<Utc>,
    ) -> Result<(), &'static str> {
        if let Some(previous) = &self.native_session_id
            && previous != &native_session_id
        {
            return Err("provider native session identity changed");
        }
        self.native_session_id = Some(native_session_id);
        self.model_id = model_id.or_else(|| self.model_id.clone());
        self.status = ProviderSessionStatus::Active;
        self.updated_at = now.max(self.updated_at);
        self.validate()
    }

    /// Records the model the runtime's own evidence says it ran.
    ///
    /// Only fills an identity that was never observed. A session that already
    /// confirmed a different model keeps it, so a disagreement stays a
    /// disagreement instead of being quietly overwritten by whichever
    /// observation arrived last.
    pub(crate) fn confirm_model(
        &mut self,
        model_id: ModelId,
        now: DateTime<Utc>,
    ) -> Result<(), &'static str> {
        if self.model_id.is_some() {
            return Ok(());
        }
        self.model_id = Some(model_id);
        self.updated_at = now.max(self.updated_at);
        self.validate()
    }

    pub(crate) fn need_user(
        &mut self,
        pending: PendingProviderAttention,
        now: DateTime<Utc>,
    ) -> Result<(), &'static str> {
        self.status = ProviderSessionStatus::NeedsUser;
        self.pending_attention = Some(pending);
        self.updated_at = now.max(self.updated_at);
        self.validate()
    }

    pub(crate) fn complete(&mut self, now: DateTime<Utc>) -> Result<(), &'static str> {
        self.status = ProviderSessionStatus::Completed;
        self.pending_attention = None;
        self.updated_at = now.max(self.updated_at);
        self.validate()
    }

    pub(crate) fn fail(&mut self, now: DateTime<Utc>) -> Result<(), &'static str> {
        self.status = ProviderSessionStatus::Failed;
        self.pending_attention = None;
        self.updated_at = now.max(self.updated_at);
        self.validate()
    }

    pub(crate) fn interrupt(&mut self, now: DateTime<Utc>) -> Result<(), &'static str> {
        self.status = ProviderSessionStatus::Interrupted;
        self.pending_attention = None;
        self.updated_at = now.max(self.updated_at);
        self.validate()
    }

    #[must_use]
    pub const fn id(&self) -> ProviderSessionRecordId {
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
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }
    #[must_use]
    pub const fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    #[must_use]
    pub const fn native_session_id(&self) -> Option<&ProviderSessionId> {
        self.native_session_id.as_ref()
    }
    #[must_use]
    pub const fn current_process_id(&self) -> Option<ManagedProcessId> {
        self.current_process_id
    }
    #[must_use]
    pub const fn status(&self) -> ProviderSessionStatus {
        self.status
    }
    #[must_use]
    pub const fn protocol_version(&self) -> u32 {
        self.protocol_version
    }
    #[must_use]
    pub const fn invocation(&self) -> u32 {
        self.invocation
    }
    #[must_use]
    pub const fn model_id(&self) -> Option<&ModelId> {
        self.model_id.as_ref()
    }
    #[must_use]
    pub fn cli_version(&self) -> Option<&str> {
        self.cli_version.as_deref()
    }
    #[must_use]
    pub const fn pending_attention(&self) -> Option<&PendingProviderAttention> {
        self.pending_attention.as_ref()
    }
    #[must_use]
    pub const fn revision(&self) -> ProviderSessionRevision {
        self.revision
    }
    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}
