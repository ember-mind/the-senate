use std::collections::{HashMap, HashSet};

use thiserror::Error;

use crate::domain::{
    AttentionKind, ModelId, ProviderId, ProviderSessionId, StageId, WorkflowDefinition,
};
use crate::store::SqliteStore;

use super::{Provider, ProviderError, ProviderPoll, ProviderRequest, ProviderSignal, UsageDelta};

/// One scripted `FakeProvider` action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeEvent {
    Started,
    Progress(String),
    NeedsUser {
        kind: AttentionKind,
        summary: String,
    },
    Usage(UsageDelta),
    Paused,
    Interrupted,
    Completed,
    Failed(String),
    /// Holds polling at current durable cursor until explicitly released.
    Delay(String),
}

impl FakeEvent {
    #[must_use]
    pub fn progress(message: impl Into<String>) -> Self {
        Self::Progress(message.into())
    }

    #[must_use]
    pub fn needs_user(kind: AttentionKind, summary: impl Into<String>) -> Self {
        Self::NeedsUser {
            kind,
            summary: summary.into(),
        }
    }

    #[must_use]
    pub fn failed(reason: impl Into<String>) -> Self {
        Self::Failed(reason.into())
    }

    #[must_use]
    pub fn delay(gate: impl Into<String>) -> Self {
        Self::Delay(gate.into())
    }

    const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed(_))
    }

    const fn is_delay(&self) -> bool {
        matches!(self, Self::Delay(_))
    }
}

/// Fluent collection of per-stage provider scripts.
#[derive(Clone, Debug, Default)]
pub struct FakeScenario {
    stages: Vec<(String, Vec<FakeEvent>)>,
    /// Whether an unscripted stage should still succeed generically, rather
    /// than fail the poll. Set only by [`Self::successful`]: a hand-rolled
    /// scenario built through [`Self::new`]/[`Self::stage`] keeps failing
    /// closed on an unknown stage, because that refusal is what catches a
    /// scheduler bug in a test that scripted every stage it expected to see.
    default_success: bool,
}

impl FakeScenario {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stages: Vec::new(),
            default_success: false,
        }
    }

    #[must_use]
    pub fn stage(self, stage_id: impl Into<String>) -> FakeStageBuilder {
        FakeStageBuilder {
            scenario: self,
            stage_id: stage_id.into(),
        }
    }

    /// Builds restart-stable success scripts from graph data only, and keeps
    /// succeeding generically for any stage appended after the fact.
    ///
    /// A completed run's `[f]`/`[c]` remediation cycle appends stages the
    /// workflow did not have when this scenario was built — the whole point
    /// of a fake "everything succeeds" provider is that it should not need
    /// to be told about them in advance. `RunFixError`/continue growth
    /// happens on the same provider instance mid-drive, so the eager script
    /// map here is necessarily a snapshot; [`FakeProvider::poll`] tops it up
    /// lazily rather than failing a stage this scenario's own promise
    /// covers.
    #[must_use]
    pub fn successful(workflow: &WorkflowDefinition) -> Self {
        let mut scenario = Self {
            default_success: true,
            ..Self::new()
        };
        for stage in workflow.stages() {
            scenario = scenario
                .stage(stage.id().as_str())
                .events(default_success_events(stage.id()));
        }
        scenario
    }
}

/// The generic success script one stage gets, whether scripted up front by
/// [`FakeScenario::successful`] or synthesized lazily by
/// [`FakeProvider::poll`] for a stage that scenario did not know about yet.
fn default_success_events(stage_id: &StageId) -> Vec<FakeEvent> {
    vec![
        FakeEvent::Started,
        FakeEvent::progress(format!("Executing {stage_id}")),
        FakeEvent::Usage(UsageDelta::stable(10, 5)),
        FakeEvent::Completed,
    ]
}

pub struct FakeStageBuilder {
    scenario: FakeScenario,
    stage_id: String,
}

impl FakeStageBuilder {
    #[must_use]
    pub fn events(mut self, events: impl IntoIterator<Item = FakeEvent>) -> FakeScenario {
        self.scenario
            .stages
            .push((self.stage_id, events.into_iter().collect()));
        self.scenario
    }
}

/// Deterministic provider driven only by durable request cursor and scenario.
pub struct FakeProvider {
    id: ProviderId,
    scripts: HashMap<StageId, Vec<FakeEvent>>,
    /// Carried from [`FakeScenario::default_success`]: whether a stage
    /// missing from `scripts` should still succeed generically.
    default_success: bool,
    released_gates: HashSet<(StageId, String)>,
}

impl FakeProvider {
    /// Validates scripts before execution.
    ///
    /// # Errors
    /// Rejects duplicate/invalid stage IDs, malformed lifecycle scripts, and
    /// empty messages or delay gates.
    pub fn new(scenario: FakeScenario) -> Result<Self, FakeScenarioError> {
        let mut scripts = HashMap::new();
        for (raw_stage_id, events) in scenario.stages {
            let stage_id = StageId::new(raw_stage_id.clone())
                .map_err(|_| FakeScenarioError::InvalidStageId(raw_stage_id))?;
            validate_script(&stage_id, &events)?;
            if scripts.insert(stage_id.clone(), events).is_some() {
                return Err(FakeScenarioError::DuplicateStage(stage_id));
            }
        }
        let id = ProviderId::new("fake").map_err(|_| FakeScenarioError::InvalidProviderId)?;
        Ok(Self {
            id,
            scripts,
            default_success: scenario.default_success,
            released_gates: HashSet::new(),
        })
    }

