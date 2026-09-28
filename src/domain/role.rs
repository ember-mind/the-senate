use serde::{Deserialize, Serialize};

/// Engineering responsibility assigned to a stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Researcher,
    Architect,
    Implementer,
    Simplifier,
    CodeQualityReviewer,
    SpecReviewer,
    /// Legacy/general review responsibility retained for persisted runs.
    Reviewer,
    EngineeringLead,
    /// Runs the repository's own verification commands. Never routed to a
    /// coding-agent provider: the router resolves it implicitly to the
    /// deterministic `verify` provider, so no snapshot has to name it.
    Verifier,
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn role_serialization_contains_no_provider_or_model_vocabulary() {
        assert_eq!(
            serde_json::to_string(&Role::CodeQualityReviewer).unwrap(),
            "\"code_quality_reviewer\""
        );
        assert_eq!(
            serde_json::to_string(&Role::SpecReviewer).unwrap(),
            "\"spec_reviewer\""
        );
        assert_eq!(
            serde_json::to_string(&Role::EngineeringLead).unwrap(),
            "\"engineering_lead\""
        );
    }

    /// Additive like `Simplifier` before it: the new role round-trips
    /// through the same snake-case shape, changing no persisted schema.
    #[test]
    fn verifier_role_round_trips_through_snake_case() {
        assert_eq!(
            serde_json::to_string(&Role::Verifier).unwrap(),
            "\"verifier\""
        );
        assert_eq!(
            serde_json::from_str::<Role>("\"verifier\"").unwrap(),
            Role::Verifier
        );
    }

    #[test]
    fn legacy_reviewer_deserializes_without_conversion() {
        assert_eq!(
            serde_json::from_str::<Role>("\"reviewer\"").unwrap(),
            Role::Reviewer
        );
    }
}
