use std::fmt::Write as _;

use crate::engine::ProviderRequest;
use crate::providers::change_handoff::ChangeHandoff;
use crate::providers::{ArtifactRecord, change_handoff, stage_prompt};

use super::CodexProviderError;

const MAX_DEPENDENCY_BYTES: u64 = 1024 * 1024;

/// Codex rejects a turn whose input exceeds 1,048,576 characters
/// (`input_too_large`, code -32602). The margin leaves room for whatever the
/// runtime wraps around stdin. Bytes bound characters from above, so budgeting
/// in bytes can only undershoot the character limit.
const MAX_INPUT_BYTES: usize = 1024 * 1024 - 16 * 1024;

/// Heading the operator-instruction section always opens with, shared by its
/// renderer and the tests that compute the same overhead it does.
const OPERATOR_INSTRUCTION_HEADER: &str = "\n# Operator instruction\n";

pub(crate) fn compose(
    request: &ProviderRequest,
    artifacts: &[ArtifactRecord],
    handoff: Option<&ChangeHandoff>,
    continue_instruction: Option<&str>,
) -> Result<String, CodexProviderError> {
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
        "You are executing one stage for The Senate. Work only inside current managed worktree. Respect repository instructions, AGENTS.md, rules, skills, MCP configuration, and native Codex configuration discovered normally. Do not apply changes to another checkout. Do not invoke The Senate apply. Do not commit or push. Return concise Markdown describing result, evidence, and unresolved risks."
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
            return Err(CodexProviderError::ArtifactTooLarge(
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
    // The operator's own instruction for a continue cycle's follow-up stage,
    // carried here through the same immutable run-private stdin path an
    // attention response uses — never argv, never a domain event. Appended
    // after dependency artifacts (a follow-up's own dependency is the prior
    // decision, which can approach the per-artifact cap on its own) and
    // budgeted against whatever room is left, the same discipline the change
    // handoff below already follows: never silently dropped, and never
    // allowed to push the turn over Codex's hard input ceiling. Truncated is
    // still acceptable — it survives marked INCOMPLETE — but a FollowUp
    // stage exists to carry this instruction, so when there is no room for
    // even that marked, truncated form, the stage must fail rather than run
    // the mutating stage under the generic follow-up contract with the
    // operator's actual scope never delivered.
    if let Some(instruction) = continue_instruction {
        let room = MAX_INPUT_BYTES.saturating_sub(prompt.len());
        let section = continue_instruction_within(instruction, room);
        if section.is_empty() {
            return Err(CodexProviderError::ContinueInstructionOmitted(
                request.stage_id().clone(),
            ));
        }
        prompt.push_str(&section);
    }
    if let Some(handoff) = handoff {
        // The change map is the one part that may legitimately dwarf the
        // input limit, and it is navigation aid, not source of truth — so it
        // is the part that yields whatever room the rest of the prompt left.
        let room = MAX_INPUT_BYTES.saturating_sub(prompt.len());
        prompt.push_str(&change_handoff::render_within(handoff, room));
    }
    Ok(prompt)
}

/// Renders the operator-instruction section so it fits inside `max_bytes`,
/// truncating the instruction text itself rather than silently dropping the
/// section or letting the turn exceed Codex's hard input ceiling. Mirrors
/// `change_handoff::render_within`'s bounded-partial-evidence pattern: the
/// heading and an explicit INCOMPLETE marker survive whenever they
/// themselves fit, because a provider with a hard limit should lose
/// instruction detail, never the knowledge that detail is missing — but when
/// `max_bytes` is too small even for the header and marker alone (the rest
/// of the prompt left no room, or none at all), the whole section is omitted
/// instead of appending a fragment that would itself exceed the budget.
fn continue_instruction_within(instruction: &str, max_bytes: usize) -> String {
    let full = format!("{OPERATOR_INSTRUCTION_HEADER}{instruction}\n");
    if full.len() <= max_bytes {
        return full;
    }
    // Overhead is measured on the full-length marker and padded a little
    // further, so the marker's own digit count shrinking as `shown` drops
    // below `instruction.len()` can never push the result past the budget.
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
        "\nCompleteness: INCOMPLETE — the operator's instruction exceeds Codex's input limit here ({shown} of {total} instruction bytes shown). Treat this as a partial instruction; the rest was not delivered.\n"
    )
}

pub(crate) fn continuation(request: &ProviderRequest) -> String {
    format!(
        "Continue exact interrupted stage {} for The Senate in same native Codex thread. Finish assigned {:?} work in current managed worktree. {} Do not commit, push, or apply changes to another checkout. Return final Markdown result.",
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

    /// Only a stage that changed the code can describe the change, so every
    /// editing stage is asked for the pull request and no read-only stage is.
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
        for (role, kind) in [
            (Role::SpecReviewer, StageKind::SpecReview),
            (Role::EngineeringLead, StageKind::Decision),
            (Role::Researcher, StageKind::Research),
        ] {
            let prompt = compose(&request(role, kind), &[], None, None).unwrap();
            assert!(!prompt.contains(stage_prompt::PULL_REQUEST), "{kind:?}");
        }
        // Simplification edits the change; it must be told to edit, not to
        // report only, and asked to describe the result like any editor.
        let simplification = compose(
            &request(Role::Simplifier, StageKind::Simplification),
            &[],
            None,
            None,
        )
        .unwrap();
        assert!(simplification.contains("Make required changes in managed worktree"));
        assert!(!simplification.contains("Do not modify repository content"));
        assert!(simplification.contains(stage_prompt::PULL_REQUEST));
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
        // Same shared render() as the Claude adapter: semantic identity is the
        // single provider-neutral section, byte-for-byte.
    }

    /// Codex rejects any turn over its input limit outright, so a change map
    /// bigger than the limit must arrive shortened, never verbatim — the run
    /// died on `input_too_large` in the wild.
    #[test]
    fn a_change_map_larger_than_the_input_limit_is_shortened_to_fit() {
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
        assert!(change_handoff::render(&giant).len() > MAX_INPUT_BYTES);
        let request = request(Role::SpecReviewer, StageKind::SpecReview);
        let prompt = compose(&request, &[], Some(&giant), None).unwrap();

        assert!(prompt.len() <= MAX_INPUT_BYTES);
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

    /// See the Claude adapter's equivalent test: the operator's instruction
    /// must be embedded verbatim for a follow-up stage and absent otherwise.
    #[test]
    fn the_operators_continue_instruction_is_embedded_verbatim_when_present() {
        let request = request(Role::Implementer, StageKind::FollowUp);
        let without = compose(&request, &[], None, None).unwrap();
        let with = compose(&request, &[], None, Some("add integration tests too")).unwrap();

        assert!(!without.contains("# Operator instruction"));
        assert!(with.contains("# Operator instruction"));
        assert!(with.contains("add integration tests too"));
    }

    /// Codex rejects any turn over its input limit outright, same as a giant
    /// change map: an oversized operator instruction must arrive shortened
    /// with an explicit marker, never verbatim and never by erroring the
    /// stage outright.
    #[test]
    fn an_oversized_operator_instruction_is_truncated_with_an_explicit_incomplete_marker() {
        let instruction = "x".repeat(MAX_INPUT_BYTES + 10_000);
        let request = request(Role::Implementer, StageKind::FollowUp);

        let prompt = compose(&request, &[], None, Some(&instruction)).unwrap();

        assert!(prompt.len() <= MAX_INPUT_BYTES, "prompt: {}", prompt.len());
        assert!(prompt.contains("Completeness: INCOMPLETE"));
        assert!(
            !prompt.contains(&instruction),
            "the full instruction must not survive verbatim"
        );
    }

    /// The reviewed scenario: a follow-up's own dependency (the prior
    /// decision) can sit close to its own per-artifact cap on its own, and
    /// the operator's instruction is appended after it — the combination
    /// must still fit, by shortening the instruction, rather than exceeding
    /// Codex's hard input ceiling.
    #[test]
    fn a_large_dependency_plus_a_large_instruction_still_fit_the_input_limit() {
        use chrono::{TimeZone, Utc};

        use crate::domain::{ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactStatus};
        use crate::providers::ArtifactRecord;

        let temp = tempfile::TempDir::new().unwrap();
        let decision_id = StageId::new("decision").unwrap();
        let content = "d".repeat(990 * 1024);
        let path = temp.path().join("decision.md");
        std::fs::write(&path, &content).unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 8, 21, 0, 0, 0).single().unwrap();
        let metadata = ArtifactMetadata::new(
            ArtifactId::new(),
            RunId::from_u128(2),
            decision_id.clone(),
            ArtifactKind::Decision,
            Role::EngineeringLead,
            ArtifactStatus::Complete,
            created_at,
        );
        let artifact = ArtifactRecord::new(
            metadata,
            1,
            path,
            "a".repeat(64),
            content.len() as u64,
            created_at,
        )
        .unwrap();
        let request = ProviderRequest::new(
            RunId::from_u128(2),
            StageId::new("followup_1").unwrap(),
            StageKind::FollowUp,
            StageStatus::Ready,
            Role::Implementer,
            "immutable task".to_owned(),
            PathBuf::from("/managed/worktree"),
            1,
            0,
            Option::<ProviderSessionId>::None,
            vec![decision_id],
        );
        let instruction = "y".repeat(64 * 1024);

        let prompt = compose(&request, &[artifact], None, Some(&instruction)).unwrap();

        assert!(prompt.len() <= MAX_INPUT_BYTES, "prompt: {}", prompt.len());
        assert!(prompt.contains(&content), "the dependency stays intact");
        assert!(
            prompt.contains("Completeness: INCOMPLETE"),
            "the instruction had to give way, and says so"
        );
    }

    /// Degenerate case the reviewed bug missed: when the rest of the prompt
    /// left exactly zero room, the section used to still append its header
    /// and completeness marker regardless, pushing the composed prompt past
    /// `MAX_INPUT_BYTES`. Zero room must instead mean no section at all.
    #[test]
    fn no_room_left_omits_the_operator_instruction_section_entirely() {
        assert_eq!(continue_instruction_within("add tests", 0), "");
    }

    /// One byte short of what the header and completeness marker alone
    /// require (with even a zero-byte excerpt) must still omit the section
    /// rather than append a fragment that overflows `max_bytes`.
    #[test]
    fn room_one_byte_short_of_header_and_marker_omits_the_section() {
        // Long enough that `max_bytes` below is always short of the
        // instruction's full rendered length, forcing the truncation path
        // regardless of the (much larger) overhead computed from it.
        let instruction = "x".repeat(10_000);
        let overhead = OPERATOR_INSTRUCTION_HEADER.len()
            + incomplete_marker(instruction.len(), instruction.len()).len()
            + 32;

        let section = continue_instruction_within(&instruction, overhead - 1);

        assert_eq!(section, "", "section: {section:?}");
    }

    /// Exactly enough room for the header and marker (zero-byte excerpt) must
    /// render the degenerate section, and it must still fit.
    #[test]
    fn room_exactly_at_the_overhead_boundary_renders_a_degenerate_section_that_fits() {
        let instruction = "x".repeat(10_000);
        let overhead = OPERATOR_INSTRUCTION_HEADER.len()
            + incomplete_marker(instruction.len(), instruction.len()).len()
            + 32;

        let section = continue_instruction_within(&instruction, overhead);

        assert!(section.len() <= overhead, "section: {}", section.len());
        assert!(section.contains("Completeness: INCOMPLETE"));
    }

    /// End-to-end boundary check at the actual `MAX_INPUT_BYTES` ceiling: a
    /// dependency artifact is sized (computed from the prompt's own fixed
    /// scaffolding, not guessed) so it leaves the operator instruction
    /// section exactly zero bytes of room. Silently composing without the
    /// instruction would let the follow-up stage run unscoped, so `compose`
    /// must fail closed instead — the stage fails rather than an agent
    /// making mutating changes without the operator's actual instruction.
    #[test]
    fn zero_room_for_the_operator_instruction_fails_closed_instead_of_composing_unscoped() {
        use chrono::{TimeZone, Utc};

        use crate::domain::{ArtifactId, ArtifactKind, ArtifactMetadata, ArtifactStatus};
        use crate::providers::ArtifactRecord;

        let temp = tempfile::TempDir::new().unwrap();
        let decision_id = StageId::new("decision").unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 8, 21, 0, 0, 0).single().unwrap();
        let request = ProviderRequest::new(
            RunId::from_u128(2),
            StageId::new("followup_1").unwrap(),
            StageKind::FollowUp,
            StageStatus::Ready,
            Role::Implementer,
            "immutable task".to_owned(),
            PathBuf::from("/managed/worktree"),
            1,
            0,
            Option::<ProviderSessionId>::None,
            vec![decision_id.clone()],
        );
        let instruction = "please also add a regression test";

        let build_artifact = |content: &str| {
            let path = temp.path().join("decision.md");
            std::fs::write(&path, content).unwrap();
            let metadata = ArtifactMetadata::new(
                ArtifactId::new(),
                RunId::from_u128(2),
                decision_id.clone(),
                ArtifactKind::Decision,
                Role::EngineeringLead,
                ArtifactStatus::Complete,
                created_at,
            );
            ArtifactRecord::new(
                metadata,
                1,
                path,
                "a".repeat(64),
                content.len() as u64,
                created_at,
            )
            .unwrap()
        };

        // The dependency-writing code adds exactly `content.len()` bytes to
        // the composed prompt for any content, so an empty-content probe
        // measures every other fixed byte the prompt carries before the
        // instruction section is appended.
        let baseline_len = compose(&request, &[build_artifact("")], None, None)
            .unwrap()
            .len();
        let content_len = MAX_INPUT_BYTES - baseline_len;
        assert!(
            (content_len as u64) < MAX_DEPENDENCY_BYTES,
            "test assumption: the sized dependency must stay under its own cap"
        );
        let artifact = build_artifact(&"d".repeat(content_len));

        let error = compose(&request, &[artifact], None, Some(instruction)).unwrap_err();

        assert!(
            matches!(
                error,
                CodexProviderError::ContinueInstructionOmitted(ref stage_id)
                    if *stage_id == StageId::new("followup_1").unwrap()
            ),
            "error: {error}"
        );
    }
}
