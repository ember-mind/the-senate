//! Shared Markdown ATX-heading matching for every artifact-section reader —
//! the TUI's `bottom_line` and `follow_ups`, and the publish pull-request
//! draft — so all of them find a section by one tolerant rule (any heading
//! level, case, and punctuation) without duplicating it.

/// Text of an ATX heading, or `None` for any other line.
pub(crate) fn heading_text(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    // `#tag` is not a heading; the space is what makes it one.
    if rest.is_empty() || rest.starts_with(' ') {
        Some(rest.trim())
    } else {
        None
    }
}

/// Whether a heading's text names `target`, regardless of level, case,
/// punctuation, or whitespace — `"Bottom line"`, `"BOTTOM-LINE:"`, and
/// `"bottom  line"` all name `"bottomline"`. `target` must already be given
/// in this same normalized form: letters and digits only, lowercase, no
/// separators, since a multi-word target may be spelled with a space, a
/// hyphen, or nothing at all and this rule must accept every one of them.
pub(crate) fn heading_matches(heading: &str, target: &str) -> bool {
    let normalized: String = heading
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    normalized == target
}

/// Everything under the first heading matching `target`, up to the next
/// heading at any level or the end of the artifact — verbatim, not reduced
/// to one line the way `tui::bottom_line::extract` reduces a quote.
/// Fenced code is tracked so a heading quoted inside one is never mistaken
/// for a real section boundary. `None` when no such heading exists, or the
/// section under it carries no non-blank content.
pub(crate) fn extract_verbatim(source: &str, target: &str) -> Option<String> {
    let mut in_fence = false;
    let mut collecting = false;
    let mut collected: Vec<&str> = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if collecting {
                collected.push(line);
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            if collecting {
                collected.push(line);
            }
            continue;
        }
        if let Some(heading) = heading_text(trimmed) {
            if collecting {
                // The next heading at any level closes the section.
                break;
            }
            if heading_matches(heading, target) {
                collecting = true;
            }
            continue;
        }
        if collecting {
            collected.push(line);
        }
    }
    let text = collected.join("\n");
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heading_is_matched_regardless_of_level_case_and_punctuation() {
        for line in ["## Follow-ups", "### FOLLOW UPS:", "# follow-ups"] {
            let heading = heading_text(line).expect("an ATX heading");
            assert!(
                heading_matches(heading, "followups"),
                "{line} must be recognized"
            );
        }
        assert_eq!(heading_text("not a heading"), None);
    }

    #[test]
    fn a_section_is_extracted_verbatim_up_to_the_next_heading() {
        let artifact = "# Decision\n\n## Follow-ups\n- Generalize the helper\n- Add a regression test\n\n## Other\nignored\n";

        let section = extract_verbatim(artifact, "followups").unwrap();

        assert_eq!(section, "- Generalize the helper\n- Add a regression test");
    }

    #[test]
    fn a_section_stops_at_a_heading_of_any_level() {
        let artifact = "## Follow-ups\nfirst\n### still inside a subheading? no\n";

        let section = extract_verbatim(artifact, "followups").unwrap();

        assert_eq!(section, "first");
    }

    #[test]
    fn fenced_code_naming_the_section_is_never_mistaken_for_one() {
        let artifact = "```\n## Follow-ups\nnot a real section\n```\n\nReal text.\n";

        assert_eq!(extract_verbatim(artifact, "followups"), None);
    }

    #[test]
    fn a_missing_section_or_an_empty_one_yields_nothing() {
        assert_eq!(
            extract_verbatim("# Decision\n\nNo such section.\n", "followups"),
            None
        );
        assert_eq!(
            extract_verbatim("## Follow-ups\n\n## Next\nx\n", "followups"),
            None
        );
    }
}
