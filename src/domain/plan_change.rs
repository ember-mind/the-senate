//! Plan changes: the fixed shape a lead uses to propose changes to a
//! mission's plan, and the parser that turns that text into operations.
//!
//! The lead answers in prose, then closes with a `## Plan changes` section
//! in this grammar, one item per change, fields indented under it:
//!
//! ```text
//! ## Plan changes
//!
//! - add `persistence`: Persistence
//!   goal: persist every character between sessions
//!   why: memory needs a durable substrate
//!   accept: a character survives a restart
//!   workflow: standard
//!   depends on: schema
//! - revise `memory`
//!   goal: characters remember what happened to them
//! - cancel `telemetry`: out of scope for the first release
//! - decide: Persist before memory
//!   why: memory needs a durable substrate
//! - none
//! ```
//!
//! Nothing here touches the mission: parsing yields [`PlanChange`]s, the
//! application layer shows them and applies the ones the user confirms.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::ids::{IdError, WorkPackageId};
use super::mission::WorkPackageContract;
use super::workflow::WorkflowKind;

/// The heading the lead's section must carry, matched case-insensitively.
pub const PLAN_CHANGES_HEADING: &str = "## Plan changes";

/// One proposed change to a mission's plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanChange {
    AddPackage {
        id: WorkPackageId,
        contract: WorkPackageContract,
        dependencies: Vec<WorkPackageId>,
    },
    /// Only the fields given change; `dependencies` replaces the list when
    /// present.
    RevisePackage {
        id: WorkPackageId,
        title: Option<String>,
        goal: Option<String>,
        rationale: Option<String>,
        scope: Option<String>,
        acceptance_criteria: Option<Vec<String>>,
        verification: Option<String>,
        workflow: Option<WorkflowKind>,
        dependencies: Option<Vec<WorkPackageId>>,
    },
    CancelPackage {
        id: WorkPackageId,
        reason: String,
    },
    RecordDecision {
        title: String,
        rationale: String,
    },
}

impl PlanChange {
    /// One line saying what applying this change would do, for the
    /// confirmation the user sees before anything moves.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::AddPackage {
                id,
                contract,
                dependencies,
            } => {
                let deps = if dependencies.is_empty() {
                    String::new()
                } else {
                    format!(" (after {})", join_ids(dependencies))
                };
                format!(
                    "add {id}: {} — {}{deps}",
                    contract.title,
                    first_line(&contract.goal)
                )
            }
            Self::RevisePackage {
                id,
                title,
                goal,
                rationale,
                scope,
                acceptance_criteria,
                verification,
                workflow,
                dependencies,
            } => {
                let mut fields = Vec::new();
                if title.is_some() {
                    fields.push("title");
                }
                if goal.is_some() {
                    fields.push("goal");
                }
                if rationale.is_some() {
                    fields.push("why");
                }
                if scope.is_some() {
                    fields.push("scope");
                }
                if acceptance_criteria.is_some() {
                    fields.push("acceptance");
                }
                if verification.is_some() {
                    fields.push("verification");
                }
                if workflow.is_some() {
                    fields.push("workflow");
                }
                if dependencies.is_some() {
                    fields.push("dependencies");
                }
                format!("revise {id}: {}", fields.join(", "))
            }
            Self::CancelPackage { id, reason } => format!("cancel {id}: {reason}"),
            Self::RecordDecision { title, rationale } => {
                format!("decide: {title} — {}", first_line(rationale))
            }
        }
    }
}

