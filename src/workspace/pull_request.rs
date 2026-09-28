//! The pull request an editing stage wrote for its own change, read verbatim.
//!
//! Every stage that edits the workspace closes its artifact with a
//! `## Pull request` section (`stage_prompt::PULL_REQUEST`): the title on its
//! first line, the description under it. Publish quotes that section rather
//! than composing a pull request from the task text, which says what was
//! asked and nothing about what was done. Reading is presentation-only: the
//! persisted artifact is never rewritten, and a run whose agent ignored the
//! contract publishes exactly as it did before the contract existed.

use crate::providers::section;

/// Title and description quoted from the artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestDraft {
    pub title: String,
    /// May be empty when the agent wrote a title and nothing under it; the
    /// publisher then falls back to the task text for the description alone.
    pub body: String,
}

/// Reads the artifact's `## Pull request` section. `None` when the artifact
/// has no such section or nothing usable as a title under it.
#[must_use]
pub fn extract(artifact: &str) -> Option<PullRequestDraft> {
    let mut lines = artifact.lines();
    let mut in_fence = false;
    let mut found = false;
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if section::heading_text(trimmed)
            .is_some_and(|h| section::heading_matches(h, "pullrequest"))
        {
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    let mut lines = lines.peekable();
    let title = loop {
        let line = lines.next()?;
        if line.trim().is_empty() {
            continue;
        }
        break title_text(line);
    };
    if title.is_empty() {
        return None;
    }
    let body = promote_headings(lines.collect::<Vec<_>>().join("\n").trim());
    Some(PullRequestDraft { title, body })
}

/// The title line as a title: a `Title:` label, a heading marker, or wrapping
/// emphasis is how an agent presented it, not part of it.
fn title_text(line: &str) -> String {
    let mut text = line.trim();
    text = text.trim_start_matches('#').trim();
    for label in ["Title:", "title:", "TITLE:", "**Title:**", "**Title**:"] {
        if let Some(rest) = text.strip_prefix(label) {
            text = rest.trim();
            break;
        }
    }
    let text = text
        .strip_prefix("**")
        .and_then(|inner| inner.strip_suffix("**"))
        .unwrap_or(text);
    text.trim().to_owned()
}

/// Lifts the description's headings so its shallowest one is `##`. The
/// contract nests them under `## Pull request` as `###`, which is right for
/// the artifact and one level too deep for a pull request of their own.
fn promote_headings(body: &str) -> String {
    let mut in_fence = false;
    let mut shallowest = usize::MAX;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && section::heading_text(trimmed).is_some() {
            shallowest = shallowest.min(trimmed.chars().take_while(|c| *c == '#').count());
        }
    }
    if shallowest == usize::MAX || shallowest <= 2 {
        return body.to_owned();
    }
    let lift = shallowest - 2;
    let mut in_fence = false;
    let mut out = String::with_capacity(body.len());
    for (index, line) in body.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && section::heading_text(trimmed).is_some() {
            out.push_str(&trimmed[lift..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIFACT: &str = "## Bottom line\nDone.\n\n# Implementation\n\n## What changed\nEdited `src/lib.rs`.\n\n## Pull request\nWiden Help Center video embeds\n\nFixes https://linear.app/a8c/issue/DOTCOM-1/full-width\n\n### Proposed changes\n\n**Videos now span the full panel width.**\n\n- Drops the lesson-only modifier.\n\n### Why\n\nVideos looked small.\n\n### Testing\n\n1. Open a guide with a video.\n";

    #[test]
    fn the_first_line_is_the_title_and_the_rest_is_the_description() {
        let draft = extract(ARTIFACT).unwrap();
        assert_eq!(draft.title, "Widen Help Center video embeds");
        assert!(
            draft
                .body
                .starts_with("Fixes https://linear.app/a8c/issue/DOTCOM-1/full-width\n")
        );
        assert!(draft.body.ends_with("1. Open a guide with a video."));
    }

    /// The contract nests the description's headings under `## Pull request`;
    /// on GitHub they stand alone and read one level up.
    #[test]
    fn nested_headings_are_lifted_to_stand_alone() {
        let draft = extract(ARTIFACT).unwrap();
        assert!(draft.body.contains("\n## Proposed changes\n"));
        assert!(draft.body.contains("\n## Why\n"));
        assert!(draft.body.contains("\n## Testing\n"));
        assert!(!draft.body.contains("###"));
    }

    #[test]
    fn headings_already_at_pull_request_depth_are_left_alone() {
        let draft = extract("## Pull request\nTitle here\n\n## Why\n\nBecause.\n").unwrap();
        assert_eq!(draft.body, "## Why\n\nBecause.");
    }

    #[test]
    fn a_heading_inside_a_fence_is_neither_a_boundary_nor_lifted() {
        let artifact = "# Notes\n```md\n## Pull request\nnot the section\n```\n## Pull request\nReal title\n\n### Testing\n```sh\n### not a heading\n```\n";
        let draft = extract(artifact).unwrap();
        assert_eq!(draft.title, "Real title");
        assert_eq!(draft.body, "## Testing\n```sh\n### not a heading\n```");
    }

    #[test]
    fn labels_and_emphasis_around_the_title_are_not_part_of_it() {
        for line in [
            "Title: Fix the thing",
            "**Title:** Fix the thing",
            "**Fix the thing**",
            "### Fix the thing",
        ] {
            let artifact = format!("## Pull request\n{line}\n\nBody.\n");
            assert_eq!(
                extract(&artifact).unwrap().title,
                "Fix the thing",
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_title_with_nothing_under_it_still_yields_the_title() {
        let draft = extract("## Pull request\n\nOnly a title\n").unwrap();
        assert_eq!(draft.title, "Only a title");
        assert_eq!(draft.body, "");
    }

    #[test]
    fn an_artifact_without_the_section_or_without_a_title_yields_nothing() {
        assert_eq!(extract("# Implementation\n\nDone.\n"), None);
        assert_eq!(extract("## Pull request\n\n\n"), None);
        assert_eq!(extract("## Pull request\n"), None);
    }

    #[test]
    fn the_heading_is_recognized_regardless_of_level_case_and_punctuation() {
        for heading in ["## Pull request", "### PULL-REQUEST:", "# pull request"] {
            let artifact = format!("{heading}\nA title\n");
            assert_eq!(extract(&artifact).unwrap().title, "A title", "{heading}");
        }
    }
}
