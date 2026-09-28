use crate::domain::{Role, StageKind};
use crate::engine::ProviderRequest;

use super::ArtifactRecord;

pub(crate) fn direct_dependency_artifacts<'a>(
    request: &ProviderRequest,
    artifacts: &'a [ArtifactRecord],
) -> Vec<&'a ArtifactRecord> {
    artifacts
        .iter()
        .filter(|artifact| {
            request
                .dependency_stage_ids()
                .iter()
                .any(|id| id == artifact.metadata().stage_id())
        })
        .collect()
}

/// Provider-neutral opening contract, identical for every role.
///
/// The control room shows one line of a finished stage before anyone opens
/// the artifact. That line is quoted, never inferred: the agent that did the
/// work is the only party that may state what the work concluded.
pub(crate) const BOTTOM_LINE: &str = "Open the returned Markdown with a `## Bottom line` section, before every other section including any title-adjacent scope note. Write at most two sentences, at most forty words total, in plain conversational language a tired reviewer reads in one glance: the verdict, and the single thing that matters most. No file paths, no symbol names, no counts, no hedging, and nothing the rest of the artifact does not already justify. Every other required section follows it, unchanged.";

/// Provider-neutral closing contract for every stage that edits the
/// workspace.
///
/// Publishing a run opens a pull request, and its title and description are
/// quoted from this section of the latest editing stage's artifact — never
/// composed by The Senate from the task text, which knows what was asked and
/// nothing about what was done. The agent that changed the code is the only
/// party that can describe the change; every editing stage restates it
/// because a fix or follow-up leaves the earlier description stale.
pub(crate) const PULL_REQUEST: &str = "Close the returned Markdown with a `## Pull request` section and put nothing after it; The Senate quotes it verbatim as the pull request when this run is published. Its first line is the title alone: imperative, specific, at most 72 characters, with no issue URL and no run identifier. Everything under that line is the description, written for a reviewer who has never opened this codebase and covering the whole change this workspace now carries against its base, not only this stage's part of it. If the task names an issue URL, open the description with `Fixes <url>` on its own line. Then `### Proposed changes`: one bold sentence saying what changed, followed by at most three one-line bullets. Then `### Why`: two or three plain sentences on the problem and what it cost, without walking through code. Then `### Testing`: the fewest numbered steps a human needs to see the change work once; automated checks do not belong there. Name a file, symbol, or flag only where a reviewer must search for it, paste no diffs, and add no checklists, footers, or rationale the diff already shows.";

/// Provider-neutral semantic contract for one engineering responsibility.
/// Native adapters add transport and safety framing around this text.
/// Provider-neutral note appended to an initial prompt only when the stage's
/// role holds an image-generation grant. It states the tool, its bound and
/// its limits in the agent's own terms; the tool schema carries the rest.
/// Every other stage's prompt is byte-identical to before the tool existed.
#[must_use]
pub(crate) fn image_tool_section(tool_name: &str, max_generations: u32, remaining: u32) -> String {
    format!(
        "\n# Image generation\nThis run authorizes you to generate original PNG images with the `{tool_name}` tool (an MCP tool The Senate provides). Use it only when the task genuinely benefits from a new image asset, and place the file where the project keeps assets. At most {max_generations} generations are allowed in this run and {remaining} remain; each call returns how many are left. Existing files are never overwritten. If a call fails you receive a typed error: continue the task without the image and say so in your report. The image is an ordinary file in the worktree; describe it in your report but do not claim anyone has visually reviewed it.\n"
    )
}

/// The lead of a mission, answering one message from the user. The
/// message arrives as the operator instruction below the contract; it
/// opens with the mission brief (goal, packages and their state,
/// decisions, what needs the user), which is the canonical state and not
/// a chat log. The lead's earlier answer, when there is one, is among the
/// dependency artifacts.
pub(crate) const LEAD_TURN: &str = "You are the engineering lead of the mission described in the operator instruction below. Read the brief first: it is the current plan, and it is the truth; your earlier answer, if attached, is history. Read the repository when the question needs it; you may not edit anything. Answer the user's message in plain conversational language, as a lead talking to the person who owns the project: what you recommend and why, tradeoffs named, no ceremony. When the message calls for changing the plan, close with a `## Plan changes` section in exactly this grammar and nothing else in it, one change per `- ` item, fields indented two spaces under it:\n\n- add `<id>`: <Title>\n  goal: <one sentence>\n  why: <one sentence>\n  accept: <one criterion>  (repeat the line per criterion)\n  workflow: fast|standard|deep|review\n  depends on: <id>, <id>\n- revise `<id>`\n  <goal|why|scope|accept|verify|workflow|depends on>: <new value>\n- cancel `<id>`: <reason>\n- decide: <Title>\n  why: <rationale>\n\nIds are lowercase words joined by hyphens or underscores. Only add a package that is a coherent, bounded piece of work one agent run can deliver; only name dependencies that must be in the checkout first. When nothing should change, write the section with the single item `- none`. When the message needs no plan change and asks none, omit the section. Never restate the brief and never invent state the brief does not show.";

