use std::fmt::Write as _;

use crate::engine::ProviderRequest;
use crate::providers::change_handoff::ChangeHandoff;
use crate::providers::{ArtifactRecord, change_handoff, stage_prompt};

use super::OpencodeProviderError;
use super::command::MAX_PROMPT_BYTES;

const MAX_DEPENDENCY_BYTES: u64 = 256 * 1024;

/// Heading the operator-instruction section always opens with, shared by its
/// renderer and the tests that compute the same overhead it does.
const OPERATOR_INSTRUCTION_HEADER: &str = "\n# Operator instruction\n";

pub(crate) fn compose(
    request: &ProviderRequest,
    artifacts: &[ArtifactRecord],
    handoff: Option<&ChangeHandoff>,
    continue_instruction: Option<&str>,
) -> Result<String, OpencodeProviderError> {
    let mut prompt = String::new();
    writeln!(prompt, "# The Senate stage").expect("String writes cannot fail");
    writeln!(prompt, "Task: {}", request.task()).expect("String writes cannot fail");
    writeln!(
        prompt,
        "Stage: {} ({:?})",
        request.stage_id(),
        request.stage_kind()
    )
    .expect("String writes cannot fail");
    writeln!(prompt, "Role: {:?}", request.role()).expect("String writes cannot fail");
    writeln!(
        prompt,
        "\n{}",
        stage_prompt::instruction(request.role(), request.stage_kind())
    )
    .expect("String writes cannot fail");
    writeln!(
        prompt,
        "You are executing one stage for The Senate. Work only inside current managed worktree. Respect repository instructions, AGENTS.md, rules, skills, and native opencode configuration discovered normally. Do not apply changes to another checkout. Do not invoke The Senate apply. Do not commit or push. Return concise Markdown describing result, evidence, and unresolved risks."
    )
    .expect("String writes cannot fail");
    writeln!(prompt, "{}", stage_prompt::BOTTOM_LINE).expect("String writes cannot fail");
    if request.stage_kind().edits_workspace() {
        writeln!(
            prompt,
            "Make required changes in managed worktree and run proportionate local validation when safe."
        )
        .expect("String writes cannot fail");
        writeln!(prompt, "{}", stage_prompt::PULL_REQUEST).expect("String writes cannot fail");
    } else {
        writeln!(
            prompt,
            "Inspect, reason, and report only. Do not modify repository content."
        )
        .expect("String writes cannot fail");
    }

    let dependencies = stage_prompt::direct_dependency_artifacts(request, artifacts);
    if !dependencies.is_empty() {
        prompt.push_str("\n# Direct dependency artifacts\n");
    }
    for artifact in dependencies {
        let metadata = std::fs::metadata(artifact.path())?;
        if metadata.len() > MAX_DEPENDENCY_BYTES {
            return Err(OpencodeProviderError::ArtifactTooLarge(
                usize::try_from(MAX_DEPENDENCY_BYTES).expect("constant fits usize"),
            ));
        }
        let content = std::fs::read_to_string(artifact.path())?;
        writeln!(
            prompt,
            "\n## {} ({:?})\n{}",
            artifact.metadata().stage_id(),
            artifact.metadata().kind(),
            content
        )
        .expect("String writes cannot fail");
    }
    // Same immutable run-private path an attention response or Codex's
    // follow-up prompt uses, never argv beyond this composed prompt itself
    // and never a domain event payload.
    if let Some(instruction) = continue_instruction {
        let room = MAX_PROMPT_BYTES.saturating_sub(prompt.len());
        let section = continue_instruction_within(instruction, room);
        if section.is_empty() {
            return Err(OpencodeProviderError::ContinueInstructionOmitted(
                request.stage_id().clone(),
            ));
        }
        prompt.push_str(&section);
    }
    if let Some(handoff) = handoff {
        // The change map is navigation aid, not source of truth, so it yields
        // whatever room the rest of the prompt left — same discipline Codex's
        // prompt uses, at opencode's tighter argv-safety ceiling.
        let room = MAX_PROMPT_BYTES.saturating_sub(prompt.len());
        prompt.push_str(&change_handoff::render_within(handoff, room));
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(OpencodeProviderError::PromptTooLarge(MAX_PROMPT_BYTES));
    }
    Ok(prompt)
}

/// Renders the operator-instruction section so it fits inside `max_bytes`,
/// truncating the instruction text itself rather than silently dropping the
/// section or letting the composed prompt exceed opencode's argv-safety
/// ceiling. Mirrors Codex's own `continue_instruction_within`.
fn continue_instruction_within(instruction: &str, max_bytes: usize) -> String {
    let full = format!("{OPERATOR_INSTRUCTION_HEADER}{instruction}\n");
    if full.len() <= max_bytes {
        return full;
    }
    let overhead = OPERATOR_INSTRUCTION_HEADER.len()
        + incomplete_marker(instruction.len(), instruction.len()).len()
        + 32;
    if overhead > max_bytes {
        return String::new();
    }
    let mut room = max_bytes.saturating_sub(overhead).min(instruction.len());
    while room > 0 && !instruction.is_char_boundary(room) {
        room -= 1;
    }
    let mut section = String::with_capacity(max_bytes.min(instruction.len() + 256));
    section.push_str(OPERATOR_INSTRUCTION_HEADER);
    section.push_str(&instruction[..room]);
    section.push_str(&incomplete_marker(room, instruction.len()));
    section
}

fn incomplete_marker(shown: usize, total: usize) -> String {
    format!(
        "\nCompleteness: INCOMPLETE — the operator's instruction exceeds opencode's argv-safety limit here ({shown} of {total} instruction bytes shown). Treat this as a partial instruction; the rest was not delivered.\n"
    )
}

