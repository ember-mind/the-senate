//! Read-only observation of the Codex CLI's own session record.
//!
//! `codex exec --json` never names the model or the reasoning effort it
//! resolved: its stream carries `thread.started`, item records and one
//! `turn.completed` usage total, and nothing else. When The Senate routes a role
//! to `codex` without pinning a model — the recommended profile does exactly
//! that, so the runtime's own configuration keeps deciding — the identity of
//! what actually ran exists in only one place: the rollout file Codex writes
//! for its own session, keyed by the same thread ID the stream announced.
//!
//! The Senate reads that file and never writes it. Every byte in it is data
//! produced by another program: the thread ID is validated before it reaches a
//! path, the search is depth- and breadth-bounded, only a bounded prefix of
//! the file is parsed, and any failure at all leaves the facts unobserved
//! rather than guessed. An unobserved model stays `None`, which the evidence
//! surfaces read as "unconfirmed" — never as agreement with what was
//! configured.

use std::path::{Path, PathBuf};

use crate::domain::ModelId;

/// Deepest directory nesting searched under `<codex home>/sessions`.
/// Codex partitions by year/month/day, so three levels is its layout and the
/// fourth is slack for a future one.
const MAX_SEARCH_DEPTH: usize = 4;
/// Ceiling on directory entries visited by one lookup.
const MAX_ENTRIES_VISITED: usize = 20_000;
/// Ceiling on bytes read from one rollout file. The record that names the
/// model is written before the conversation starts, so the prefix is enough
/// and a long session never costs a long read.
const MAX_PREFIX_BYTES: usize = 256 * 1024;

/// What the runtime's own session record says it ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ObservedRuntime {
    pub model: Option<ModelId>,
    /// The runtime's native reasoning-effort value, verbatim. The Senate does
    /// not map it onto its own [`crate::domain::EffortSetting`]: this is what
    /// the runtime chose, not what The Senate requested.
    pub effort: Option<String>,
}

impl ObservedRuntime {
    pub(crate) const fn is_empty(&self) -> bool {
        self.model.is_none() && self.effort.is_none()
    }
}

/// Reads what Codex recorded for one thread, or nothing.
///
/// Returns `None` when the home is absent, the thread ID is not a plausible
/// session identity, no rollout matches it, or the file names neither fact.
pub(crate) fn observe(codex_home: &Path, thread_id: &str) -> Option<ObservedRuntime> {
    let file = rollout_path(codex_home, thread_id)?;
    let observed = parse_prefix(&file)?;
    (!observed.is_empty()).then_some(observed)
}

/// The Codex home this process should read, from `CODEX_HOME` or the default
/// under the user's home directory. Resolved once by the adapter so no later
/// code consults the environment mid-run.
pub(crate) fn home_from_environment() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("CODEX_HOME") {
        let path = PathBuf::from(explicit);
        return (!path.as_os_str().is_empty()).then_some(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .map(|home| home.join(".codex"))
}

/// Whether a runtime-supplied thread ID may be used to build a path.
///
/// The ID arrives from another program's stdout. Restricting it to the shape
/// Codex actually emits keeps it from reaching the filesystem as anything but
/// a leaf name component.
pub(crate) fn is_plausible_thread_id(thread_id: &str) -> bool {
    !thread_id.is_empty()
        && thread_id.len() <= 64
        && thread_id != "."
        && thread_id != ".."
        && thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn rollout_path(codex_home: &Path, thread_id: &str) -> Option<PathBuf> {
    if !is_plausible_thread_id(thread_id) {
        return None;
    }
    let suffix = format!("-{thread_id}.jsonl");
    let mut visited = 0_usize;
    let mut frontier = vec![(codex_home.join("sessions"), 0_usize)];
    while let Some((directory, depth)) = frontier.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_ENTRIES_VISITED {
                return None;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("rollout-") && name.ends_with(&suffix) {
                return Some(path);
            }
            if depth + 1 < MAX_SEARCH_DEPTH && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                frontier.push((path, depth + 1));
            }
        }
    }
    None
}

