//! Deterministic bounded implementation-change handoff for review stages.
//!
//! The handoff is navigation evidence derived from the managed worktree
//! relative to the immutable persisted run base — the exact delta semantics
//! used by apply and diff preview. It is not an authored artifact and never
//! replaces independent inspection of the real worktree.

use std::fmt::Write as _;

use crate::domain::Role;
use crate::engine::ProviderRequest;
use crate::git::{ChangedFileRecord, Git, generate_change_evidence};
use crate::store::SqliteStore;
use crate::workspace::WorkspaceStatus;

/// Maximum injected diff bytes. Aligned with the existing 1 MiB per-block
/// dependency-artifact injection cap; unlike immutable artifacts (which fail
/// closed on oversize), an oversized diff degrades to explicit bounded partial
/// evidence because reviewers retain the worktree as source of truth.
pub(crate) const MAX_CHANGE_HANDOFF_DIFF_BYTES: usize = 1024 * 1024;

/// Maximum changed files listed individually; the remainder is reported as an
/// explicit count so the inventory can never silently look complete.
pub(crate) const MAX_CHANGE_HANDOFF_LISTED_FILES: usize = 200;

/// Deterministic change evidence handed to stages that judge implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangeHandoff {
    base_commit: String,
    files: Vec<ChangedFileRecord>,
    diff_text: String,
    total_diff_bytes: u64,
    diff_complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ChangeHandoffError {
    #[error("run workspace is missing; change handoff cannot be derived")]
    WorkspaceMissing,
    #[error("run workspace is {status} and cannot provide deterministic change evidence")]
    WorkspaceNotReady { status: &'static str },
    #[error("provider request worktree does not match persisted run workspace")]
    WorktreeMismatch,
    #[error(transparent)]
    Git(#[from] crate::git::GitError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// Stages whose responsibility is judging or reshaping the implementation
/// receive change evidence. Researcher/Architect run before implementation
/// exists; Implementer authored the change; EngineeringLead/Decision already
/// receives both review artifacts as direct dependency evidence, so injecting
/// the diff again would duplicate context without a demonstrated benefit. The
/// Simplifier receives it because the run delta is the boundary of what it may
/// touch.
#[must_use]
pub(crate) const fn stage_receives(role: Role) -> bool {
    matches!(
        role,
        Role::Simplifier | Role::CodeQualityReviewer | Role::SpecReviewer | Role::Reviewer
    )
}

/// Derives the handoff for one initial provider invocation, or `None` when the
/// stage does not receive implementation-change evidence.
///
/// Restart-safe: everything is derived from the persisted run base commit and
/// the managed worktree; no in-memory prompt state, source branch, or external
/// repository state participates. Derivation is read-only — the delta is
/// staged in an ephemeral Git index and the real worktree index is untouched.
pub(crate) fn for_request(
    store: &SqliteStore,
    request: &ProviderRequest,
) -> Result<Option<ChangeHandoff>, ChangeHandoffError> {
    if !stage_receives(request.role()) {
        return Ok(None);
    }
    let workspace = store
        .load_workspace(request.run_id())?
        .ok_or(ChangeHandoffError::WorkspaceMissing)?;
    if workspace.status() != WorkspaceStatus::Ready {
        return Err(ChangeHandoffError::WorkspaceNotReady {
            status: workspace.status().as_str(),
        });
    }
    if workspace.worktree_path() != request.workspace_path() {
        return Err(ChangeHandoffError::WorktreeMismatch);
    }
    let evidence = generate_change_evidence(
        &Git::default(),
        workspace.worktree_path(),
        workspace.base_commit(),
        MAX_CHANGE_HANDOFF_DIFF_BYTES,
    )?;
    Ok(Some(ChangeHandoff {
        base_commit: workspace.base_commit().to_owned(),
        files: evidence.files,
        diff_text: String::from_utf8_lossy(&evidence.diff.bytes).into_owned(),
        total_diff_bytes: evidence.diff.total_bytes,
        diff_complete: !evidence.diff.truncated,
    }))
}

#[cfg(test)]
impl ChangeHandoff {
    pub(crate) fn for_tests(
        base_commit: &str,
        files: Vec<ChangedFileRecord>,
        diff_text: &str,
        total_diff_bytes: u64,
        diff_complete: bool,
    ) -> Self {
        Self {
            base_commit: base_commit.to_owned(),
            files,
            diff_text: diff_text.to_owned(),
            total_diff_bytes,
            diff_complete,
        }
    }
}

/// Renders the section so it fits inside `max_bytes`, shedding diff lines
/// first. The inventory and the completeness verdict always survive: a
/// provider with a hard input limit loses navigation detail, never the
/// knowledge that detail is missing. Bytes bound characters from above, so a
/// byte budget is safe against a character-counted limit.
#[must_use]
pub(crate) fn render_within(handoff: &ChangeHandoff, max_bytes: usize) -> String {
    let section = render(handoff);
    if section.len() <= max_bytes {
        return section;
    }
    let mut bounded = handoff.clone();
    bounded.diff_complete = false;
    // Overhead is measured on the truncated shape itself — including the diff
    // header a non-empty diff brings back — so neither it nor the longer
    // INCOMPLETE completeness line can push the result past the budget.
    let overhead = {
        let mut probe = bounded.clone();
        "\n".clone_into(&mut probe.diff_text);
        render(&probe).len()
    };
    // A few spare bytes absorb the count digits the INCOMPLETE line grows by.
    let mut room = max_bytes
        .saturating_sub(overhead + 32)
        .min(bounded.diff_text.len());
    while room > 0 && !bounded.diff_text.is_char_boundary(room) {
        room -= 1;
    }
    let keep = bounded.diff_text[..room]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    bounded.diff_text.truncate(keep);
    bounded.diff_complete = false;
    render(&bounded)
}

/// Renders the provider-neutral prompt section. Byte-identical across
/// adapters for the same handoff.
#[must_use]
pub(crate) fn render(handoff: &ChangeHandoff) -> String {
    let mut section = String::new();
    section.push_str("\n# Implementation change map\n");
    let _ = writeln!(
        section,
        "Deterministic change evidence: managed worktree relative to immutable run base {} (same delta semantics as apply). This map is a navigation aid only; the worktree is the source of truth — inspect real files as needed. Binary contents are never included.",
        handoff.base_commit
    );
    if handoff.files.is_empty() {
        section.push_str("\nChanged files: none detected relative to the run base.\n");
    } else {
        let _ = writeln!(section, "\nChanged files ({} total):", handoff.files.len());
        for file in handoff.files.iter().take(MAX_CHANGE_HANDOFF_LISTED_FILES) {
            let _ = write!(section, "  {} {}", file.kind.label(), file.path);
            if let Some(previous) = &file.previous_path {
                let _ = write!(section, " (from {previous})");
            }
            if file.binary {
                section.push_str(" [binary]");
            }
            section.push('\n');
        }
        if handoff.files.len() > MAX_CHANGE_HANDOFF_LISTED_FILES {
            let _ = writeln!(
                section,
                "  ... file list truncated: only the first {} of {} changed files are listed above.",
                MAX_CHANGE_HANDOFF_LISTED_FILES,
                handoff.files.len()
            );
        }
    }
    if !handoff.diff_text.is_empty() {
        section
            .push_str("\nDiff (unified; binary changes appear as \"Binary files ... differ\"):\n");
        section.push_str(&handoff.diff_text);
        if !handoff.diff_text.ends_with('\n') {
            section.push('\n');
        }
    }
    if handoff.diff_complete && handoff.files.len() <= MAX_CHANGE_HANDOFF_LISTED_FILES {
        section
            .push_str("\nCompleteness: complete — this change map covers the entire run delta.\n");
    } else {
        let _ = writeln!(
            section,
            "\nCompleteness: INCOMPLETE — the change exceeds the deterministic handoff bound ({} of {} diff bytes shown). Do not treat this map as the full change; inspect the managed worktree for anything beyond it.",
            handoff.diff_text.len(),
            handoff.total_diff_bytes
        );
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{ChangeKind, PatchPreview};

    fn handoff(files: Vec<ChangedFileRecord>, diff: &PatchPreview) -> ChangeHandoff {
        ChangeHandoff {
            base_commit: "a".repeat(40),
            files,
            diff_text: String::from_utf8_lossy(&diff.bytes).into_owned(),
            total_diff_bytes: diff.total_bytes,
            diff_complete: !diff.truncated,
        }
    }

    fn file(kind: ChangeKind, path: &str, binary: bool) -> ChangedFileRecord {
        ChangedFileRecord {
            kind,
            path: path.to_owned(),
            previous_path: None,
            binary,
        }
    }

    /// A budget below the section's size sheds diff lines but keeps the
    /// inventory and turns the completeness verdict INCOMPLETE; an ample
    /// budget changes nothing.
    #[test]
    fn render_within_sheds_diff_lines_and_keeps_the_verdict_honest() {
        let diff_text = "diff --git a/x b/x\n".repeat(400);
        let preview = PatchPreview {
            bytes: diff_text.clone().into_bytes(),
            total_bytes: diff_text.len() as u64,
            truncated: false,
        };
        let full = handoff(vec![file(ChangeKind::Modified, "x", false)], &preview);
        let complete = render(&full);
        assert_eq!(render_within(&full, complete.len()), complete);

        let bounded = render_within(&full, 2000);
        assert!(bounded.len() <= 2000);
        assert!(bounded.contains("Changed files (1 total):"));
        assert!(bounded.contains("Completeness: INCOMPLETE"));
        // What diff survives ends on a whole line, never mid-hunk.
        let diff_part = bounded
            .split("differ\"):\n")
            .nth(1)
            .unwrap_or("")
            .split("\nCompleteness")
            .next()
            .unwrap_or("");
        assert!(diff_part.is_empty() || diff_part.ends_with('\n'));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "single end-to-end fixture proving store-derived handoff policy"
    )]
    fn derives_handoff_from_persisted_workspace_for_reviewers_only() {
        use std::path::{Path, PathBuf};
        use std::process::Command;

        use serde_json::json;
        use tempfile::TempDir;

        use crate::domain::{
            ConfigSnapshotId, EventId, EventMetadata, ProviderSessionId, Run, RunId, StageId,
            StageKind, StageStatus, WorkflowDefinition, WorkflowKind,
        };
        use crate::store::ResolvedConfigSnapshot;
        use crate::workspace::WorkspaceManager;

        fn git(path: &Path, args: &[&str]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn request(
            run_id: RunId,
            role: Role,
            kind: StageKind,
            worktree: PathBuf,
        ) -> ProviderRequest {
            ProviderRequest::new(
                run_id,
                StageId::new("stage").unwrap(),
                kind,
                StageStatus::Ready,
                role,
                "task".to_owned(),
                worktree,
                1,
                0,
                Option::<ProviderSessionId>::None,
                vec![],
            )
        }

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "The Senate Test"]);
        git(&source, &["config", "user.email", "senate@example.invalid"]);
        std::fs::write(
            source.join("README.md"),
            "fixture
",
        )
        .unwrap();
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "fixture"]);

        let database = temp.path().join("senate.sqlite3");
        let mut store = SqliteStore::open(&database).unwrap();
        let run_id = RunId::from_u128(77);
        let created_at: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
        let config_id = ConfigSnapshotId::new("config-handoff").unwrap();
        let run = Run::new(
            run_id,
            WorkflowDefinition::built_in(WorkflowKind::Standard),
            config_id.clone(),
            created_at,
        );
        let config =
            ResolvedConfigSnapshot::new(config_id, 1, json!({"provider": "fake"}), created_at)
                .unwrap();
        let created = run.created_event(EventMetadata::new(EventId::from_u128(78), created_at));
        store.create_run(&run, &config, &[created]).unwrap();
        let workspace = WorkspaceManager::new(temp.path().join("worktrees"))
            .prepare_run_workspace(&mut store, run_id, &source)
            .unwrap();

        // Simulated implementation output inside the managed worktree.
        std::fs::write(
            workspace.worktree_path().join("README.md"),
            "changed
",
        )
        .unwrap();
        std::fs::write(
            workspace.worktree_path().join("new_file.rs"),
            "fn new() {}
",
        )
        .unwrap();

        let quality = for_request(
            &store,
            &request(
                run_id,
                Role::CodeQualityReviewer,
                StageKind::CodeQualityReview,
                workspace.worktree_path().to_path_buf(),
            ),
        )
        .unwrap()
        .expect("reviewer receives handoff");
        let spec = for_request(
            &store,
            &request(
                run_id,
                Role::SpecReviewer,
                StageKind::SpecReview,
                workspace.worktree_path().to_path_buf(),
            ),
        )
        .unwrap()
        .expect("spec reviewer receives handoff");

        // Shared factual evidence only: both reviewers derive identical maps.
        assert_eq!(quality, spec);
        assert_eq!(quality.base_commit, workspace.base_commit());
        let mut paths: Vec<&str> = quality
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["README.md", "new_file.rs"]);
        assert!(quality.diff_complete);
        assert!(quality.diff_text.contains("+changed"));
        assert!(quality.diff_text.contains("+fn new() {}"));

        // Non-review stages never receive implementation change evidence.
        for (role, kind) in [
            (Role::Researcher, StageKind::Research),
            (Role::Architect, StageKind::Architecture),
            (Role::Implementer, StageKind::Implementation),
            (Role::EngineeringLead, StageKind::Decision),
        ] {
            assert!(
                for_request(
                    &store,
                    &request(run_id, role, kind, workspace.worktree_path().to_path_buf()),
                )
                .unwrap()
                .is_none(),
                "{role:?} must not receive handoff"
            );
        }

        // Stale request worktree fails closed instead of producing wrong evidence.
        let mismatch = for_request(
            &store,
            &request(
                run_id,
                Role::SpecReviewer,
                StageKind::SpecReview,
                temp.path().join("elsewhere"),
            ),
        );
        assert!(matches!(
            mismatch,
            Err(ChangeHandoffError::WorktreeMismatch)
        ));
    }

    #[test]
    fn only_implementation_judging_roles_receive_handoff() {
        assert!(stage_receives(Role::Simplifier));
        assert!(stage_receives(Role::CodeQualityReviewer));
        assert!(stage_receives(Role::SpecReviewer));
        assert!(stage_receives(Role::Reviewer));
        assert!(!stage_receives(Role::Researcher));
        assert!(!stage_receives(Role::Architect));
        assert!(!stage_receives(Role::Implementer));
        assert!(!stage_receives(Role::EngineeringLead));
    }

    #[test]
    fn complete_handoff_renders_explicit_complete_marker() {
        let rendered = render(&handoff(
            vec![
                file(ChangeKind::Modified, "src/a.rs", false),
                file(ChangeKind::Added, "src/new.rs", false),
            ],
            &PatchPreview {
                bytes: b"diff --git a/src/a.rs b/src/a.rs\n".to_vec(),
                total_bytes: 34,
                truncated: false,
            },
        ));
        assert!(rendered.contains("# Implementation change map"));
        assert!(rendered.contains("Changed files (2 total):"));
        assert!(rendered.contains("modified src/a.rs"));
        assert!(rendered.contains("added src/new.rs"));
        assert!(rendered.contains("Completeness: complete"));
        assert!(!rendered.contains("INCOMPLETE"));
        assert!(rendered.contains("navigation aid only"));
    }

    #[test]
    fn truncated_diff_is_explicitly_marked_incomplete_never_silently() {
        let rendered = render(&handoff(
            vec![file(ChangeKind::Modified, "src/big.rs", false)],
            &PatchPreview {
                bytes: b"diff --git a/src/big.rs b".to_vec(),
                total_bytes: 5_000_000,
                truncated: true,
            },
        ));
        assert!(rendered.contains("Completeness: INCOMPLETE"));
        assert!(rendered.contains("25 of 5000000 diff bytes shown"));
        assert!(rendered.contains("inspect the managed worktree"));
        assert!(!rendered.contains("Completeness: complete —"));
    }

    #[test]
    fn oversized_file_list_is_bounded_with_explicit_remainder() {
        let files = (0..MAX_CHANGE_HANDOFF_LISTED_FILES + 5)
            .map(|index| file(ChangeKind::Added, &format!("src/f{index}.rs"), false))
            .collect();
        let rendered = render(&handoff(
            files,
            &PatchPreview {
                bytes: Vec::new(),
                total_bytes: 0,
                truncated: false,
            },
        ));
        assert!(rendered.contains("file list truncated: only the first 200 of 205"));
        assert!(rendered.contains("Completeness: INCOMPLETE"));
    }

    #[test]
    fn binary_change_is_identified_without_content() {
        let rendered = render(&handoff(
            vec![file(ChangeKind::Modified, "assets/logo.png", true)],
            &PatchPreview {
                bytes: b"Binary files a/assets/logo.png and b/assets/logo.png differ\n".to_vec(),
                total_bytes: 60,
                truncated: false,
            },
        ));
        assert!(rendered.contains("modified assets/logo.png [binary]"));
        assert!(!rendered.contains("GIT binary patch"));
    }

    #[test]
    fn empty_delta_renders_deterministic_no_change_evidence() {
        let rendered = render(&handoff(
            Vec::new(),
            &PatchPreview {
                bytes: Vec::new(),
                total_bytes: 0,
                truncated: false,
            },
        ));
        assert!(rendered.contains("Changed files: none detected relative to the run base."));
        assert!(rendered.contains("Completeness: complete"));
    }
}
