use crate::process::OutputChunk;

use super::{ArtifactRecord, ProviderSessionRecord, ProviderSessionRevision};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionMutation {
    session: ProviderSessionRecord,
    expected_revision: ProviderSessionRevision,
}

impl ProviderSessionMutation {
    #[must_use]
    pub const fn new(
        session: ProviderSessionRecord,
        expected_revision: ProviderSessionRevision,
    ) -> Self {
        Self {
            session,
            expected_revision,
        }
    }

    #[must_use]
    pub const fn session(&self) -> &ProviderSessionRecord {
        &self.session
    }
    #[must_use]
    pub const fn expected_revision(&self) -> ProviderSessionRevision {
        self.expected_revision
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCommit {
    output: OutputChunk,
    acknowledged_end: u64,
    session: Option<ProviderSessionMutation>,
    artifact: Option<ArtifactRecord>,
}

impl ProviderCommit {
    #[must_use]
    pub const fn new(output: OutputChunk, acknowledged_end: u64) -> Self {
        Self {
            output,
            acknowledged_end,
            session: None,
            artifact: None,
        }
    }

    #[must_use]
    pub fn with_session(mut self, session: ProviderSessionMutation) -> Self {
        self.session = Some(session);
        self
    }

    #[must_use]
    pub fn with_artifact(mut self, artifact: ArtifactRecord) -> Self {
        self.artifact = Some(artifact);
        self
    }

    #[must_use]
    pub const fn output(&self) -> &OutputChunk {
        &self.output
    }
    #[must_use]
    pub const fn acknowledged_end(&self) -> u64 {
        self.acknowledged_end
    }
    #[must_use]
    pub const fn session(&self) -> Option<&ProviderSessionMutation> {
        self.session.as_ref()
    }
    #[must_use]
    pub const fn artifact(&self) -> Option<&ArtifactRecord> {
        self.artifact.as_ref()
    }
}