fn join_ids(ids: &[WorkPackageId]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Why a `## Plan changes` section could not be read. Every variant names
/// the line, so the lead can be told exactly what to fix.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanChangeParseError {
    #[error("the answer has no `{PLAN_CHANGES_HEADING}` section")]
    MissingSection,
    #[error(
        "line {line}: expected `- add`, `- revise`, `- cancel`, `- decide` or `- none`, found {text:?}"
    )]
    UnknownItem { line: usize, text: String },
    #[error("line {line}: a field belongs under an item, found {text:?}")]
    FieldWithoutItem { line: usize, text: String },
    #[error("line {line}: a field is `key: value`, found {text:?}")]
    MalformedField { line: usize, text: String },
    #[error("line {line}: unknown field {field:?} for `{item}`")]
    UnknownField {
        line: usize,
        item: &'static str,
        field: String,
    },
    #[error("line {line}: `{item}` needs {field}")]
    MissingField {
        line: usize,
        item: &'static str,
        field: &'static str,
    },
    #[error("line {line}: invalid package id {id:?}: {source}")]
    InvalidId {
        line: usize,
        id: String,
        #[source]
        source: IdError,
    },
    #[error("line {line}: unknown workflow {workflow:?}; use fast, standard, deep or review")]
    UnknownWorkflow { line: usize, workflow: String },
    #[error("line {line}: `- none` cannot be combined with other changes")]
    NoneWithChanges { line: usize },
}

/// Parses the `## Plan changes` section out of a lead's whole answer.
///
/// # Errors
/// Returns the first thing wrong with the section, by line.
pub fn parse_plan_changes(answer: &str) -> Result<Vec<PlanChange>, PlanChangeParseError> {
    let (section, offset) = section(answer).ok_or(PlanChangeParseError::MissingSection)?;
    let section = section.as_str();
    let mut items: Vec<Item> = Vec::new();
    let mut none_at = None;
    for (index, raw) in section.lines().enumerate() {
        let line = offset + index + 1;
        let trimmed = raw.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ") {
            let head = rest.trim();
            if head.eq_ignore_ascii_case("none") {
                none_at = Some(line);
                continue;
            }
            items.push(Item::open(line, head)?);
            continue;
        }
        if trimmed.starts_with(' ') || trimmed.starts_with('\t') {
            let Some(item) = items.last_mut() else {
                return Err(PlanChangeParseError::FieldWithoutItem {
                    line,
                    text: trimmed.trim().to_owned(),
                });
            };
            item.field(line, trimmed.trim())?;
            continue;
        }
        return Err(PlanChangeParseError::UnknownItem {
            line,
            text: trimmed.to_owned(),
        });
    }
    if let Some(line) = none_at
        && !items.is_empty()
    {
        return Err(PlanChangeParseError::NoneWithChanges { line });
    }
    items.into_iter().map(Item::finish).collect()
}

/// The section's text and the number of lines before it, for line numbers
/// that refer to the whole answer.
fn section(answer: &str) -> Option<(String, usize)> {
    let lines: Vec<&str> = answer.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim().eq_ignore_ascii_case(PLAN_CHANGES_HEADING))?
        + 1;
    let end = lines[start..]
        .iter()
        .position(|line| line.trim_start().starts_with("## "))
        .map_or(lines.len(), |offset| start + offset);
    Some((lines[start..end].join("\n"), start))
}

#[derive(Debug)]
enum Item {
    Add {
        line: usize,
        id: WorkPackageId,
        title: String,
        goal: Option<String>,
        rationale: Option<String>,
        scope: Option<String>,
        accept: Vec<String>,
        verification: Option<String>,
        workflow: Option<WorkflowKind>,
        dependencies: Vec<WorkPackageId>,
    },
    Revise {
        line: usize,
        id: WorkPackageId,
        title: Option<String>,
        goal: Option<String>,
        rationale: Option<String>,
        scope: Option<String>,
        accept: Option<Vec<String>>,
        verification: Option<String>,
        workflow: Option<WorkflowKind>,
        dependencies: Option<Vec<WorkPackageId>>,
    },
    Cancel {
        id: WorkPackageId,
        reason: String,
    },
    Decide {
        line: usize,
        title: String,
        rationale: Option<String>,
    },
}

