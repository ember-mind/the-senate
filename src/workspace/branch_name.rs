//! Human-readable branch names for run workspaces.
//!
//! `senate/run-01J...` says nothing on `git branch`. A run created from a
//! Linear or GitHub issue is named after that issue (`senate/dotcom-17972`);
//! anything else takes the opening words of its task. A short tail of the
//! run's ULID keeps two runs on the same issue from colliding.

use crate::domain::RunId;

/// Longest slug taken from free-form task text.
const MAX_TASK_SLUG: usize = 40;
/// How many trailing characters of the run ULID disambiguate the branch.
const ID_TAIL: usize = 6;

/// The branch a workspace should own for `run_id`, described by `task` when
/// the run has one.
#[must_use]
pub fn branch_name(run_id: RunId, task: Option<&str>) -> String {
    let id = run_id.to_string().to_ascii_lowercase();
    let tail = &id[id.len().saturating_sub(ID_TAIL)..];
    match task.and_then(slug_for) {
        Some(slug) => format!("senate/{slug}-{tail}"),
        None => format!("senate/run-{id}"),
    }
}

/// Issue key when the task references one, otherwise a slug of its first words.
fn slug_for(task: &str) -> Option<String> {
    issue_key(task).or_else(|| words_slug(task))
}

/// `DOTCOM-17972` from a Linear URL (`linear.app/<org>/issue/<KEY>/...`), a
/// GitHub issue or PR URL (`github.com/<owner>/<repo>/issues/<n>` becomes
/// `<repo>-<n>`), or a bare `KEY-123` token anywhere in the task.
fn issue_key(task: &str) -> Option<String> {
    for token in task.split(|c: char| {
        c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | ',' | '"' | '\'')
    }) {
        let token = token.trim_end_matches(['.', ':', ';', '!', '?']);
        if let Some(rest) = token.split_once("linear.app/").map(|(_, rest)| rest) {
            let mut parts = rest.split('/');
            let _org = parts.next();
            if parts.next() == Some("issue")
                && let Some(key) = parts.next().filter(|key| is_bare_key(key))
            {
                return Some(key.to_ascii_lowercase());
            }
        }
        if let Some(rest) = token.split_once("github.com/").map(|(_, rest)| rest) {
            let parts: Vec<&str> = rest.split('/').collect();
            if let [_owner, repo, kind, number, ..] = parts.as_slice()
                && matches!(*kind, "issues" | "pull")
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
            {
                return Some(format!("{}-{number}", repo.to_ascii_lowercase()));
            }
        }
    }
    task.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .find(|token| is_bare_key(token))
        .map(str::to_ascii_lowercase)
}

/// `ABC-123`: two or more uppercase letters, a dash, digits.
fn is_bare_key(token: &str) -> bool {
    let Some((prefix, number)) = token.split_once('-') else {
        return false;
    };
    prefix.len() >= 2
        && prefix.chars().all(|c| c.is_ascii_uppercase())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

/// Lowercase ASCII words of the task joined by dashes, cut at a word boundary
/// before `MAX_TASK_SLUG`. URLs are skipped so a bare link does not become
/// `https-example-com`.
fn words_slug(task: &str) -> Option<String> {
    let mut slug = String::new();
    for word in task.split_whitespace().filter(|word| !word.contains("://")) {
        let cleaned: String = word
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if cleaned.is_empty() {
            continue;
        }
        let next_len = slug.len() + cleaned.len() + usize::from(!slug.is_empty());
        if next_len > MAX_TASK_SLUG {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&cleaned);
    }
    (!slug.is_empty()).then_some(slug)
}

#[cfg(test)]
mod tests {
    use super::{branch_name, slug_for};
    use crate::domain::RunId;

    fn run() -> RunId {
        "01J8ZK2M7Q3V4X5Y6Z7A8B9CDE".parse().unwrap()
    }

    #[test]
    fn a_linear_url_names_the_branch_after_its_issue() {
        let task = "Fix https://linear.app/a8c/issue/DOTCOM-17972/stepper-transfer-waits please";
        assert_eq!(branch_name(run(), Some(task)), "senate/dotcom-17972-8b9cde");
    }

    #[test]
    fn a_bare_issue_key_anywhere_wins_over_the_words() {
        assert_eq!(
            slug_for("Stepper waits (DOTCOM-17972)").unwrap(),
            "dotcom-17972"
        );
        assert_eq!(slug_for("see DOTSUP-9.").unwrap(), "dotsup-9");
    }

    #[test]
    fn a_github_issue_or_pull_url_names_the_repo_and_number() {
        assert_eq!(
            slug_for("https://github.com/Automattic/wp-calypso/issues/1234").unwrap(),
            "wp-calypso-1234"
        );
        assert_eq!(
            slug_for("review https://github.com/o/Repo/pull/7/files").unwrap(),
            "repo-7"
        );
    }

    #[test]
    fn free_text_becomes_a_bounded_word_slug() {
        assert_eq!(
            slug_for("Replace the scripted messages with the honest stepper transfer waits")
                .unwrap(),
            "replace-the-scripted-messages-with-the"
        );
        assert_eq!(
            slug_for("Fix   Ünïcode & punctuation!!").unwrap(),
            "fix-ncode-punctuation"
        );
        assert_eq!(slug_for("https://example.com/only-a-link"), None);
        assert_eq!(slug_for("!!! ???"), None);
    }

    #[test]
    fn a_hyphenated_lowercase_word_is_not_mistaken_for_an_issue_key() {
        assert_eq!(slug_for("re-2 things").unwrap(), "re2-things");
        assert_eq!(slug_for("A-1 sauce").unwrap(), "a1-sauce");
    }

    #[test]
    fn without_a_task_the_run_id_still_names_the_branch() {
        assert_eq!(
            branch_name(run(), None),
            "senate/run-01j8zk2m7q3v4x5y6z7a8b9cde"
        );
        assert_eq!(
            branch_name(run(), Some(" \n ")),
            "senate/run-01j8zk2m7q3v4x5y6z7a8b9cde"
        );
    }
}
