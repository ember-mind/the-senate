//! The decision's own `## Follow-ups` section, read verbatim.
//!
//! The Decision/EngineeringLead contract (`stage_prompt::instruction`) asks
//! for an optional `## Follow-ups` section of non-blocking suggested next
//! steps. `[w]` pre-fills its instruction from exactly that section, using
//! the same heading-matching rule `bottom_line` uses for its own quote —
//! factored into `section` so the two extractions cannot drift apart — but
//! keeping the section's own Markdown verbatim rather than reducing it to
//! one line: a follow-up instruction is meant to be handed back to an agent,
//! not read as a headline.

use crate::providers::section;

/// Reads the artifact's `## Follow-ups` section, if it wrote one. `None`
/// when the artifact carries no such section, or wrote an empty one.
pub(crate) fn extract(source: &str) -> Option<String> {
    section::extract_verbatim(source, "followups")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_follow_ups_section_is_extracted_verbatim() {
        let artifact = "# Decision\n\n## Verdict\n\nApproved.\n\n## Follow-ups\n- Add integration coverage for the retry path\n- Consider generalizing the helper used here\n";

        let follow_ups = extract(artifact).unwrap();

        assert_eq!(
            follow_ups,
            "- Add integration coverage for the retry path\n- Consider generalizing the helper used here"
        );
    }

    #[test]
    fn an_artifact_without_the_section_yields_nothing() {
        assert_eq!(extract("# Decision\n\n## Verdict\n\nApproved.\n"), None);
    }

    /// The contract says omit the section entirely rather than pad it; an
    /// agent that wrote the heading anyway with nothing under it still
    /// yields nothing rather than an empty instruction.
    #[test]
    fn an_empty_section_yields_nothing() {
        assert_eq!(extract("## Follow-ups\n\n## Next\ncontent\n"), None);
    }

    #[test]
    fn the_heading_is_recognized_regardless_of_level_case_and_punctuation() {
        for heading in ["## Follow-ups", "### FOLLOW-UPS:", "# follow ups"] {
            let artifact = format!("{heading}\n- one item\n");
            assert_eq!(
                extract(&artifact).as_deref(),
                Some("- one item"),
                "{heading} must be recognized"
            );
        }
    }
}
