use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    AttentionKind, AttentionRequestId, EventId, ModelId, ProviderId, ProviderSessionId, RunId,
    StageId, WorkflowKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventMetadata {
    id: EventId,
    occurred_at: DateTime<Utc>,
}

impl EventMetadata {
    #[must_use]
    pub const fn new(id: EventId, occurred_at: DateTime<Utc>) -> Self {
        Self { id, occurred_at }
    }

    #[must_use]
    pub const fn id(&self) -> EventId {
        self.id
    }

    #[must_use]
    pub const fn occurred_at(self) -> DateTime<Utc> {
        self.occurred_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEvent {
    id: EventId,
    occurred_at: DateTime<Utc>,
    run_id: RunId,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage_id: Option<StageId>,
    #[serde(flatten)]
    kind: DomainEventKind,
}

impl DomainEvent {
    #[must_use]
    pub const fn new(
        metadata: EventMetadata,
        run_id: RunId,
        stage_id: Option<StageId>,
        kind: DomainEventKind,
    ) -> Self {
        Self {
            id: metadata.id,
            occurred_at: metadata.occurred_at,
            run_id,
            stage_id,
            kind,
        }
    }

    #[must_use]
    pub const fn id(&self) -> EventId {
        self.id
    }

    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub const fn stage_id(&self) -> Option<&StageId> {
        self.stage_id.as_ref()
    }

    #[must_use]
    pub const fn occurred_at(&self) -> &DateTime<Utc> {
        &self.occurred_at
    }

    #[must_use]
    pub const fn kind(&self) -> &DomainEventKind {
        &self.kind
    }
}

/// Provider-neutral semantic history and integration signal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEventKind {
    RunCreated {
        workflow: WorkflowKind,
    },
    RunPreparationStarted,
    RunPrepared,
    RunStarted,
    RunPaused,
    RunInterrupted,
    RunResumed,
    RunRecovered,
    RunCompleted,
    RunFailed,
    RunApplied,
    RunDiscarded,
    StageReady {
        degraded: bool,
    },
    StageStarted,
    StagePaused,
    StageInterrupted,
    StageResumed,
    StageRecovered,
    StageCompleted,
    StageSkipped,
    StageFailed,
    StageRetryScheduled,
    /// An operator sent one failed stage to a different provider (and
    /// optionally model) than its role was configured with, ahead of retrying
    /// it. The configuration snapshot is untouched; this records the
    /// exception and why it was made.
    StageRouteOverridden {
        provider_id: ProviderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_id: Option<ModelId>,
        reason: String,
    },
    NeedsUser {
        attention_request_id: AttentionRequestId,
        kind: AttentionKind,
    },
    AttentionResolved {
        attention_request_id: AttentionRequestId,
    },
    AttentionCancelled {
        attention_request_id: AttentionRequestId,
    },
    ProviderStarted {
        provider_id: ProviderId,
        model_id: Option<ModelId>,
        session_id: Option<ProviderSessionId>,
    },
    ProviderResumed {
        provider_id: ProviderId,
        session_id: ProviderSessionId,
    },
    ProviderProgress {
        provider_id: ProviderId,
        message: String,
    },
    ProviderNeedsUser {
        provider_id: ProviderId,
        session_id: Option<ProviderSessionId>,
        attention_request_id: AttentionRequestId,
    },
    ProviderPaused {
        provider_id: ProviderId,
        session_id: Option<ProviderSessionId>,
    },
    ProviderInterrupted {
        provider_id: ProviderId,
        session_id: Option<ProviderSessionId>,
    },
    ProviderCompleted {
        provider_id: ProviderId,
        session_id: Option<ProviderSessionId>,
    },
    ProviderFailed {
        provider_id: ProviderId,
        session_id: Option<ProviderSessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// An operator asked a completed run to remediate its own result.
    ///
    /// Records which stages the run grew and, when given, the operator's
    /// instruction. Append-only like every other event: the run's shape at any
    /// point is the built-in workflow plus the cycles recorded here.
    RunFixRequested {
        stage_ids: Vec<StageId>,
    },
    /// An operator asked a completed run to continue with a fresh instruction
    /// of their own, rather than answering the verdict's blocking findings.
    ///
    /// Distinct from `RunFixRequested` in meaning, not in shape: a continue
    /// cycle carries the operator's own next instruction rather than
    /// resolving findings the decision called blocking. The instruction text
    /// itself is never in this payload — it is immutable run-private stdin
    /// content, the same mechanism an attention response uses — so, exactly
    /// like its sibling event, this records only which stages appeared.
    RunContinueRequested {
        stage_ids: Vec<StageId>,
    },
    /// An operator moved a completed run's workspace onto the source
    /// checkout's current `HEAD`, because the checkout had moved on since the
    /// run started and its patch no longer applied there.
    ///
    /// Recorded on the run rather than only on the workspace row because it
    /// is what makes an earlier passed verification stale: the checks ran
    /// over `from_base` plus the delta, and the delta now stands on
    /// `to_base`. The apply gate reads this event's position in the log
    /// against the latest verification's, so the fact has to be in the log.
    WorkspaceRebased {
        from_base: String,
        to_base: String,
    },
    /// An operator pushed a completed run's delta to `origin`, onto `branch`
    /// at `commit`, and — when GitHub answered — to the pull request at
    /// `pull_request_url`.
    ///
    /// Transport, not disposition: the run stays `Completed`. Recorded so the
    /// interface can say whether the run is already published and whether a
    /// later fix, continue or rebase has left that publication behind; the
    /// fact otherwise lives only on the remote.
    RunPublished {
        branch: String,
        commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pull_request_url: Option<String>,
    },
    UsageUpdated,
    /// What the native runtime's own records say it ran for this stage.
    ///
    /// Distinct from `ProviderStarted`, which carries what the runtime
    /// announced at launch (nothing, for a runtime that does not announce a
    /// model). Absent fields mean the runtime never made the fact observable;
    /// they never mean "as configured".
    ProviderRuntimeObserved {
        provider_id: ProviderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_id: Option<ModelId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_effort: Option<String>,
    },
    ProviderUsageUpdated {
        provider_id: ProviderId,
        input_units: u64,
        output_units: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_units: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_units: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_output_units: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_models: Option<Vec<NativeModelUsage>>,
    },
}

/// One native-runtime-reported per-model usage entry.
///
/// Units are provider-native and never normalized across providers. The
/// breakdown is a parallel provider-defined view of the same execution and
/// MUST NOT be summed together with the aggregate `input_units`/`output_units`
/// of the carrying event: for Claude the aggregate covers the primary agent
/// while the breakdown spans every model the runtime used (including
/// subagents), so the two views overlap. `None` means the runtime did not
/// report the dimension; it is never a synonym for zero.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeModelUsage {
    pub model: String,
    pub input_units: u64,
    pub output_units: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_units: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_units: Option<u64>,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn semantic_event_round_trip_uses_explicit_snake_case_type() {
        let event = DomainEvent::new(
            EventMetadata::new(
                EventId::from_u128(1),
                Utc.with_ymd_and_hms(2026, 8, 14, 8, 0, 0).single().unwrap(),
            ),
            RunId::from_u128(2),
            Some(StageId::new("review").unwrap()),
            DomainEventKind::StageReady { degraded: true },
        );
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: DomainEvent = serde_json::from_str(&encoded).unwrap();

        assert!(encoded.contains("\"type\":\"stage_ready\""));
        assert_eq!(decoded, event);
    }

    #[test]
    fn pre_m13a_usage_event_payload_decodes_with_unavailable_resource_dimensions() {
        // Exact shape persisted before resource observability existed. Missing
        // dimensions must decode as unavailable (None), never as zero.
        // Derive the exact pre-M13a JSON from a real event by re-encoding:
        // the new optional fields are absent when None, so this byte shape is
        // identical to what pre-M13a code persisted.
        let event = DomainEvent::new(
            EventMetadata::new(
                EventId::from_u128(1),
                Utc.with_ymd_and_hms(2026, 8, 14, 8, 0, 0).single().unwrap(),
            ),
            RunId::from_u128(2),
            Some(StageId::new("implementation").unwrap()),
            DomainEventKind::ProviderUsageUpdated {
                provider_id: ProviderId::new("claude").unwrap(),
                input_units: 18,
                output_units: 83,
                cache_read_units: None,
                cache_write_units: None,
                reasoning_output_units: None,
                native_models: None,
            },
        );
        let legacy = serde_json::to_string(&event).unwrap();
        assert!(legacy.contains("\"input_units\":18"));
        let decoded: DomainEvent = serde_json::from_str(&legacy).unwrap();
        let DomainEventKind::ProviderUsageUpdated {
            input_units,
            output_units,
            cache_read_units,
            cache_write_units,
            reasoning_output_units,
            native_models,
            ..
        } = decoded.kind()
        else {
            panic!("expected usage event");
        };
        assert_eq!((*input_units, *output_units), (18, 83));
        assert_eq!(*cache_read_units, None);
        assert_eq!(*cache_write_units, None);
        assert_eq!(*reasoning_output_units, None);
        assert_eq!(*native_models, None);
        // Round-trip keeps unavailable dimensions absent rather than zero.
        let encoded = serde_json::to_string(&decoded).unwrap();
        assert!(!encoded.contains("cache_read_units"));
        assert!(!encoded.contains("native_models"));
    }

    #[test]
    fn usage_event_distinguishes_reported_zero_from_unavailable() {
        let event = DomainEvent::new(
            EventMetadata::new(
                EventId::from_u128(3),
                Utc.with_ymd_and_hms(2026, 8, 19, 8, 0, 0).single().unwrap(),
            ),
            RunId::from_u128(4),
            Some(StageId::new("implementation").unwrap()),
            DomainEventKind::ProviderUsageUpdated {
                provider_id: ProviderId::new("codex").unwrap(),
                input_units: 100,
                output_units: 20,
                cache_read_units: Some(0),
                cache_write_units: None,
                reasoning_output_units: Some(5),
                native_models: None,
            },
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("\"cache_read_units\":0"));
        assert!(!encoded.contains("cache_write_units"));
        let decoded: DomainEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);
    }
}