pub(crate) const fn instruction(role: Role, kind: StageKind) -> &'static str {
    match (role, kind) {
        (Role::CodeQualityReviewer, StageKind::CodeQualityReview) => {
            "Inspect the actual repository code and relevant diff, not only an Implementation artifact when one exists. Judge HOW the implementation is engineered: simplicity, readability, maintainability, naming, module boundaries, error handling, hidden side effects, coupling, avoidable nesting and control-flow complexity, unnecessary abstraction, speculative generality, brittle tests, tests that pass by construction, dead code, duplicated concepts, convoluted comments, and implementation-level regressions. Do not repeat a complete requirement or specification review. Do not edit files. Return Markdown with # Code Quality Review and optional ## Must fix, ## Minor, and ## Good sections; do not invent findings."
        }
        (Role::SpecReviewer, StageKind::SpecReview) => {
            "Inspect the actual repository code and relevant diff against the immutable user task and available architecture or design evidence. Judge WHAT behavior was delivered. Classify findings as Missing, Wrong, or Unrequested. Treat RunInput as primary intent; architecture refines implementation but cannot silently override it. Treat code and repository state as observed result, Implementation artifacts as supporting context, repository-native authoritative specifications as additional evidence, and tests as evidence rather than product truth. Useful unrequested behavior remains a scope issue. Do not spend the review on style unless it causes a requirement failure. Do not edit files. Return Markdown with # Specification Review and optional ## Missing, ## Wrong, ## Unrequested, and ## Good sections; do not invent findings."
        }
        (Role::Researcher, _) => "Inspect repository and gather evidence. Do not invent facts.",
        (Role::Architect, _) => {
            "Design the smallest coherent change and name its constraints and tradeoffs. Write the design so an implementer can execute it without re-deriving it. Return Markdown with # Architecture and these sections. ## Plan: numbered steps, one per file, each naming the file, the symbols to add or change, and what in that file must not change. ## Verification: the exact commands that prove the change, as this repository runs them. ## Out of scope: what the change deliberately leaves alone. ## Assumptions: the facts about the repository the plan depends on, each checkable by reading one place, such as a file existing, a function having a signature, or a test covering a path; state none you did not check yourself."
        }
        (Role::Implementer, StageKind::Fix) => {
            "Resolve the blocking findings in the decision that sent this work back, and only those. The implementation already exists in this workspace; correct it rather than rewriting it, and leave findings the decision did not treat as blocking alone. Where you disagree with a finding, say so in the artifact and leave the code as it is rather than silently declining. Run proportionate verification. Return Markdown with # Fix and one section per finding stating what changed or why nothing did."
        }
        (Role::Implementer, StageKind::FollowUp) => {
            "Continue the work in this workspace as the operator instructs below. The previous decision artifact is context, not a boundary. Run proportionate verification. Return Markdown with # Follow-up and one section per instruction item stating what changed."
        }
        (Role::Implementer, _) => {
            "Implement the requested change and run proportionate verification. When an architecture artifact is among your dependencies, confirm its ## Assumptions against the repository before editing. Record any that do not hold under a ## Plan deviations section, each with the smallest correct reading you took instead, and follow the plan everywhere else."
        }
        (Role::Simplifier, StageKind::Simplification) => {
            "Simplify the implementation change in this workspace by removing accidental complexity, and touch nothing outside the changed lines and what they directly force. Reduce, never improve: delete comments that restate code, inline abstractions with one caller, remove speculative generality, dead branches, needless defensive layers, and configuration nothing requests. Preserve observable behavior exactly; when a simplification would change behavior, leave the code alone and note it. Run proportionate verification after editing. Return Markdown with # Simplification and one section per simplification stating what was removed and why, or a single ## No changes section when the implementation is already minimal; never invent work to justify the stage."
        }
        (Role::Simplifier, _) => {
            "Simplify the existing change in this workspace without altering observable behavior. Reduce, never improve. Run proportionate verification."
        }
        (Role::CodeQualityReviewer, _) => {
            "Assess implementation quality independently. Inspect actual repository state and do not edit files."
        }
        (Role::SpecReviewer, _) => {
            "Assess delivered behavior against immutable user intent independently. Inspect actual repository state and do not edit files."
        }
        (Role::Reviewer, _) => {
            "Perform legacy/general independent review. Prioritize correctness, regressions, and missing tests. Do not edit files."
        }
        (Role::EngineeringLead, StageKind::Decision) => {
            "Synthesize direct review evidence across two distinct axes: implementation quality and specification compliance. Do not count findings mechanically or infer approval from reviewer completion. Surface disagreements between reviewers and make an explicit engineering decision. When a previous decision and a fix answering it are both in evidence, judge whether that fix actually resolves the findings the previous decision called blocking; a fix artifact claiming a finding is resolved is a claim to verify against the code, not a resolution. When you see reasonable next steps that are not blocking findings — follow-on work, generalizations, or things worth doing but not required by this task — add an optional `## Follow-ups` section, one bullet per item, written as an instruction an operator could hand back to an agent verbatim. Omit the section entirely when there is nothing worth suggesting; never pad it to have something to say."
        }
        (Role::EngineeringLead, StageKind::Lead) => LEAD_TURN,
        (Role::EngineeringLead, _) => {
            "Integrate direct dependency evidence into one actionable engineering result."
        }
        // Never sent anywhere: the verifier is the deterministic `verify`
        // provider, which runs commands rather than reading a contract.
        (Role::Verifier, _) => {
            "Run the repository's verification commands and report every exit code."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specialized_reviews_have_distinct_contracts() {
        let quality = instruction(Role::CodeQualityReviewer, StageKind::CodeQualityReview);
        let spec = instruction(Role::SpecReviewer, StageKind::SpecReview);

        assert!(quality.contains("HOW"));
        assert!(quality.contains("actual repository code and relevant diff"));
        assert!(quality.contains("Do not repeat a complete requirement"));
        assert!(!quality.contains("Classify findings as Missing, Wrong, or Unrequested"));
        assert!(spec.contains("WHAT"));
        assert!(spec.contains("Missing, Wrong, or Unrequested"));
        assert!(spec.contains("tests as evidence rather than product truth"));
        assert_ne!(quality, spec);
    }

    /// The simplifier reduces the existing change; it is neither a second
    /// implementation pass nor a review, and it may not grow the work or
    /// change what it does.
    #[test]
    fn the_simplification_contract_reduces_within_the_delta_without_changing_behavior() {
        let simplification = instruction(Role::Simplifier, StageKind::Simplification);
        let implementation = instruction(Role::Implementer, StageKind::Implementation);
        assert_ne!(simplification, implementation);
        assert!(simplification.contains("Reduce, never improve"));
        assert!(simplification.contains("touch nothing outside the changed lines"));
        assert!(simplification.contains("Preserve observable behavior exactly"));
        // An already-minimal change is a valid outcome, never a failure to
        // find work.
        assert!(simplification.contains("## No changes"));
        assert!(simplification.contains("never invent work"));
    }

    /// A fix answers a verdict; it is not a second implementation pass.
    #[test]
    fn the_fix_contract_is_bounded_by_the_decision_it_answers() {
        let fix = instruction(Role::Implementer, StageKind::Fix);
        let implementation = instruction(Role::Implementer, StageKind::Implementation);
        assert_ne!(fix, implementation);
        assert!(fix.contains("blocking findings"));
        assert!(fix.contains("and only those"));
        assert!(
            fix.contains("correct it rather than rewriting it"),
            "the work already exists in the workspace"
        );
        // Disagreeing with a finding is allowed; silently ignoring it is not.
        assert!(fix.contains("say so in the artifact"));
    }

    /// Unlike a fix, a follow-up carries no findings of its own to bound it —
    /// the operator's instruction is the whole scope, and the contract must
    /// say the prior decision is context rather than a limit on it.
    #[test]
    fn the_follow_up_contract_treats_the_operators_instruction_as_the_scope() {
        let follow_up = instruction(Role::Implementer, StageKind::FollowUp);
        let fix = instruction(Role::Implementer, StageKind::Fix);
        let implementation = instruction(Role::Implementer, StageKind::Implementation);
        assert_ne!(follow_up, fix);
        assert_ne!(follow_up, implementation);
        assert!(follow_up.contains("as the operator instructs below"));
        assert!(follow_up.contains("context, not a boundary"));
        assert!(follow_up.contains("# Follow-up"));
    }

    /// The architect's output is what makes lower-effort execution safe: a
    /// plan an implementer can follow without re-deriving it, verification
    /// it can run, a fence it can respect, and assumptions it can check.
    #[test]
    fn the_architecture_contract_asks_for_an_executable_plan() {
        let architecture = instruction(Role::Architect, StageKind::Architecture);
        for section in [
            "## Plan",
            "## Verification",
            "## Out of scope",
            "## Assumptions",
        ] {
            assert!(architecture.contains(section), "{section}");
        }
        assert!(architecture.contains("one per file"));
        assert!(architecture.contains("exact commands"));
        assert!(architecture.contains("checkable by reading one place"));
        assert!(
            architecture.contains("state none you did not check yourself"),
            "an unchecked assumption is a guess dressed as a fact"
        );
        // The original charge survives the added shape.
        assert!(architecture.contains("smallest coherent change"));
        assert!(architecture.contains("constraints and tradeoffs"));
    }

    /// The implementer's first act is a check, not a judgment: the plan's
    /// assumptions are confirmed before any edit, and a miss is recorded
    /// rather than silently improvised around. Fix and follow-up keep their
    /// own bounded contracts.
    #[test]
    fn the_implementation_contract_checks_the_plans_assumptions_before_editing() {
        let implementation = instruction(Role::Implementer, StageKind::Implementation);
        assert!(implementation.contains("## Assumptions"));
        assert!(implementation.contains("before editing"));
        assert!(implementation.contains("## Plan deviations"));
        assert!(implementation.contains("follow the plan everywhere else"));
        assert!(implementation.contains("proportionate verification"));
        assert!(!instruction(Role::Implementer, StageKind::Fix).contains("## Plan deviations"));
        assert!(
            !instruction(Role::Implementer, StageKind::FollowUp).contains("## Plan deviations")
        );
    }

    #[test]
    fn a_decision_over_a_fix_verifies_it_against_the_code() {
        let decision = instruction(Role::EngineeringLead, StageKind::Decision);
        assert!(decision.contains("previous decision and a fix"));
        assert!(
            decision.contains("claim to verify against the code"),
            "a fix saying it fixed something is not evidence that it did"
        );
    }

    /// The control room quotes this section instead of summarizing prose it
    /// cannot judge, so the contract has to name the heading it will look for
    /// and bound what may be written under it.
    #[test]
    fn bottom_line_contract_names_the_section_and_bounds_it() {
        assert!(BOTTOM_LINE.contains("## Bottom line"));
        assert!(BOTTOM_LINE.contains("two sentences"));
        assert!(BOTTOM_LINE.contains("forty words"));
        assert!(BOTTOM_LINE.contains("plain conversational language"));
        assert!(
            BOTTOM_LINE.contains("nothing the rest of the artifact does not already justify"),
            "a headline is a quote of the work, never an addition to it"
        );
    }

    /// Publish quotes this section verbatim, so the contract has to name the
    /// heading, put the title where the reader looks first, and ask for the
    /// sections a reviewer expects to find.
    #[test]
    fn pull_request_contract_names_the_section_and_its_shape() {
        assert!(PULL_REQUEST.contains("## Pull request"));
        assert!(PULL_REQUEST.contains("put nothing after it"));
        assert!(PULL_REQUEST.contains("first line is the title alone"));
        assert!(PULL_REQUEST.contains("72 characters"));
        for section in ["### Proposed changes", "### Why", "### Testing"] {
            assert!(PULL_REQUEST.contains(section), "{section}");
        }
        assert!(PULL_REQUEST.contains("Fixes <url>"));
        assert!(
            PULL_REQUEST.contains("whole change this workspace now carries"),
            "a fix restates the pull request for the branch, not for its own part"
        );
        assert!(PULL_REQUEST.contains("no checklists, footers"));
    }

    #[test]
    fn decision_contract_synthesizes_both_review_axes() {
        let decision = instruction(Role::EngineeringLead, StageKind::Decision);
        assert!(decision.contains("implementation quality"));
        assert!(decision.contains("specification compliance"));
        assert!(decision.contains("Surface disagreements"));
    }

    /// The follow-ups section is a suggestion, not a verdict: it must read as
    /// optional and non-blocking, and it must ask for the exact heading the
    /// TUI's extraction later looks for.
    #[test]
    fn decision_contract_asks_for_an_optional_non_blocking_follow_ups_section() {
        let decision = instruction(Role::EngineeringLead, StageKind::Decision);
        assert!(decision.contains("## Follow-ups"));
        assert!(decision.contains("optional"));
        assert!(decision.contains("not blocking findings"));
        assert!(
            decision.contains("Omit the section entirely when there is nothing worth suggesting")
        );
    }
}