fn parse_prefix(file: &Path) -> Option<ObservedRuntime> {
    use std::io::Read as _;

    let mut prefix = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(MAX_PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)
        .ok()?;
    let text = String::from_utf8_lossy(&prefix);
    let mut observed = ObservedRuntime::default();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            // A truncated final line is expected while the session is live.
            continue;
        };
        if record.get("type").and_then(serde_json::Value::as_str) != Some("turn_context") {
            continue;
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        observed.model = payload
            .get("model")
            .and_then(serde_json::Value::as_str)
            .and_then(|model| ModelId::new(model).ok());
        observed.effort = payload
            .get("effort")
            .and_then(serde_json::Value::as_str)
            .filter(|effort| !effort.is_empty())
            .map(ToOwned::to_owned);
        if !observed.is_empty() {
            return Some(observed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{ObservedRuntime, is_plausible_thread_id, observe};

    /// Writes a rollout in Codex's own layout. The records are the real
    /// shapes: `session_meta` never names the model, `turn_context` does.
    fn rollout(home: &std::path::Path, thread_id: &str, body: &str) {
        let directory = home.join("sessions/2026/08/29");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("rollout-2026-08-29T18-53-38-{thread_id}.jsonl")),
            body,
        )
        .unwrap();
    }

    const THREAD: &str = "01a04e70-ec47-78a2-a0cf-4989498a3a8c";

    fn real_shape() -> String {
        [
            r#"{"timestamp":"2026-08-29T16:53:38.924Z","ordinal":0,"type":"session_meta","payload":{"session_id":"01a04e70-ec47-78a2-a0cf-4989498a3a8c","cli_version":"0.149.0","model_provider":"openai"}}"#,
            r#"{"timestamp":"2026-08-29T16:53:38.926Z","ordinal":1,"type":"event_msg","payload":{"type":"task_started","model_context_window":258400}}"#,
            r#"{"timestamp":"2026-08-29T16:53:41.500Z","ordinal":5,"type":"turn_context","payload":{"model":"gpt-5.6-luna","effort":"xhigh","summary":"auto"}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn the_model_and_effort_come_from_the_runtimes_own_record() {
        let home = tempfile::tempdir().unwrap();
        rollout(home.path(), THREAD, &real_shape());

        let observed = observe(home.path(), THREAD).expect("the rollout names both facts");
        assert_eq!(
            observed.model.as_ref().map(ToString::to_string).as_deref(),
            Some("gpt-5.6-luna")
        );
        // Verbatim runtime value, not remapped onto The Senate's own levels.
        assert_eq!(observed.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn nothing_is_observed_when_the_record_is_absent_wrong_or_silent() {
        let home = tempfile::tempdir().unwrap();

        // No sessions directory at all.
        assert_eq!(observe(home.path(), THREAD), None);

        // A rollout for a different thread is never borrowed from.
        rollout(
            home.path(),
            "0000000-dead-beef-0000-000000000000",
            &real_shape(),
        );
        assert_eq!(observe(home.path(), THREAD), None);

        // A session that recorded no turn context leaves both facts absent
        // rather than defaulting either of them.
        rollout(
            home.path(),
            THREAD,
            r#"{"timestamp":"2026-08-29T16:53:38.924Z","ordinal":0,"type":"session_meta","payload":{"cli_version":"0.149.0"}}"#,
        );
        assert_eq!(observe(home.path(), THREAD), None);
    }

    #[test]
    fn a_live_session_with_a_half_written_final_line_still_reads() {
        let home = tempfile::tempdir().unwrap();
        rollout(
            home.path(),
            THREAD,
            &format!("{}\n{{\"timestamp\":\"2026-08-2", real_shape()),
        );
        let observed = observe(home.path(), THREAD).expect("the complete records still parse");
        assert_eq!(observed.effort.as_deref(), Some("xhigh"));
    }

    /// The thread ID is another program's output, so two separate things
    /// keep it harmless, and this proves each of them.
    ///
    /// Structurally it never becomes a path component at all: it is matched
    /// against file names inside `<home>/sessions`, so a traversal string
    /// cannot reach outside that tree even if the guard were gone. On top of
    /// that, an ID that is not the shape Codex emits is refused before the
    /// search starts, so a file that happens to match one is still not read.
    #[test]
    fn a_thread_id_is_matched_as_a_name_inside_sessions_and_refused_when_implausible() {
        assert!(is_plausible_thread_id(THREAD));
        // Stage-derived thread identities carry underscores, so the check has
        // to admit them without admitting a path separator.
        assert!(is_plausible_thread_id("codex-thread-quality_review"));
        for hostile in [
            "../../../../etc/passwd",
            "..",
            ".",
            "a/b",
            "a\\b",
            "",
            "id with spaces",
            "id\0nul",
        ] {
            assert!(
                !is_plausible_thread_id(hostile),
                "{hostile} must be refused"
            );
        }

        let home = tempfile::tempdir().unwrap();
        rollout(home.path(), THREAD, &real_shape());

        // A rollout sitting outside the sessions tree is never reached, which
        // is what makes the ID structurally unable to escape.
        std::fs::write(
            home.path()
                .join("rollout-2026-08-29T18-53-38-escaped.jsonl"),
            real_shape(),
        )
        .unwrap();
        assert_eq!(observe(home.path(), "escaped"), None);

        // And an implausible ID is refused even when a file inside the tree
        // would have matched the suffix built from it.
        let implausible = "id with spaces";
        let directory = home.path().join("sessions/2026/08/29");
        std::fs::write(
            directory.join(format!("rollout-2026-08-29T18-53-38-{implausible}.jsonl")),
            real_shape(),
        )
        .unwrap();
        assert_eq!(observe(home.path(), implausible), None);
    }

    #[test]
    fn an_observation_with_neither_fact_is_not_an_observation() {
        assert!(ObservedRuntime::default().is_empty());
    }
}