/// opencode has no dedicated resume-continuation grammar of its own; this
/// mirrors Codex's, since both continue same native session state and both
/// need the operator's stage kind restated rather than re-sent context.
pub(crate) fn continuation(request: &ProviderRequest) -> String {
    format!(
        "Continue exact interrupted stage {} for The Senate in same native opencode session. Finish assigned {:?} work in current managed worktree. {} Do not commit, push, or apply changes to another checkout. Return final Markdown result.",
        request.stage_id(),
        request.stage_kind(),
        stage_prompt::instruction(request.role(), request.stage_kind())
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::domain::{ProviderSessionId, Role, RunId, StageId, StageKind, StageStatus};
    use crate::git::{ChangeKind, ChangedFileRecord};

    use super::*;

    fn request(role: Role, kind: StageKind) -> ProviderRequest {
        ProviderRequest::new(
            RunId::from_u128(2),
            StageId::new("review").unwrap(),
            kind,
            StageStatus::Ready,
            role,
            "immutable task".to_owned(),
            PathBuf::from("/managed/worktree"),
            1,
            0,
            Option::<ProviderSessionId>::None,
            vec![],
        )
    }

    fn handoff() -> ChangeHandoff {
        ChangeHandoff::for_tests(
            &"b".repeat(40),
            vec![ChangedFileRecord {
                kind: ChangeKind::Modified,
                path: "src/lib.rs".to_owned(),
                previous_path: None,
                binary: false,
            }],
            "diff --git a/src/lib.rs b/src/lib.rs\n+one line\n",
            46,
            true,
        )
    }

    #[test]
    fn every_stage_prompt_asks_for_the_bottom_line_section() {
        let prompt = compose(
            &request(Role::Researcher, StageKind::Research),
            &[],
            None,
            None,
        )
        .unwrap();
        assert!(prompt.contains(stage_prompt::BOTTOM_LINE));
    }

    #[test]
    fn editing_stages_are_asked_for_the_pull_request_and_reviews_are_not() {
        for kind in [
            StageKind::Implementation,
            StageKind::Fix,
            StageKind::FollowUp,
        ] {
            let prompt = compose(&request(Role::Implementer, kind), &[], None, None).unwrap();
            assert!(prompt.contains(stage_prompt::PULL_REQUEST), "{kind:?}");
        }
        let review = compose(
            &request(Role::SpecReviewer, StageKind::SpecReview),
            &[],
            None,
            None,
        )
        .unwrap();
        assert!(!review.contains(stage_prompt::PULL_REQUEST));
    }

    #[test]
    fn reviewer_prompt_embeds_shared_change_handoff_verbatim_and_grows_by_it() {
        let handoff = handoff();
        let request = request(Role::SpecReviewer, StageKind::SpecReview);
        let without = compose(&request, &[], None, None).unwrap();
        let with = compose(&request, &[], Some(&handoff), None).unwrap();
        let section = change_handoff::render(&handoff);
        assert!(!without.contains("# Implementation change map"));
        assert!(with.contains(&section), "section must embed verbatim");
        assert_eq!(with.len(), without.len() + section.len());
    }

    /// opencode has no stdin/file alternative to argv for the prompt, so an
    /// oversized change map is shortened to opencode's tighter ceiling rather
    /// than allowed to grow the argv without bound.
    #[test]
    fn a_change_map_larger_than_the_prompt_ceiling_is_shortened_to_fit() {
        let diff_text = "+padding line of diff text to overflow the input\n".repeat(25_000);
        let total = diff_text.len() as u64;
        let giant = ChangeHandoff::for_tests(
            &"b".repeat(40),
            vec![ChangedFileRecord {
                kind: ChangeKind::Modified,
                path: "src/lib.rs".to_owned(),
                previous_path: None,
                binary: false,
            }],
            &diff_text,
            total,
            true,
        );
        assert!(change_handoff::render(&giant).len() > MAX_PROMPT_BYTES);
        let request = request(Role::SpecReviewer, StageKind::SpecReview);
        let prompt = compose(&request, &[], Some(&giant), None).unwrap();
        assert!(prompt.len() <= MAX_PROMPT_BYTES);
        assert!(prompt.contains("Completeness: INCOMPLETE"));
    }

    #[test]
    fn continuation_prompt_stays_compact_without_change_handoff() {
        let text = continuation(&request(
            Role::CodeQualityReviewer,
            StageKind::CodeQualityReview,
        ));
        assert!(!text.contains("# Implementation change map"));
    }

    #[test]
    fn the_operators_continue_instruction_is_embedded_verbatim_when_present() {
        let request = request(Role::Implementer, StageKind::FollowUp);
        let without = compose(&request, &[], None, None).unwrap();
        let with = compose(&request, &[], None, Some("add integration tests too")).unwrap();
        assert!(!without.contains("# Operator instruction"));
        assert!(with.contains("# Operator instruction"));
        assert!(with.contains("add integration tests too"));
    }

    #[test]
    fn an_oversized_operator_instruction_is_truncated_with_an_explicit_incomplete_marker() {
        let instruction = "x".repeat(MAX_PROMPT_BYTES + 10_000);
        let request = request(Role::Implementer, StageKind::FollowUp);
        let prompt = compose(&request, &[], None, Some(&instruction)).unwrap();
        assert!(prompt.len() <= MAX_PROMPT_BYTES, "prompt: {}", prompt.len());
        assert!(prompt.contains("Completeness: INCOMPLETE"));
        assert!(!prompt.contains(&instruction));
    }

    #[test]
    fn no_room_left_omits_the_operator_instruction_section_entirely() {
        assert_eq!(continue_instruction_within("add tests", 0), "");
    }
}