impl Item {
    fn open(line: usize, head: &str) -> Result<Self, PlanChangeParseError> {
        let (verb, rest) = head.split_once(char::is_whitespace).unwrap_or((head, ""));
        let (verb, rest) = if let Some(verb) = verb.strip_suffix(':') {
            (verb, rest)
        } else {
            (verb, rest)
        };
        let rest = rest.trim();
        match verb.to_ascii_lowercase().as_str() {
            "add" => {
                let (id, title) = id_and_text(line, rest)?;
                Ok(Self::Add {
                    line,
                    id,
                    title,
                    goal: None,
                    rationale: None,
                    scope: None,
                    accept: Vec::new(),
                    verification: None,
                    workflow: None,
                    dependencies: Vec::new(),
                })
            }
            "revise" => {
                let (id, _) = id_and_text(line, rest)?;
                Ok(Self::Revise {
                    line,
                    id,
                    title: None,
                    goal: None,
                    rationale: None,
                    scope: None,
                    accept: None,
                    verification: None,
                    workflow: None,
                    dependencies: None,
                })
            }
            "cancel" => {
                let (id, reason) = id_and_text(line, rest)?;
                if reason.is_empty() {
                    return Err(PlanChangeParseError::MissingField {
                        line,
                        item: "cancel",
                        field: "a reason after the id",
                    });
                }
                Ok(Self::Cancel { id, reason })
            }
            "decide" => {
                if rest.is_empty() {
                    return Err(PlanChangeParseError::MissingField {
                        line,
                        item: "decide",
                        field: "a title",
                    });
                }
                Ok(Self::Decide {
                    line,
                    title: rest.to_owned(),
                    rationale: None,
                })
            }
            _ => Err(PlanChangeParseError::UnknownItem {
                line,
                text: format!("- {head}"),
            }),
        }
    }

    fn field(&mut self, line: usize, text: &str) -> Result<(), PlanChangeParseError> {
        let Some((key, value)) = text.split_once(':') else {
            return Err(PlanChangeParseError::MalformedField {
                line,
                text: text.to_owned(),
            });
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        match self {
            Self::Add {
                goal,
                rationale,
                scope,
                accept,
                verification,
                workflow,
                dependencies,
                ..
            } => match key.as_str() {
                "goal" => *goal = Some(value),
                "why" | "rationale" => *rationale = Some(value),
                "scope" => *scope = Some(value),
                "accept" | "acceptance" => accept.push(value),
                "verify" | "verification" => *verification = Some(value),
                "workflow" => *workflow = Some(parse_workflow(line, &value)?),
                "depends on" | "depends" | "after" => *dependencies = parse_ids(line, &value)?,
                _ => {
                    return Err(PlanChangeParseError::UnknownField {
                        line,
                        item: "add",
                        field: key,
                    });
                }
            },
            Self::Revise {
                title,
                goal,
                rationale,
                scope,
                accept,
                verification,
                workflow,
                dependencies,
                ..
            } => match key.as_str() {
                "title" => *title = Some(value),
                "goal" => *goal = Some(value),
                "why" | "rationale" => *rationale = Some(value),
                "scope" => *scope = Some(value),
                "accept" | "acceptance" => accept.get_or_insert_with(Vec::new).push(value),
                "verify" | "verification" => *verification = Some(value),
                "workflow" => *workflow = Some(parse_workflow(line, &value)?),
                "depends on" | "depends" | "after" => {
                    *dependencies = Some(parse_ids(line, &value)?);
                }
                _ => {
                    return Err(PlanChangeParseError::UnknownField {
                        line,
                        item: "revise",
                        field: key,
                    });
                }
            },
            Self::Cancel { .. } => {
                return Err(PlanChangeParseError::UnknownField {
                    line,
                    item: "cancel",
                    field: key,
                });
            }
            Self::Decide { rationale, .. } => match key.as_str() {
                "why" | "rationale" => *rationale = Some(value),
                _ => {
                    return Err(PlanChangeParseError::UnknownField {
                        line,
                        item: "decide",
                        field: key,
                    });
                }
            },
        }
        Ok(())
    }

    fn finish(self) -> Result<PlanChange, PlanChangeParseError> {
        match self {
            Self::Add {
                line,
                id,
                title,
                goal,
                rationale,
                scope,
                accept,
                verification,
                workflow,
                dependencies,
            } => {
                let goal = goal.ok_or(PlanChangeParseError::MissingField {
                    line,
                    item: "add",
                    field: "a `goal:` field",
                })?;
                Ok(PlanChange::AddPackage {
                    id,
                    contract: WorkPackageContract {
                        title,
                        goal,
                        rationale: rationale.unwrap_or_default(),
                        scope: scope.unwrap_or_default(),
                        acceptance_criteria: accept,
                        verification: verification.unwrap_or_default(),
                        workflow: workflow.unwrap_or(WorkflowKind::Standard),
                    },
                    dependencies,
                })
            }
            Self::Revise {
                line,
                id,
                title,
                goal,
                rationale,
                scope,
                accept,
                verification,
                workflow,
                dependencies,
            } => {
                if title.is_none()
                    && goal.is_none()
                    && rationale.is_none()
                    && scope.is_none()
                    && accept.is_none()
                    && verification.is_none()
                    && workflow.is_none()
                    && dependencies.is_none()
                {
                    return Err(PlanChangeParseError::MissingField {
                        line,
                        item: "revise",
                        field: "at least one field to change",
                    });
                }
                Ok(PlanChange::RevisePackage {
                    id,
                    title,
                    goal,
                    rationale,
                    scope,
                    acceptance_criteria: accept,
                    verification,
                    workflow,
                    dependencies,
                })
            }
            Self::Cancel { id, reason } => Ok(PlanChange::CancelPackage { id, reason }),
            Self::Decide {
                line,
                title,
                rationale,
            } => Ok(PlanChange::RecordDecision {
                title,
                rationale: rationale.ok_or(PlanChangeParseError::MissingField {
                    line,
                    item: "decide",
                    field: "a `why:` field",
                })?,
            }),
        }
    }
}

/// `` `id`: text `` or `` `id` `` (backticks optional), with the text after
/// the id and its colon.
fn id_and_text(line: usize, rest: &str) -> Result<(WorkPackageId, String), PlanChangeParseError> {
    let rest = rest.trim();
    let (raw_id, text) = match rest.split_once(':') {
        Some((id, text)) => (id.trim(), text.trim()),
        None => (rest, ""),
    };
    let raw_id = raw_id.trim_matches('`').trim();
    if raw_id.is_empty() {
        return Err(PlanChangeParseError::MissingField {
            line,
            item: "add/revise/cancel",
            field: "a package id",
        });
    }
    let id = WorkPackageId::new(raw_id).map_err(|source| PlanChangeParseError::InvalidId {
        line,
        id: raw_id.to_owned(),
        source,
    })?;
    Ok((id, text.to_owned()))
}

fn parse_ids(line: usize, value: &str) -> Result<Vec<WorkPackageId>, PlanChangeParseError> {
    if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|raw| raw.trim().trim_matches('`').trim())
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            WorkPackageId::new(raw).map_err(|source| PlanChangeParseError::InvalidId {
                line,
                id: raw.to_owned(),
                source,
            })
        })
        .collect()
}