    /// Releases one named deterministic delay gate.
    ///
    /// # Errors
    /// Rejects invalid stage IDs.
    pub fn release(
        &mut self,
        stage_id: &str,
        gate: impl Into<String>,
    ) -> Result<(), FakeScenarioError> {
        let stage_id = StageId::new(stage_id)
            .map_err(|_| FakeScenarioError::InvalidStageId(stage_id.to_owned()))?;
        self.released_gates.insert((stage_id, gate.into()));
        Ok(())
    }
}

impl Provider for FakeProvider {
    fn provider_id_for(&self, _request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
        Ok(self.id.clone())
    }

    fn supports_role(&self, _role: crate::domain::Role) -> bool {
        true
    }

    fn poll(
        &mut self,
        _store: &mut SqliteStore,
        request: &ProviderRequest,
    ) -> Result<ProviderPoll, ProviderError> {
        if !self.scripts.contains_key(request.stage_id()) {
            if !self.default_success {
                return Err(ProviderError::new(format!(
                    "no fake script for stage {}",
                    request.stage_id()
                )));
            }
            // A remediation cycle appended this stage after the scenario was
            // built; `successful()` promised every stage succeeds, so this
            // one gets the same generic script the eager loop would have
            // given it had it existed at construction time.
            self.scripts.insert(
                request.stage_id().clone(),
                default_success_events(request.stage_id()),
            );
        }
        let events = self
            .scripts
            .get(request.stage_id())
            .expect("just inserted or already present");
        let mut signal_index = 0_usize;
        for event in events {
            if let FakeEvent::Delay(gate) = event {
                if signal_index == request.signal_index()
                    && !self
                        .released_gates
                        .contains(&(request.stage_id().clone(), gate.clone()))
                {
                    return Ok(ProviderPoll::Pending);
                }
                continue;
            }
            if signal_index == request.signal_index() {
                return Ok(ProviderPoll::Signal(to_signal(event, request)?));
            }
            signal_index = signal_index
                .checked_add(1)
                .ok_or_else(|| ProviderError::new("fake signal cursor overflow"))?;
        }
        Err(ProviderError::new(format!(
            "fake script for stage {} exhausted at cursor {}",
            request.stage_id(),
            request.signal_index()
        )))
    }
}

fn to_signal(
    event: &FakeEvent,
    request: &ProviderRequest,
) -> Result<ProviderSignal, ProviderError> {
    let signal = match event {
        FakeEvent::Started => ProviderSignal::Started {
            model_id: Some(ModelId::new("fake-model").expect("static model ID must be valid")),
            session_id: Some(
                ProviderSessionId::new(format!(
                    "fake-{}-{}-{}",
                    request.run_id(),
                    request.stage_id(),
                    request.attempt()
                ))
                .map_err(|error| ProviderError::new(error.to_string()))?,
            ),
        },
        FakeEvent::Progress(message) => ProviderSignal::Progress(message.clone()),
        FakeEvent::NeedsUser { kind, summary } => ProviderSignal::NeedsUser {
            kind: *kind,
            summary: summary.clone(),
            request_id: None,
        },
        FakeEvent::Usage(usage) => ProviderSignal::Usage(usage.clone()),
        FakeEvent::Paused => ProviderSignal::Paused,
        FakeEvent::Interrupted => ProviderSignal::Interrupted,
        FakeEvent::Completed => ProviderSignal::Completed,
        FakeEvent::Failed(reason) => ProviderSignal::Failed(reason.clone()),
        FakeEvent::Delay(_) => return Err(ProviderError::new("delay cannot become a signal")),
    };
    Ok(signal)
}

fn validate_script(stage_id: &StageId, events: &[FakeEvent]) -> Result<(), FakeScenarioError> {
    let signals = events
        .iter()
        .filter(|event| !event.is_delay())
        .collect::<Vec<_>>();
    if signals.is_empty() {
        return Err(FakeScenarioError::EmptyScript(stage_id.clone()));
    }
    if !matches!(signals.first(), Some(FakeEvent::Started)) {
        return Err(FakeScenarioError::MustStart(stage_id.clone()));
    }
    if !signals.last().is_some_and(|event| event.is_terminal()) {
        return Err(FakeScenarioError::MustEnd(stage_id.clone()));
    }
    if signals
        .iter()
        .skip(1)
        .any(|event| matches!(event, FakeEvent::Started))
    {
        return Err(FakeScenarioError::RepeatedStart(stage_id.clone()));
    }
    if signals
        .iter()
        .take(signals.len().saturating_sub(1))
        .any(|event| event.is_terminal())
    {
        return Err(FakeScenarioError::EventAfterTerminal(stage_id.clone()));
    }
    for event in events {
        let empty = match event {
            FakeEvent::Progress(message)
            | FakeEvent::Failed(message)
            | FakeEvent::Delay(message) => message.trim().is_empty(),
            FakeEvent::NeedsUser { summary, .. } => summary.trim().is_empty(),
            _ => false,
        };
        if empty {
            return Err(FakeScenarioError::EmptyText(stage_id.clone()));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum FakeScenarioError {
    #[error("static fake provider ID is invalid")]
    InvalidProviderId,
    #[error("invalid fake stage ID: {0:?}")]
    InvalidStageId(String),
    #[error("duplicate fake script for stage {0}")]
    DuplicateStage(StageId),
    #[error("fake script for stage {0} has no provider signals")]
    EmptyScript(StageId),
    #[error("fake script for stage {0} must begin with Started")]
    MustStart(StageId),
    #[error("fake script for stage {0} repeats Started")]
    RepeatedStart(StageId),
    #[error("fake script for stage {0} must end with Completed or Failed")]
    MustEnd(StageId),
    #[error("fake script for stage {0} contains an event after its terminal signal")]
    EventAfterTerminal(StageId),
    #[error("fake script for stage {0} contains empty text")]
    EmptyText(StageId),
}
