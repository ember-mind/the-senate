//! Native coding-agent provider adapters and provider-owned persisted state.

mod artifact;
pub(crate) mod change_handoff;
mod checkpoint;
pub mod claude;
pub mod codex;
pub(crate) mod continue_instruction;
pub(crate) mod repo_config;
pub(crate) mod section;
mod session;
mod stage_prompt;
pub mod verify;

pub use artifact::{ArtifactRecord, ArtifactRecordError};
pub use checkpoint::{ProviderCommit, ProviderSessionMutation};
pub use session::{
    PendingProviderAttention, ProviderSessionRecord, ProviderSessionRecordId,
    ProviderSessionRevision, ProviderSessionStatus,
};

/// How one native runtime counts the input units it reports.
///
/// The two supported runtimes disagree, and the difference is not cosmetic.
/// Claude Code reports `input_tokens` and `cache_read_input_tokens` as
/// disjoint dimensions: its input total excludes everything its prompt cache
/// served. Codex reports one `input_tokens` total that already contains
/// `cached_input_tokens`.
///
/// Two consequences bind every consumer. Printing a Codex input total beside
/// its cache read counts the same tokens twice. Adding the two runtimes'
/// input totals produces a number that is not a quantity of anything, because
/// the addends measure different things.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAccounting {
    /// Reported input excludes cache reads (Claude Code).
    CacheExclusive,
    /// Reported input already includes cache reads (Codex).
    CacheInclusive,
}

/// Input-accounting convention declared by one provider adapter.
///
/// `None` for any runtime that never declared one. Callers then report its
/// raw reported numbers and derive nothing from them, rather than assuming a
/// convention on its behalf.
#[must_use]
pub fn input_accounting(provider_id: &str) -> Option<InputAccounting> {
    match provider_id {
        "claude" => Some(InputAccounting::CacheExclusive),
        "codex" => Some(InputAccounting::CacheInclusive),
        _ => None,
    }
}

#[cfg(test)]
mod accounting_tests {
    use super::{InputAccounting, input_accounting};

    #[test]
    fn each_supported_runtime_declares_its_own_input_convention() {
        assert_eq!(
            input_accounting("claude"),
            Some(InputAccounting::CacheExclusive)
        );
        assert_eq!(
            input_accounting("codex"),
            Some(InputAccounting::CacheInclusive)
        );
        // An unrecognised runtime never inherits a convention by default.
        assert_eq!(input_accounting("gemini"), None);
    }
}