fn parse_workflow(line: usize, value: &str) -> Result<WorkflowKind, PlanChangeParseError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "fast" => Ok(WorkflowKind::Fast),
        "standard" => Ok(WorkflowKind::Standard),
        "deep" => Ok(WorkflowKind::Deep),
        "review" => Ok(WorkflowKind::Review),
        other => Err(PlanChangeParseError::UnknownWorkflow {
            line,
            workflow: other.to_owned(),
        }),
    }
}

impl fmt::Display for PlanChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> WorkPackageId {
        WorkPackageId::new(value).unwrap()
    }

    #[test]
    fn a_full_section_parses_into_every_kind_of_change() {
        let answer = "\
Some prose first.

## Bottom line

Two packages, one decision.

## Plan changes

- add `persistence`: Persistence
  goal: persist every character between sessions
  why: memory needs a durable substrate
  accept: a character survives a restart
  accept: nothing is lost on crash
  workflow: standard
  depends on: schema, `events`
- revise `memory`
  goal: characters remember what happened to them
  depends on: persistence
- cancel `telemetry`: out of scope for the first release
- decide: Persist before memory
  why: memory needs a durable substrate

## Something after

Ignored.
";
        let changes = parse_plan_changes(answer).unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(
            changes[0],
            PlanChange::AddPackage {
                id: id("persistence"),
                contract: WorkPackageContract {
                    title: "Persistence".to_owned(),
                    goal: "persist every character between sessions".to_owned(),
                    rationale: "memory needs a durable substrate".to_owned(),
                    scope: String::new(),
                    acceptance_criteria: vec![
                        "a character survives a restart".to_owned(),
                        "nothing is lost on crash".to_owned(),
                    ],
                    verification: String::new(),
                    workflow: WorkflowKind::Standard,
                },
                dependencies: vec![id("schema"), id("events")],
            }
        );
        assert_eq!(
            changes[1],
            PlanChange::RevisePackage {
                id: id("memory"),
                title: None,
                goal: Some("characters remember what happened to them".to_owned()),
                rationale: None,
                scope: None,
                acceptance_criteria: None,
                verification: None,
                workflow: None,
                dependencies: Some(vec![id("persistence")]),
            }
        );
        assert_eq!(
            changes[2],
            PlanChange::CancelPackage {
                id: id("telemetry"),
                reason: "out of scope for the first release".to_owned(),
            }
        );
        assert_eq!(
            changes[3],
            PlanChange::RecordDecision {
                title: "Persist before memory".to_owned(),
                rationale: "memory needs a durable substrate".to_owned(),
            }
        );
        assert_eq!(
            changes[0].describe(),
            "add persistence: Persistence — persist every character between sessions (after schema, events)"
        );
        assert_eq!(changes[1].describe(), "revise memory: goal, dependencies");
    }

    #[test]
    fn none_means_no_changes_and_a_missing_section_is_an_error() {
        assert_eq!(
            parse_plan_changes("## Plan changes\n\n- none\n").unwrap(),
            Vec::new()
        );
        assert_eq!(parse_plan_changes("## Plan changes\n").unwrap(), Vec::new());
        assert_eq!(
            parse_plan_changes("Just prose.").unwrap_err(),
            PlanChangeParseError::MissingSection
        );
    }

    #[test]
    fn every_mistake_names_its_line() {
        let cases = [
            (
                "## Plan changes\n- add `x`: X\n  why: no goal\n",
                "line 2: `add` needs a `goal:` field",
            ),
            (
                "## Plan changes\n- add `x`: X\n  goal: g\n  colour: red\n",
                "line 4: unknown field \"colour\" for `add`",
            ),
            (
                "## Plan changes\n  goal: orphan\n",
                "line 2: a field belongs under an item, found \"goal: orphan\"",
            ),
            (
                "## Plan changes\n- add `x`: X\n  goal g\n",
                "line 3: a field is `key: value`, found \"goal g\"",
            ),
            (
                "## Plan changes\n- rename `x`\n",
                "line 2: expected `- add`, `- revise`, `- cancel`, `- decide` or `- none`, found \"- rename `x`\"",
            ),
            (
                "## Plan changes\n- add `x`: X\n  goal: g\n  workflow: heroic\n",
                "line 4: unknown workflow \"heroic\"; use fast, standard, deep or review",
            ),
            (
                "## Plan changes\n- cancel `x`\n",
                "line 2: `cancel` needs a reason after the id",
            ),
            (
                "## Plan changes\n- decide: Title\n",
                "line 2: `decide` needs a `why:` field",
            ),
            (
                "## Plan changes\n- revise `x`\n",
                "line 2: `revise` needs at least one field to change",
            ),
            (
                "## Plan changes\n- none\n- cancel `x`: gone\n",
                "line 2: `- none` cannot be combined with other changes",
            ),
        ];
        for (answer, expected) in cases {
            let error = parse_plan_changes(answer).unwrap_err();
            assert_eq!(error.to_string(), expected, "{answer}");
        }
    }

    #[test]
    fn line_numbers_count_from_the_start_of_the_answer() {
        let answer = "prose\n\n## Plan changes\n\n- add `x`: X\n  goal: g\n  nope: 1\n";
        let error = parse_plan_changes(answer).unwrap_err();
        assert_eq!(
            error,
            PlanChangeParseError::UnknownField {
                line: 7,
                item: "add",
                field: "nope".to_owned(),
            }
        );
    }

    #[test]
    fn an_invalid_id_is_refused_with_its_line() {
        let error =
            parse_plan_changes("## Plan changes\n- add `Bad Id`: X\n  goal: g\n").unwrap_err();
        assert!(
            matches!(error, PlanChangeParseError::InvalidId { line: 2, .. }),
            "{error}"
        );
    }
}
