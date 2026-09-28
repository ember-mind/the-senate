//! The artifact's own opening statement, reduced to one plain line.
//!
//! Every stage prompt asks its agent to open the artifact with a
//! `## Bottom line` section: two sentences at most, written for someone who
//! has not read the rest. The hero panel quotes that section verbatim rather
//! than summarizing prose it does not understand, so the panel never states a
//! verdict the artifact did not state itself.
//!
//! Artifacts written before the contract — and any run whose agent ignored it
//! — still have an opening paragraph, which is quoted instead and marked as
//! what it is. Presentation only: the persisted artifact is never rewritten.

use crate::providers::section;

use super::format;

/// One line lifted from an artifact, and whether it came from the contracted
/// section or from the artifact's first paragraph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Opening {
    pub text: String,
    pub contracted: bool,
}

/// Reads the artifact's opening statement. `None` when the artifact carries
/// no prose at all, which is the only case where the panel shows nothing.
pub(crate) fn extract(source: &str) -> Option<Opening> {
    let mut lines = source.lines().peekable();
    let mut fallback: Option<String> = None;
    let mut in_fence = false;
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(heading) = section::heading_text(trimmed) {
            if section::heading_matches(heading, "bottomline") {
                // The contracted section wins wherever it sits, even when an
                // agent placed it after its title block.
                if let Some(text) = paragraph(&mut lines) {
                    return Some(Opening {
                        text,
                        contracted: true,
                    });
                }
            }
            continue;
        }
        if fallback.is_none() && !is_structural(trimmed) {
            fallback = Some(plain(trimmed)).filter(|text| !text.is_empty());
        }
    }
    fallback.map(|text| Opening {
        text,
        contracted: false,
    })
}

/// First prose paragraph after a heading, joined into one line. Blank lines
/// before it are the agent's formatting, not an empty section.
fn paragraph<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Option<String> {
    let mut collected = String::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if collected.is_empty() {
                continue;
            }
            break;
        }
        if section::heading_text(trimmed).is_some() || trimmed.starts_with("```") {
            break;
        }
        let text = plain(trimmed);
        if text.is_empty() {
            continue;
        }
        if !collected.is_empty() {
            collected.push(' ');
        }
        collected.push_str(&text);
    }
    (!collected.is_empty()).then_some(collected)
}

/// Lines that carry structure rather than a statement. A quoted or listed
/// line still reads as prose once its marker is gone, so only rules and
/// tables are refused outright.
fn is_structural(trimmed: &str) -> bool {
    trimmed.is_empty()
        || trimmed.starts_with('|')
        || trimmed
            .chars()
            .all(|c| matches!(c, '-' | '=' | '*' | '_' | ' '))
}

/// One artifact line as plain terminal text: markers and inline Markdown
/// removed, whitespace collapsed, control characters neutralized.
fn plain(trimmed: &str) -> String {
    let stripped = strip_marker(trimmed);
    let mut text = String::with_capacity(stripped.len());
    let mut characters = stripped.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '`' | '*' | '_' => {}
            '\\' => {
                if let Some(escaped) = characters.next() {
                    text.push(escaped);
                }
            }
            '[' => text.push_str(&link_label(&mut characters)),
            _ => text.push(character),
        }
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format::viewer_line(&collapsed)
}

/// Leading list, quote, or checkbox marker, which says how the line is
/// arranged rather than what it says.
fn strip_marker(trimmed: &str) -> &str {
    let mut rest = trimmed;
    loop {
        let stripped = rest
            .strip_prefix("> ")
            .or_else(|| rest.strip_prefix('>'))
            .or_else(|| rest.strip_prefix("- "))
            .or_else(|| rest.strip_prefix("* "))
            .or_else(|| rest.strip_prefix("+ "))
            .or_else(|| numbered_marker(rest))
            .or_else(|| rest.strip_prefix("[ ] "))
            .or_else(|| rest.strip_prefix(" x  "))
            .or_else(|| rest.strip_prefix(" X  "));
        match stripped {
            Some(next) => rest = next.trim_start(),
            None => return rest,
        }
    }
}

fn numbered_marker(rest: &str) -> Option<&str> {
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    rest[digits..]
        .strip_prefix(". ")
        .or_else(|| rest[digits..].strip_prefix(") "))
}

/// Label of an inline link, consumed up to its target. A malformed link
/// degrades to its own literal text rather than swallowing the line.
fn link_label(characters: &mut std::iter::Peekable<impl Iterator<Item = char>>) -> String {
    let mut label = String::new();
    for character in characters.by_ref() {
        if character == ']' {
            break;
        }
        label.push(character);
    }
    if characters.peek() == Some(&'(') {
        characters.next();
        for character in characters.by_ref() {
            if character == ')' {
                break;
            }
        }
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contracted_section_is_quoted_verbatim_wherever_it_sits() {
        let artifact = "# Code Quality Review\n\nScope: three files.\n\n## Bottom line\n\nTwo screens copy the same waiting logic, and one of them dead-ends.\n\n## Must fix\n\nDetail.\n";

        let opening = extract(artifact).unwrap();

        assert!(opening.contracted);
        assert_eq!(
            opening.text,
            "Two screens copy the same waiting logic, and one of them dead-ends."
        );
    }

    #[test]
    fn contracted_heading_is_matched_regardless_of_level_case_and_punctuation() {
        for heading in ["## Bottom line", "### BOTTOM LINE:", "# bottom line"] {
            let artifact = format!("{heading}\nIt is fine.\n");
            let opening = extract(&artifact).unwrap();
            assert!(opening.contracted, "{heading} must be recognized");
            assert_eq!(opening.text, "It is fine.");
        }
    }

    #[test]
    fn multi_line_section_joins_into_one_statement_and_stops_at_the_paragraph() {
        let artifact = "## Bottom line\nIt works.\nOne rough edge remains.\n\nIgnored trailer.\n";

        let opening = extract(artifact).unwrap();

        assert_eq!(opening.text, "It works. One rough edge remains.");
    }

    #[test]
    fn inline_markdown_is_reduced_to_plain_terminal_text() {
        let artifact =
            "## Bottom line\n- **Solid**, but `useInterval` [drifts](https://x.test) badly.\n";

        let opening = extract(artifact).unwrap();

        assert_eq!(opening.text, "Solid, but useInterval drifts badly.");
    }

    #[test]
    fn artifact_without_the_section_falls_back_to_its_first_paragraph() {
        let artifact =
            "## Result\n\nPR #113913 is open, CI green, no reviews.\n\n## Evidence\n\nMore.\n";

        let opening = extract(artifact).unwrap();

        assert!(!opening.contracted);
        assert_eq!(opening.text, "PR #113913 is open, CI green, no reviews.");
    }

    #[test]
    fn fenced_code_is_never_mistaken_for_prose_or_for_a_section() {
        let artifact = "# Research\n\n```\n## Bottom line\nnot prose\n```\n\nReal opening line.\n";

        let opening = extract(artifact).unwrap();

        assert!(!opening.contracted);
        assert_eq!(opening.text, "Real opening line.");
    }

    #[test]
    fn artifact_without_prose_shows_nothing() {
        assert_eq!(extract("# Review\n\n---\n\n"), None);
        assert_eq!(extract(""), None);
    }

    #[test]
    fn control_characters_from_agent_output_never_reach_the_panel() {
        let opening = extract("## Bottom line\nRed \u{1b}[31malert\u{7f} here.\n").unwrap();

        assert!(!opening.text.contains('\u{1b}'));
        assert!(!opening.text.contains('\u{7f}'));
    }
}
