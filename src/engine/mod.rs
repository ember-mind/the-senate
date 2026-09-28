//! Deterministic workflow scheduling and provider boundaries.

mod error;
mod fake;
mod provider;
mod scheduler;

pub use error::EngineError;
pub use fake::{FakeEvent, FakeProvider, FakeScenario, FakeScenarioError, FakeStageBuilder};
pub use provider::{
    Provider, ProviderAttentionContext, ProviderError, ProviderPoll, ProviderRequest,
    ProviderSignal, UsageDelta,
};
pub use scheduler::{EngineStatus, ExecutionContext, SystemExecutionContext, WorkflowEngine};
