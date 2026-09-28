use std::collections::BTreeSet;

use serde_json::Value;

use crate::domain::{NativeModelUsage, StageKind};
use crate::engine::UsageDelta;

use super::ClaudeProviderError;

#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror native Claude Code permission_denials JSON keys"
)]
pub(crate) struct PermissionDenial {
    pub tool_name: String,
    /// Native Claude Code `tool_use_id`. Lets cumulative `permission_denials`
    /// history be split into previously observed and newly raised requests.
    pub tool_use_id: Option<String>,
    pub tool_input: Value,
}

/// Stage kinds that may never mutate repository content: the complement of
/// `StageKind::edits_workspace`, so a new editing kind is never read-only here
/// by omission.
pub(crate) const fn read_only_stage(kind: StageKind) -> bool {
    !kind.edits_workspace()
}

impl PermissionDenial {
    pub(crate) fn is_mutating_tool(&self) -> bool {
        matches!(
            self.tool_name.as_str(),
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit"
        )
    }

    pub(crate) fn is_question(&self) -> bool {
        self.tool_name == "AskUserQuestion"
    }

    /// What the request is *about*: the command, path or URL the operator has
    /// to judge. A bare tool name is not a permission question anyone can
    /// answer, so every summary shown to a human carries this.
    pub(crate) fn subject(&self) -> Option<&str> {
        let field = match self.tool_name.as_str() {
            "Bash" => "command",
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Read" | "NotebookRead" => {
                "file_path"
            }
            "WebFetch" => "url",
            _ => return None,
        };
        self.tool_input
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Whether approving this request as asked can actually be replayed.
    pub(crate) fn is_grantable(&self) -> bool {
        self.exact_rules().is_ok()
    }

    /// Whether two denial entries describe the same native permission request.
    ///
    /// Prefers `tool_use_id`; falls back to structural equality only when the
    /// CLI omitted identifiers on either side.
    pub(crate) fn same_request(&self, other: &Self) -> bool {
        match (&self.tool_use_id, &other.tool_use_id) {
            (Some(left), Some(right)) => left == right,
            _ => self.tool_name == other.tool_name && self.tool_input == other.tool_input,
        }
    }

    /// Whether denial can be treated as historical after a successful terminal result.
    ///
    /// This is deliberately stricter than `exact_rules`: an exact Bash rule is
    /// still not enough to grant execution authority. Only commands proven to
    /// be read-only diagnostics may be ignored as recovered history.
    pub(crate) fn is_recovered_diagnostic(&self) -> bool {
        match self.tool_name.as_str() {
            "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "WebFetch" | "WebSearch" => true,
            "Bash" => self
                .tool_input
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(safe_diagnostic_shell),
            _ => false,
        }
    }

    /// Whether the denial cost the stage an input it went outside the
    /// workspace to fetch.
    ///
    /// Mutation is not the only unfinished business a denial can leave. A
    /// denied fetch — an MCP call, a web fetch, `gh`/`curl`, a `git fetch` —
    /// means the evidence the stage asked for never arrived, and every later
    /// stage inherits an artifact written without it. That is exactly the
    /// shape a "successful" result hides, so it is terminal for every stage
    /// kind, read-only ones included.
    ///
    /// Unparseable shell stays non-acquisition: the classifier only reports
    /// what it can read, and the remaining rules still apply.
    pub(crate) fn is_denied_acquisition(&self) -> bool {
        if self.tool_name.starts_with("mcp__") {
            return true;
        }
        match self.tool_name.as_str() {
            "WebFetch" | "WebSearch" => true,
            "Bash" => self
                .tool_input
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(fetches_over_network),
            _ => false,
        }
    }

    /// Whether a denial left in a *successful* terminal result still needs a human.
    ///
    /// A denied tool call never executed, so it cannot have mutated anything —
    /// but it can still have withheld an input (`is_denied_acquisition`), which
    /// is terminal everywhere. Beyond that, for read-only stages the only
    /// unfinished business is a requested mutation or an unanswered question;
    /// any denied local Bash/read history is recovered history regardless of
    /// shell syntax. Mutating stages stay conservative: only deterministically
    /// read-only diagnostics are history.
    pub(crate) fn requires_terminal_attention(&self, stage_kind: StageKind) -> bool {
        if self.is_denied_acquisition() {
            return true;
        }
        if read_only_stage(stage_kind) {
            self.is_mutating_tool() || self.is_question()
        } else {
            !self.is_recovered_diagnostic()
        }
    }

    /// Whether this refusal is answered by the agent's own later success.
    ///
    /// Restricted to requests that could not themselves have changed
    /// anything. A refused *mutation* is never excused this way: the operator
    /// still needs to know the agent was blocked from editing, even if it
    /// managed to write something else afterwards.
    pub(crate) fn was_superseded(&self, superseded: &BTreeSet<String>) -> bool {
        !self.is_mutating_tool()
            && !self.is_question()
            && self
                .tool_use_id
                .as_ref()
                .is_some_and(|id| superseded.contains(id))
    }

    /// Terminal attention rule for explicit disposable native eval execution.
    ///
    /// Separates "safe to GRANT this Bash" (never, in eval) from "should a
    /// DENIED Bash strand a completed eval stage" (no). A denied tool never
    /// executed, so a successful eval terminal with only denied Bash/read/
    /// search history completes and the harness — diff, scope, trusted
    /// validation, structured artifact — decides pass or normal FAIL. Only a
    /// mutation request, a question, or an unknown tool still needs a human;
    /// exact safe Edit/Write then flows through eval auto-resolution.
    pub(crate) fn requires_eval_terminal_attention(&self) -> bool {
        !matches!(
            self.tool_name.as_str(),
            "Bash" | "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "WebFetch" | "WebSearch"
        )
    }

    /// Exact Edit/Write continuation allowed only by disposable native eval.
    pub(crate) fn is_safe_eval_edit(&self, workspace_path: &std::path::Path) -> bool {
        if !matches!(self.tool_name.as_str(), "Edit" | "Write") {
            return false;
        }
        let Some(path) = self.tool_input.get("file_path").and_then(Value::as_str) else {
            return false;
        };
        if !safe_rule_value(path) || !std::path::Path::new(path).is_absolute() {
            return false;
        }
        let requested = std::path::Path::new(path);
        if requested
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return false;
        }
        if std::fs::symlink_metadata(requested)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return false;
        }
        if std::fs::metadata(requested).is_ok_and(|metadata| metadata.is_dir()) {
            return false;
        }
        let Ok(root) = std::fs::canonicalize(workspace_path) else {
            return false;
        };
        let canonical = if requested.exists() {
            std::fs::canonicalize(requested).ok()
        } else {
            requested.parent().and_then(|parent| {
                std::fs::canonicalize(parent)
                    .ok()
                    .map(|parent| parent.join(requested.file_name().unwrap_or_default()))
            })
        };
        canonical.is_some_and(|path| path.starts_with(&root) && path != root)
    }

    /// Every exact `--allowedTools` rule that replays this one denied call.
    ///
    /// One rule for every tool but Bash. A Bash line that is a single simple
    /// command is one rule; a compound line is one rule per simple command,
    /// because that is how Claude Code itself reads it — it splits a compound
    /// command on its separators and runs it only when *every* part matches a
    /// rule. Granting the parts therefore replays exactly the denied call and
    /// nothing wider.
    ///
    /// # Errors
    /// `UnsafePermission` when the request cannot be expressed as exact rules
    /// at all: an unknown tool, a path or command carrying rule syntax, or
    /// shell this parser refuses to read (command substitution, backticks,
    /// subshells, heredocs).
    pub(crate) fn exact_rules(&self) -> Result<Vec<String>, ClaudeProviderError> {
        let mut unsafe_bash = None;
        let target = match self.tool_name.as_str() {
            "Write" | "Edit" => self
                .tool_input
                .get("file_path")
                .and_then(Value::as_str)
                .filter(|path| safe_rule_value(path))
                .map(|path| vec![format!("Edit({})", path_rule(path))]),
            "Read" => self
                .tool_input
                .get("file_path")
                .and_then(Value::as_str)
                .filter(|path| safe_rule_value(path))
                .map(|path| vec![format!("Read({})", path_rule(path))]),
            "Bash" => self
                .tool_input
                .get("command")
                .and_then(Value::as_str)
                .ok_or(BashRefusal::Unreadable)
                .and_then(bash_rules)
                .map_err(|refusal| {
                    // Surface the command itself and why it was refused: the
                    // user sees *which* Bash call cannot be granted exactly,
                    // and what to change, not a bare tool name.
                    let command = self
                        .tool_input
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing command>");
                    unsafe_bash = Some(format!(
                        "Bash command cannot be granted as an exact rule ({refusal}); type a response to continue without granting it, or stop the run: {command}"
                    ));
                })
                .ok(),
            "WebFetch" => self
                .tool_input
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| url.split('/').nth(2))
                .map(|domain| vec![format!("WebFetch(domain:{domain})")]),
            // An MCP tool name is already the whole rule Claude Code reads:
            // `mcp__<server>__<tool>` grants that one tool and nothing else,
            // so the denied call replays exactly. No arguments enter the
            // rule, so there is no wider call it could let through.
            name if name.starts_with("mcp__") && safe_rule_value(name) => {
                Some(vec![name.to_owned()])
            }
            _ => None,
        };
        target.filter(|rules| !rules.is_empty()).ok_or_else(|| {
            ClaudeProviderError::UnsafePermission(
                unsafe_bash.unwrap_or_else(|| self.tool_name.clone()),
            )
        })
    }
}

/// Why one denied Bash command cannot be granted as exact rules.
#[derive(Debug, PartialEq)]
enum BashRefusal {
    /// Shell the parser does not read: substitution, subshells, heredocs,
    /// unbalanced quotes.
    Unreadable,
    /// A part that is only a redirection, which no rule can name.
    BareRedirection(String),
    /// A part carrying rule syntax: a wildcard would widen the rule past the
    /// denied call, and `)` would end it early.
    RuleSyntax { part: String, character: char },
}

impl std::fmt::Display for BashRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable => formatter.write_str(
                "shell this parser will not read: command substitution, subshell, or heredoc",
            ),
            Self::BareRedirection(part) => {
                write!(formatter, "a part is only a redirection: {part}")
            }
            Self::RuleSyntax { part, character } => write!(
                formatter,
                "'{character}' is permission-rule syntax and would grant more than this call, in: {part}"
            ),
        }
    }
}

/// The exact rules that grant one denied Bash command, or why the line
/// cannot be expressed as rules.
///
/// A compound line becomes one rule per simple command, built from the
/// *original* source text of each part — quote removal is lossy, and a rule
/// has to match the command Claude Code will re-read character for
/// character. A part that is only a redirection, or that carries rule
/// syntax, makes the whole line ungrantable rather than partly granted.
fn bash_rules(command: &str) -> Result<Vec<String>, BashRefusal> {
    // Every line goes through the parser, single command included: a rule may
    // only name shell that was read and understood, never text waved through
    // because it happened to hold no separator.
    let segments = split_simple_commands(command).ok_or(BashRefusal::Unreadable)?;
    if segments.is_empty() {
        return Err(BashRefusal::Unreadable);
    }
    segments
        .iter()
        .map(|segment| {
            let text = command
                .get(segment.source.clone())
                .ok_or(BashRefusal::Unreadable)?
                .trim();
            if segment.words.iter().all(|word| word.redirect) {
                return Err(BashRefusal::BareRedirection(text.to_owned()));
            }
            if let Some(character) = rule_syntax(text) {
                return Err(BashRefusal::RuleSyntax {
                    part: text.to_owned(),
                    character,
                });
            }
            // Re-read the slice rather than scanning it for separator
            // characters: `2>&1` is an `&` that separates nothing, and a rule
            // holding a real separator would grant more than one command.
            if split_simple_commands(text).is_some_and(|parts| parts.len() == 1) {
                Ok(format!("Bash({text})"))
            } else {
                Err(BashRefusal::Unreadable)
            }
        })
        .collect()
}

fn path_rule(path: &str) -> String {
    if path.starts_with('/') {
        format!("/{path}")
    } else {
        path.to_owned()
    }
}

/// One shell word after quote removal.
#[derive(Debug, Default)]
struct ShellWord {
    text: String,
    /// Word contains an unquoted `<` or `>` and therefore is a redirection.
    redirect: bool,
}

/// One simple command inside a shell line.
#[derive(Debug)]
struct SimpleCommand {
    words: Vec<ShellWord>,
    /// Byte range this command occupies in the original line, separators
    /// excluded. Quote removal makes `words` unusable for anything that has
    /// to be handed back to a shell — or to Claude Code as a permission
    /// rule — so the untouched source text travels alongside it.
    source: std::ops::Range<usize>,
}

/// Splits one Bash command line into simple commands (segments of words).
///
/// Understands single/double quotes, backslash escapes, and the separators
/// `;`, `|`, `||`, `&`, `&&`, newlines. Returns `None` for anything that is
/// not a flat sequence of simple commands: subshells, grouping, command
/// substitution, backticks, heredocs, or process substitution.
fn split_simple_commands(command: &str) -> Option<Vec<SimpleCommand>> {
    // Heredocs and here-strings turn the following lines into data, so the
    // line-by-line reading below would take a document body for commands.
    if command.contains("$(") || command.contains('`') || command.contains("<<") {
        return None;
    }
    let mut segments: Vec<SimpleCommand> = Vec::new();
    let mut words: Vec<ShellWord> = Vec::new();
    let mut word = ShellWord::default();
    let mut in_word = false;
    // Byte span of the segment being read: `start` opens on its first word
    // character, `end` follows the last one, so separators and the spaces
    // around them stay outside the recorded source.
    let mut start: Option<usize> = None;
    let mut end = 0;
    let mut chars = command.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        match character {
            '\'' => {
                in_word = true;
                start = start.or(Some(index));
                end = read_single_quoted(&mut chars, &mut word.text)?;
            }
            '"' => {
                in_word = true;
                start = start.or(Some(index));
                end = read_double_quoted(&mut chars, &mut word.text)?;
            }
            '\\' => match chars.next() {
                Some((_, '\n')) => flush_word(&mut words, &mut word, &mut in_word),
                Some((escaped_index, escaped)) => {
                    in_word = true;
                    start = start.or(Some(index));
                    end = escaped_index + escaped.len_utf8();
                    word.text.push(escaped);
                }
                None => return None,
            },
            ' ' | '\t' => flush_word(&mut words, &mut word, &mut in_word),
            '\n' | '\r' | ';' => {
                flush_word(&mut words, &mut word, &mut in_word);
                flush_segment(&mut words, &mut start, end, &mut segments);
            }
            '|' => {
                flush_word(&mut words, &mut word, &mut in_word);
                if chars.peek().is_some_and(|(_, next)| *next == '|') {
                    chars.next();
                }
                flush_segment(&mut words, &mut start, end, &mut segments);
            }
            '&' => {
                if chars.peek().is_some_and(|(_, next)| *next == '>') {
                    // `&>target` redirection prefix.
                    in_word = true;
                    start = start.or(Some(index));
                    end = index + 1;
                    word.text.push('&');
                } else if in_word && word.redirect && word.text.ends_with('>') {
                    // `2>&1` style descriptor duplication.
                    end = index + 1;
                    word.text.push('&');
                } else {
                    flush_word(&mut words, &mut word, &mut in_word);
                    if chars.peek().is_some_and(|(_, next)| *next == '&') {
                        chars.next();
                    }
                    flush_segment(&mut words, &mut start, end, &mut segments);
                }
            }
            '<' | '>' => {
                // Redirection glued to a preceding descriptor digit (`2>`) or
                // `&>` prefix stays in the same word; otherwise starts a new one.
                let glued = in_word
                    && (word.text.chars().all(|digit| digit.is_ascii_digit())
                        || word.text == "&"
                        || word.text == ">");
                if !glued {
                    flush_word(&mut words, &mut word, &mut in_word);
                }
                in_word = true;
                start = start.or(Some(index));
                end = index + 1;
                word.redirect = true;
                word.text.push(character);
            }
            '(' | ')' => return None,
            other => {
                in_word = true;
                start = start.or(Some(index));
                end = index + other.len_utf8();
                word.text.push(other);
            }
        }
    }
    flush_word(&mut words, &mut word, &mut in_word);
    flush_segment(&mut words, &mut start, end, &mut segments);
    Some(segments)
}

/// Closes the word being read, if one is open.
fn flush_word(words: &mut Vec<ShellWord>, word: &mut ShellWord, in_word: &mut bool) {
    if *in_word {
        words.push(std::mem::take(word));
        *in_word = false;
    }
}

/// Closes the simple command being read, if it collected any word.
fn flush_segment(
    words: &mut Vec<ShellWord>,
    start: &mut Option<usize>,
    end: usize,
    segments: &mut Vec<SimpleCommand>,
) {
    if let (false, Some(open)) = (words.is_empty(), start.take()) {
        segments.push(SimpleCommand {
            words: std::mem::take(words),
            source: open..end,
        });
    } else {
        words.clear();
        *start = None;
    }
}

/// Reads to the closing quote and returns the byte index just past it.
fn read_single_quoted(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    text: &mut String,
) -> Option<usize> {
    loop {
        match chars.next() {
            Some((index, '\'')) => return Some(index + 1),
            Some((_, inner)) => text.push(inner),
            None => return None,
        }
    }
}

/// Reads to the closing quote and returns the byte index just past it.
fn read_double_quoted(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    text: &mut String,
) -> Option<usize> {
    loop {
        match chars.next() {
            Some((index, '"')) => return Some(index + 1),
            Some((_, '\\')) => match chars.next() {
                Some((_, escaped @ ('"' | '\\' | '$' | '`'))) => text.push(escaped),
                Some((_, other)) => {
                    text.push('\\');
                    text.push(other);
                }
                None => return None,
            },
            Some((_, inner)) => text.push(inner),
            None => return None,
        }
    }
}

/// Whether one denied Bash command is deterministically read-only.
///
/// Accepts flat pipelines/sequences of known diagnostic executables with
/// harmless redirections (`2>&1`, `1>&2`, `>/dev/null`, `2>/dev/null`,
/// `</dev/null`). Everything else — assignments, loops, substitution,
/// unknown executables, file redirection — is rejected, not interpreted.
fn safe_diagnostic_shell(command: &str) -> bool {
    let Some(segments) = split_simple_commands(command) else {
        return false;
    };
    !segments.is_empty()
        && segments
            .into_iter()
            .all(|segment| safe_diagnostic_command(&segment.words))
}

fn harmless_redirection(word: &str) -> bool {
    matches!(
        word,
        "2>&1"
            | "1>&2"
            | ">&2"
            | ">&1"
            | "2>/dev/null"
            | ">/dev/null"
            | "1>/dev/null"
            | "&>/dev/null"
            | "</dev/null"
    )
}

fn strip_redirections(segment: &[ShellWord]) -> Option<Vec<&str>> {
    let mut words = Vec::new();
    let mut index = 0;
    while index < segment.len() {
        let word = &segment[index];
        if word.redirect {
            if harmless_redirection(&word.text) {
                index += 1;
                continue;
            }
            let target = segment.get(index + 1)?;
            if matches!(word.text.as_str(), ">" | "1>" | "2>" | "&>" | "<")
                && !target.redirect
                && target.text == "/dev/null"
            {
                index += 2;
                continue;
            }
            return None;
        }
        words.push(word.text.as_str());
        index += 1;
    }
    Some(words)
}

/// Whether any simple command in the line reaches the network for data.
///
/// Deliberately narrow: named fetch tools and the Git subcommands that talk to
/// a remote. It answers "did this denial cost the stage an input", not "is this
/// command safe" — `safe_diagnostic_shell` owns that question.
fn fetches_over_network(command: &str) -> bool {
    split_simple_commands(command).is_some_and(|segments| {
        segments.iter().any(|segment| {
            strip_redirections(&segment.words).is_some_and(|words| {
                words
                    .split_first()
                    .is_some_and(|(executable, arguments)| network_fetch(executable, arguments))
            })
        })
    })
}

fn network_fetch(executable: &str, arguments: &[&str]) -> bool {
    let executable = std::path::Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable);
    match executable {
        "gh" | "glab" | "hub" | "curl" | "wget" | "http" | "https" | "ssh" | "scp" | "sftp"
        | "rsync" | "aws" | "nc" | "ncat" | "telnet" => true,
        "git" => {
            let mut index = 0;
            while let Some(word) = arguments.get(index) {
                match *word {
                    "-C" => index += 2,
                    "--no-pager" | "-P" => index += 1,
                    _ => break,
                }
            }
            arguments.get(index).is_some_and(|subcommand| {
                matches!(
                    *subcommand,
                    "fetch" | "clone" | "pull" | "ls-remote" | "remote" | "submodule"
                )
            })
        }
        _ => false,
    }
}

fn safe_diagnostic_command(segment: &[ShellWord]) -> bool {
    let Some(words) = strip_redirections(segment) else {
        return false;
    };
    let Some((executable, arguments)) = words.split_first() else {
        return false;
    };
    let executable = std::path::Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable);
    match executable {
        "git" => safe_git(arguments),
        "cargo" => safe_cargo(arguments),
        "find" => !arguments.iter().any(|word| {
            matches!(
                *word,
                "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-delete"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        "sort" => !arguments.iter().any(|word| {
            *word == "-o"
                || word.starts_with("--output")
                || (word.starts_with('-') && !word.starts_with("--") && word.contains('o'))
        }),
        "rg" => !arguments.iter().any(|word| word.starts_with("--pre")),
        "cd" | "echo" | "printf" | "true" | "false" | ":" | "pwd" | "test" | "[" | "type"
        | "which" | "grep" | "egrep" | "fgrep" | "cat" | "head" | "tail" | "ls" | "wc" | "file"
        | "stat" | "tr" | "cut" | "uniq" | "nl" | "basename" | "dirname" | "readlink"
        | "realpath" | "du" | "df" | "tree" | "diff" | "cmp" | "shasum" | "sha256sum"
        | "md5sum" | "od" | "xxd" | "hexdump" | "strings" | "column" | "date" => true,
        _ => false,
    }
}

fn safe_git(arguments: &[&str]) -> bool {
    let mut index = 0;
    while let Some(word) = arguments.get(index) {
        match *word {
            "-C" => index += 2,
            "--no-pager" | "-P" => index += 1,
            _ => break,
        }
    }
    let Some(subcommand) = arguments.get(index) else {
        return false;
    };
    matches!(
        *subcommand,
        "status"
            | "diff"
            | "log"
            | "ls-files"
            | "show"
            | "rev-parse"
            | "grep"
            | "rev-list"
            | "blame"
            | "cat-file"
            | "ls-tree"
            | "describe"
            | "shortlog"
    ) && !arguments[index + 1..]
        .iter()
        .any(|word| word.starts_with("--output"))
}

fn safe_cargo(arguments: &[&str]) -> bool {
    let mut index = 0;
    while arguments
        .get(index)
        .is_some_and(|word| word.starts_with('+'))
    {
        index += 1;
    }
    let Some(subcommand) = arguments.get(index) else {
        return false;
    };
    let rest = &arguments[index + 1..];
    match *subcommand {
        "test" | "check" | "build" | "clippy" | "metadata" | "tree" => !rest.contains(&"--fix"),
        "fmt" => rest.contains(&"--check"),
        _ => false,
    }
}

/// Largest invocation log scanned for supersession evidence. Past it the
/// scan gives up and reports nothing superseded, so an unreadably large
/// stream asks the operator instead of guessing.
pub(crate) const MAX_SUPERSESSION_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Native `tool_use` identifiers whose refusal the agent later worked around.
///
/// A refused call never ran, so the only question it leaves is whether the
/// agent stayed blocked. This answers that from the invocation's own record:
/// an identifier is superseded when a *later* call of the same tool, in the
/// same invocation, returned a result that was not an error. The agent asked,
/// was told no, tried again another way, and got through.
///
/// Only the tool matters, not the arguments. Recovery is precisely the case
/// where the agent does something different — a shorter command, a different
/// file — so demanding the same target would never match anything. What the
/// evidence supports is narrow and stated plainly: the capability was not
/// denied to the agent overall.
///
/// Callers decide what supersession excuses. It is evidence, not a policy.
pub(crate) fn superseded_requests(raw: &str) -> BTreeSet<String> {
    let mut calls: Vec<(String, String)> = Vec::new();
    let mut succeeded: BTreeSet<String> = BTreeSet::new();
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    if let (Some(id), Some(name)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                    ) {
                        calls.push((id.to_owned(), name.to_owned()));
                    }
                }
                Some("tool_result") => {
                    let failed = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !failed && let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        succeeded.insert(id.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    let mut superseded = BTreeSet::new();
    for (index, (id, name)) in calls.iter().enumerate() {
        if calls[index + 1..]
            .iter()
            .any(|(later_id, later_name)| later_name == name && succeeded.contains(later_id))
        {
            superseded.insert(id.clone());
        }
    }
    superseded
}

fn safe_rule_value(value: &str) -> bool {
    !value.is_empty() && rule_syntax(value).is_none()
}

/// The first character in `value` that a permission rule would read as
/// syntax rather than as itself.
fn rule_syntax(value: &str) -> Option<char> {
    value
        .chars()
        .find(|character| matches!(character, '*' | '?' | ')' | '\n' | '\r'))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ClaudeRecord {
    Initialized {
        session_id: String,
        model: Option<String>,
    },
    Progress(String),
    NeedsUser {
        summary: String,
        denials: Vec<PermissionDenial>,
        question: bool,
    },
    Result {
        session_id: Option<String>,
        content: String,
        success: bool,
        error: Option<String>,
        denials: Vec<PermissionDenial>,
        usage: Option<UsageDelta>,
    },
    Ignored,
}

pub(crate) fn first_record(
    bytes: &[u8],
) -> Result<Option<(ClaudeRecord, usize)>, ClaudeProviderError> {
    let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
        return Ok(None);
    };
    let line = &bytes[..newline];
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some((ClaudeRecord::Ignored, newline + 1)));
    }
    let value: Value = serde_json::from_slice(line)
        .map_err(|error| ClaudeProviderError::Protocol(error.to_string()))?;
    Ok(Some((decode(&value)?, newline + 1)))
}

fn decode(value: &Value) -> Result<ClaudeRecord, ClaudeProviderError> {
    match value.get("type").and_then(Value::as_str) {
        Some("system") if value.get("subtype").and_then(Value::as_str) == Some("init") => {
            let session_id = required_string(value, "session_id")?;
            Ok(ClaudeRecord::Initialized {
                session_id,
                model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            })
        }
        Some("assistant") => Ok(decode_assistant(value)),
        Some("result") => Ok(decode_result(value)),
        Some(_) | None => Ok(ClaudeRecord::Ignored),
    }
}

fn decode_assistant(value: &Value) -> ClaudeRecord {
    let message = value.get("message").unwrap_or(value);
    let content = message.get("content").and_then(Value::as_array);
    if let Some(tool) = content.and_then(|items| {
        items.iter().find(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool_use")
                && item.get("name").and_then(Value::as_str) == Some("AskUserQuestion")
        })
    }) {
        return ClaudeRecord::NeedsUser {
            summary: question_summary(tool.get("input").unwrap_or(&Value::Null)),
            denials: vec![PermissionDenial {
                tool_name: "AskUserQuestion".to_owned(),
                tool_use_id: tool
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                tool_input: tool.get("input").cloned().unwrap_or(Value::Null),
            }],
            question: true,
        };
    }
    // Per-assistant-message `usage` is intentionally NOT accounted: real
    // role_core_v3 streams repeat identical usage across content-block records
    // of one API call and carry partial output snapshots (summed 18/83 vs the
    // authoritative terminal 8/1221 in one real session), and sidechain
    // records are indistinguishable here. The terminal result record is the
    // only trustworthy Claude usage source; see decode_result.
    let text = content
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        ClaudeRecord::Ignored
    } else {
        ClaudeRecord::Progress(text)
    }
}

fn decode_result(value: &Value) -> ClaudeRecord {
    let denials = value
        .get("permission_denials")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|denial| {
            Some(PermissionDenial {
                tool_name: denial.get("tool_name")?.as_str()?.to_owned(),
                tool_use_id: denial
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                tool_input: denial.get("tool_input").cloned().unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    // `permission_denials` is cumulative history, not proof of an unresolved
    // request. Provider splits it into historical and newly raised denials and
    // applies stage semantics separately.
    let success = !value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && value.get("subtype").and_then(Value::as_str) == Some("success");
    let content = value
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let error = (!success).then(|| {
        value
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| value.get("subtype").and_then(Value::as_str))
            .unwrap_or("Claude Code execution failed")
            .to_owned()
    });
    ClaudeRecord::Result {
        session_id: value
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        content,
        success,
        error,
        denials,
        usage: decode_result_usage(value),
    }
}

/// Extracts the terminal cumulative usage of one Claude Code invocation.
///
/// The result record's `usage` object is the runtime's authoritative
/// main-agent total for the whole invocation; `modelUsage` is the runtime's
/// own per-model breakdown across every model it used (subagents included).
/// The breakdown overlaps the aggregate and is carried separately so it is
/// never summed into it. Absent native fields stay `None` (unavailable),
/// never zero. Returns `None` when the record carries no `usage` object.
fn decode_result_usage(value: &Value) -> Option<UsageDelta> {
    let usage = value.get("usage")?;
    let native_models = value
        .get("modelUsage")
        .and_then(Value::as_object)
        .map(|models| {
            let mut entries = models
                .iter()
                .map(|(model, dims)| NativeModelUsage {
                    model: model.clone(),
                    input_units: dims.get("inputTokens").and_then(Value::as_u64).unwrap_or(0),
                    output_units: dims
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cache_read_units: dims.get("cacheReadInputTokens").and_then(Value::as_u64),
                    cache_write_units: dims.get("cacheCreationInputTokens").and_then(Value::as_u64),
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.model.cmp(&right.model));
            entries
        })
        .filter(|entries| !entries.is_empty());
    Some(UsageDelta {
        input_units: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_units: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_units: usage.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_write_units: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        reasoning_output_units: None,
        native_models,
    })
}

fn required_string(value: &Value, key: &'static str) -> Result<String, ClaudeProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ClaudeProviderError::Protocol(format!("missing {key}")))
}

fn question_summary(input: &Value) -> String {
    input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| question.get("question").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
        .pipe(|summary| {
            if summary.is_empty() {
                "Claude Code needs user input".to_owned()
            } else {
                summary
            }
        })
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str) -> PermissionDenial {
        PermissionDenial {
            tool_name: "Bash".to_owned(),
            tool_use_id: None,
            tool_input: serde_json::json!({ "command": command }),
        }
    }

    fn tool(name: &str, input: Value) -> PermissionDenial {
        PermissionDenial {
            tool_name: name.to_owned(),
            tool_use_id: None,
            tool_input: input,
        }
    }

    #[test]
    fn partial_record_waits_for_newline() {
        assert_eq!(first_record(br#"{"type":"system"}"#).unwrap(), None);
    }

    #[test]
    fn init_extracts_native_identity() {
        let raw = b"{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"abc\",\"model\":\"claude-x\"}\n";
        assert_eq!(
            first_record(raw).unwrap(),
            Some((
                ClaudeRecord::Initialized {
                    session_id: "abc".to_owned(),
                    model: Some("claude-x".to_owned())
                },
                raw.len()
            ))
        );
    }

    #[test]
    fn result_captures_authoritative_usage_and_native_model_breakdown() {
        // Shape copied structurally from a real role_core_v3 Claude result
        // record (implementer_scope_discipline rep-003): terminal usage is the
        // runtime's cumulative main-agent total, modelUsage the per-model
        // breakdown across every model the runtime used (subagents included).
        let raw = concat!(
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,",
            "\"result\":\"done\",\"session_id\":\"abc\",\"permission_denials\":[],",
            "\"num_turns\":4,\"total_cost_usd\":0.31,",
            "\"usage\":{\"input_tokens\":8,\"cache_creation_input_tokens\":3638,",
            "\"cache_read_input_tokens\":153292,\"output_tokens\":1221,",
            "\"service_tier\":\"standard\"},",
            "\"modelUsage\":{",
            "\"claude-sonnet-5\":{\"inputTokens\":4,\"outputTokens\":782,",
            "\"cacheReadInputTokens\":13118,\"cacheCreationInputTokens\":3855,",
            "\"costUSD\":0.03,\"contextWindow\":1000000},",
            "\"claude-fable-5\":{\"inputTokens\":8,\"outputTokens\":1221,",
            "\"cacheReadInputTokens\":153292,\"cacheCreationInputTokens\":3638,",
            "\"costUSD\":0.28,\"contextWindow\":1000000}}}\n"
        )
        .as_bytes();
        let Some((ClaudeRecord::Result { usage, .. }, _)) = first_record(raw).unwrap() else {
            panic!("expected result record");
        };
        let usage = usage.expect("result carries usage");
        assert_eq!(usage.input_units, 8);
        assert_eq!(usage.output_units, 1221);
        assert_eq!(usage.cache_read_units, Some(153_292));
        assert_eq!(usage.cache_write_units, Some(3638));
        assert_eq!(usage.reasoning_output_units, None);
        let models = usage.native_models.expect("modelUsage captured");
        assert_eq!(models.len(), 2);
        // Sorted by model for determinism regardless of native map order.
        assert_eq!(models[0].model, "claude-fable-5");
        assert_eq!(models[0].output_units, 1221);
        assert_eq!(models[0].cache_read_units, Some(153_292));
        assert_eq!(models[1].model, "claude-sonnet-5");
        assert_eq!(models[1].input_units, 4);
        assert_eq!(models[1].cache_write_units, Some(3855));
        // The breakdown overlaps the aggregate (fable == main agent) and is
        // carried separately, never summed into input/output units.
        assert_eq!(models[0].input_units, usage.input_units);
    }

    #[test]
    fn result_missing_usage_dimensions_stay_unavailable_not_zero() {
        let bare = b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"x\",\"permission_denials\":[],\"usage\":{\"input_tokens\":8,\"output_tokens\":9}}\n";
        let Some((ClaudeRecord::Result { usage, .. }, _)) = first_record(bare).unwrap() else {
            panic!("expected result record");
        };
        let usage = usage.unwrap();
        assert_eq!(usage.cache_read_units, None);
        assert_eq!(usage.cache_write_units, None);
        assert_eq!(usage.native_models, None);

        let without = b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"x\",\"permission_denials\":[]}\n";
        let Some((ClaudeRecord::Result { usage, .. }, _)) = first_record(without).unwrap() else {
            panic!("expected result record");
        };
        assert_eq!(usage, None);
    }

    #[test]
    fn assistant_message_usage_is_not_accounted_as_usage() {
        // Real streams repeat identical per-message usage across content-block
        // records and carry partial output snapshots; only the terminal result
        // usage is trustworthy, so assistant usage maps to Progress/Ignored.
        let with_text = b"{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":7},\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";
        assert!(matches!(
            first_record(with_text).unwrap(),
            Some((ClaudeRecord::Progress(text), _)) if text == "working"
        ));
        let without_text = b"{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":7},\"content\":[]}}\n";
        assert!(matches!(
            first_record(without_text).unwrap(),
            Some((ClaudeRecord::Ignored, _))
        ));
    }

    #[test]
    fn structured_denial_stays_structured_and_keeps_tool_use_id() {
        let raw = b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"permission_denials\":[{\"tool_name\":\"Write\",\"tool_use_id\":\"toolu_01A\",\"tool_input\":{\"file_path\":\"/tmp/a\"}}]}\n";
        let Some((
            ClaudeRecord::Result {
                success, denials, ..
            },
            _,
        )) = first_record(raw).unwrap()
        else {
            panic!()
        };
        assert!(success);
        assert_eq!(denials[0].tool_use_id.as_deref(), Some("toolu_01A"));
        assert_eq!(denials[0].exact_rules().unwrap(), vec!["Edit(//tmp/a)"]);
    }

    #[test]
    fn same_request_prefers_tool_use_id_then_structure() {
        let mut first = bash("cargo test 2>&1 | tail -20");
        first.tool_use_id = Some("toolu_1".to_owned());
        let mut second = bash("cargo test 2>&1 | tail -20");
        second.tool_use_id = Some("toolu_2".to_owned());
        assert!(!first.same_request(&second));
        assert!(first.same_request(&first.clone()));
        let untagged = bash("cargo test 2>&1 | tail -20");
        assert!(untagged.same_request(&first));
        assert!(!untagged.same_request(&bash("cargo test")));
    }

    #[test]
    fn simple_synthetic_diagnostic_is_still_recovered() {
        let denial = bash("cargo test && cargo clippy");
        assert!(denial.is_recovered_diagnostic());
        assert!(!denial.requires_terminal_attention(StageKind::Implementation));
        assert_eq!(
            denial.exact_rules().unwrap(),
            vec![
                "Bash(cargo test)".to_owned(),
                "Bash(cargo clippy)".to_owned()
            ],
        );
    }

    // Real shapes below are copied structurally from native role_core_v3 logs.

    #[test]
    fn real_invalid_plan_stop_diagnostic_wrapper_is_recovered() {
        let denial = bash(
            "cd \"/ABS/EVAL/WORKTREE\" && git ls-files | head -100 && echo \"---grep---\" && grep -rniE \"ConfigRegistry|config_registry|config-registry\" --exclude-dir=.git . ; echo \"grep exit: $?\"",
        );
        assert!(denial.is_recovered_diagnostic());
        assert!(!denial.requires_terminal_attention(StageKind::Implementation));
    }

    #[test]
    fn real_cargo_diagnostics_with_stderr_plumbing_are_recovered() {
        for command in [
            "cd \"/ABS/EVAL/WORKTREE\" && cargo test 2>&1 | tail -8; cargo clippy --all-targets 2>&1 | tail -30",
            "cd \"/ABS/EVAL/WORKTREE\" && cat Cargo.toml && cargo build 2>&1 | tail -3",
            "cargo test 2>&1 | tail -20",
            "cd \"/ABS/EVAL/WORKTREE\" && cat Cargo.toml .gitignore && git show --stat HEAD | head -20 && cargo build 2>&1 | tail -5",
            "cargo check 2> /dev/null; git status 1>&2",
        ] {
            assert!(
                bash(command).is_recovered_diagnostic(),
                "should be diagnostic: {command}"
            );
        }
    }

    #[test]
    fn shell_scripts_substitution_and_assignments_stay_unclassified() {
        for command in [
            "cd \"/ABS/EVAL/WORKTREE\" && git ls-files && echo --- && for f in $(git ls-files); do echo \"=== $f\"; cat \"$f\"; done",
            "R=/ABS/EVAL/WORKTREE; grep -rniE \"registry\" \"$R\" 2>/dev/null | head -100",
            "cd \"$(git rev-parse --show-toplevel)\" 2>/dev/null; cat -n src/lib.rs",
            "git grep -n foo $(git rev-list --all)",
            "(cd src && cat lib.rs)",
            "cat `ls`",
            "bash -c 'cargo test'",
            "sh scripts/check.sh",
        ] {
            let denial = bash(command);
            assert!(
                !denial.is_recovered_diagnostic(),
                "must stay unclassified: {command}"
            );
            assert!(denial.requires_terminal_attention(StageKind::Implementation));
            // Read-only stages do not depend on shell classification at all.
            assert!(!denial.requires_terminal_attention(StageKind::CodeQualityReview));
            assert!(!denial.requires_terminal_attention(StageKind::SpecReview));
        }
    }

    #[test]
    fn mutating_shell_is_never_diagnostic() {
        for command in [
            "cd \"/ABS/EVAL/WORKTREE\" && sed -i '' 's/    value + 2/    value * 2/' src/lib.rs && git diff && cargo test 2>&1 | tail -15",
            "sed -i '' 's/^    input\\.to_owned()$/    input.trim().to_owned()/' src/lib.rs && git diff",
            "perl -i -pe 's/a/b/' src/lib.rs",
            "rm -rf /tmp/output && git status",
            "mv src/a.rs src/b.rs",
            "cp src/a.rs src/b.rs",
            "cargo test 2>&1 | tee out.log",
            "cargo test > out.log 2>&1",
            "cargo test 2>out.log",
            "echo x >> file",
            "cat < input.txt",
            "cargo fmt",
            "cargo clippy --fix",
            "git status --output=x",
            "git -c alias.st='!rm -rf .' st",
            "sort -o out.txt input.txt",
            "find . -name '*.rs' -exec rm {} \\;",
            "find . -delete",
            "rg --pre 'rm -rf' foo",
            "xargs rm",
            "printf x >> file && git diff",
            "cargo run",
        ] {
            let denial = bash(command);
            assert!(
                !denial.is_recovered_diagnostic(),
                "must not pass: {command}"
            );
            assert!(denial.requires_terminal_attention(StageKind::Implementation));
        }
        assert!(bash("cargo fmt --check").is_recovered_diagnostic());
        assert!(
            bash("git -C /ABS/EVAL/WORKTREE --no-pager log --oneline -5").is_recovered_diagnostic()
        );
        assert!(
            bash("find . -type d \\( -name .git -o -name target \\) -prune -o -type f -print")
                .is_recovered_diagnostic()
        );
        assert!(bash("grep -rn \"a > b\" src").is_recovered_diagnostic());
    }

    /// The exact shape a real invocation has: an assistant message issues a
    /// `tool_use`, a user message answers with a `tool_result`, and a refusal
    /// is that result carrying `is_error`.
    fn stream(events: &[(&str, &str, bool)]) -> String {
        let mut lines = Vec::new();
        for (id, name, ok) in events {
            lines.push(
                serde_json::json!({
                    "type": "assistant",
                    "message": {"content": [{"type": "tool_use", "id": id, "name": name,
                                             "input": {"command": "irrelevant"}}]}
                })
                .to_string(),
            );
            let mut result = serde_json::json!({
                "type": "tool_result", "tool_use_id": id, "content": "output"
            });
            if !ok {
                result["is_error"] = serde_json::json!(true);
            }
            lines.push(
                serde_json::json!({"type": "user", "message": {"content": [result]}}).to_string(),
            );
        }
        lines.join("\n")
    }

    #[test]
    fn a_refusal_is_superseded_only_by_a_later_success_of_the_same_tool() {
        // Refused, then the agent got through with a different command.
        let recovered = superseded_requests(&stream(&[
            ("call_1", "Bash", false),
            ("call_2", "Bash", true),
        ]));
        assert!(recovered.contains("call_1"));
        assert!(
            !recovered.contains("call_2"),
            "the success itself is not superseded"
        );

        // Refused and never got through: the operator still decides.
        assert!(superseded_requests(&stream(&[("call_1", "Bash", false)])).is_empty());
        assert!(
            superseded_requests(&stream(&[
                ("call_1", "Bash", false),
                ("call_2", "Bash", false),
            ]))
            .is_empty(),
            "a second refusal is not recovery"
        );

        // Order matters: an earlier success says nothing about a later refusal.
        let backwards = superseded_requests(&stream(&[
            ("call_1", "Bash", true),
            ("call_2", "Bash", false),
        ]));
        assert!(backwards.is_empty());

        // A different tool succeeding is not evidence about this one.
        assert!(
            superseded_requests(&stream(&[
                ("call_1", "Bash", false),
                ("call_2", "Read", true),
            ]))
            .is_empty()
        );

        // Nothing to read is not evidence of anything.
        assert!(superseded_requests("").is_empty());
        assert!(superseded_requests("not json\n{}\n").is_empty());
    }

    #[test]
    fn supersession_never_excuses_a_refused_mutation_or_question() {
        let recovered = BTreeSet::from(["call_1".to_owned()]);
        let mut write = tool("Write", serde_json::json!({"file_path": "/x"}));
        write.tool_use_id = Some("call_1".to_owned());
        assert!(
            !write.was_superseded(&recovered),
            "a blocked edit still asks"
        );

        let mut question = tool("AskUserQuestion", serde_json::json!({}));
        question.tool_use_id = Some("call_1".to_owned());
        assert!(!question.was_superseded(&recovered));

        let mut shell = bash("ls && python3 - <<'PY'");
        shell.tool_use_id = Some("call_1".to_owned());
        assert!(shell.was_superseded(&recovered));
        // Without an identifier there is no evidence to match against.
        shell.tool_use_id = None;
        assert!(!shell.was_superseded(&recovered));
    }

    #[test]
    fn read_only_stage_terminal_attention_only_for_mutation_or_question() {
        let edit = tool(
            "Edit",
            serde_json::json!({"file_path":"/ABS/EVAL/WORKTREE/src/lib.rs"}),
        );
        let write = tool(
            "Write",
            serde_json::json!({"file_path":"/ABS/EVAL/WORKTREE/src/lib.rs"}),
        );
        let question = tool("AskUserQuestion", serde_json::json!({"questions":[]}));
        for kind in [
            StageKind::CodeQualityReview,
            StageKind::SpecReview,
            StageKind::Research,
        ] {
            assert!(read_only_stage(kind));
            assert!(edit.requires_terminal_attention(kind));
            assert!(write.requires_terminal_attention(kind));
            assert!(question.requires_terminal_attention(kind));
            assert!(!bash("sed -i '' 's/a/b/' src/lib.rs").requires_terminal_attention(kind));
            assert!(
                !tool("Read", serde_json::json!({"file_path":"/x"}))
                    .requires_terminal_attention(kind)
            );
        }
        for kind in [
            StageKind::Implementation,
            StageKind::Simplification,
            StageKind::Fix,
            StageKind::FollowUp,
        ] {
            assert!(
                !read_only_stage(kind),
                "{kind:?} mutates the workspace, same as Implementation and Fix"
            );
            assert!(edit.requires_terminal_attention(kind));
            assert!(question.requires_terminal_attention(kind));
            assert!(bash("sed -i '' 's/a/b/' src/lib.rs").requires_terminal_attention(kind));
            assert!(
                !tool("Read", serde_json::json!({"file_path":"/x"}))
                    .requires_terminal_attention(kind)
            );
        }
    }

    /// Real denial shapes from run `01M1K8KAJ1HMS47H7WR8YMN2PW`, where every
    /// stage lost its only route to an internal pull request and each one
    /// still terminated `"subtype":"success","is_error":false`. Research,
    /// synthesis, both reviews and the decision then wrote artifacts saying
    /// they were blocked, and the run rolled on to the end.
    #[test]
    fn denied_fetch_is_terminal_for_read_only_stages() {
        let denials = [
            bash(
                "gh api repos/Automattic/missioncontrol/pulls/164318 --hostname github.a8c.com 2>&1 | head -50",
            ),
            tool(
                "mcp__context-a8c__context-a8c-load-provider",
                serde_json::json!({"provider":"github-a8c"}),
            ),
            tool(
                "WebFetch",
                serde_json::json!({
                    "url":"https://github.a8c.com/Automattic/missioncontrol/pull/164318",
                    "prompt":"What is the PR title, description, and files changed?"
                }),
            ),
        ];
        for denial in &denials {
            assert!(denial.is_denied_acquisition(), "{denial:?}");
            for kind in [
                StageKind::Research,
                StageKind::CodeQualityReview,
                StageKind::SpecReview,
                StageKind::Implementation,
            ] {
                assert!(
                    denial.requires_terminal_attention(kind),
                    "{denial:?} left {kind:?} without the evidence it asked for"
                );
            }
        }
    }

    #[test]
    fn local_diagnostics_are_not_acquisitions() {
        for command in [
            "git status --porcelain",
            "git -C /ABS/EVAL/WORKTREE --no-pager log --oneline -5",
            "git diff trunk...HEAD --stat | tail -15",
            "cargo test",
            "grep -rn \"gh api\" src",
            "echo curl",
        ] {
            assert!(
                !bash(command).is_denied_acquisition(),
                "{command} never leaves the machine"
            );
        }
        for tool_name in ["Read", "Glob", "Grep", "LS", "Edit", "AskUserQuestion"] {
            assert!(
                !tool(tool_name, serde_json::json!({})).is_denied_acquisition(),
                "{tool_name}"
            );
        }
    }

    /// Unclassifiable shell keeps the pre-existing rules; the classifier only
    /// reports network use it can actually read.
    #[test]
    fn unparseable_shell_is_not_reported_as_acquisition() {
        for command in ["curl $(cat url.txt)", "`gh pr view 1`"] {
            assert!(!bash(command).is_denied_acquisition(), "{command}");
        }
    }

    /// `read_only_stage`'s doc comment claims it is the same predicate as
    /// `WorkflowDefinition::requires_writable_workspace`; this pins that
    /// claim so the two cannot drift apart the way they already did once —
    /// `FollowUp` was added to one and silently missed in the other.
    #[test]
    fn read_only_stage_agrees_with_workspace_writability_for_every_stage_kind() {
        use crate::domain::{
            Dependency, Role, StageDefinition, StageId, WorkflowDefinition, WorkflowKind,
        };

        for kind in [
            StageKind::Research,
            StageKind::Architecture,
            StageKind::Implementation,
            StageKind::Simplification,
            StageKind::CodeQualityReview,
            StageKind::SpecReview,
            StageKind::Review,
            StageKind::IndependentReview,
            StageKind::DeepAnalysis,
            StageKind::Synthesis,
            StageKind::Decision,
            StageKind::Fix,
            StageKind::FollowUp,
            StageKind::Lead,
        ] {
            let workflow = WorkflowDefinition::new(
                WorkflowKind::Standard,
                vec![StageDefinition::new(
                    StageId::new("stage").unwrap(),
                    kind,
                    Role::Implementer,
                    Vec::<Dependency>::new(),
                )],
            )
            .unwrap();
            assert_eq!(
                !read_only_stage(kind),
                workflow.requires_writable_workspace(),
                "{kind:?} disagrees between read_only_stage and requires_writable_workspace"
            );
        }
    }

    #[test]
    fn eval_edit_requires_canonical_path_inside_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("worktree");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let denial = tool(
            "Edit",
            serde_json::json!({
                "file_path": workspace.join("src/lib.rs").to_string_lossy()
            }),
        );
        assert!(denial.is_safe_eval_edit(&workspace));
        let escape = tool(
            "Edit",
            serde_json::json!({"file_path": "/tmp/other-repo/file"}),
        );
        assert!(!escape.is_safe_eval_edit(&workspace));
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, "outside\n").unwrap();
        let link = workspace.join("src/link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let symlink = tool("Write", serde_json::json!({"file_path": link}));
        assert!(!symlink.is_safe_eval_edit(&workspace));
        let wildcard = tool(
            "Edit",
            serde_json::json!({"file_path": workspace.join("src/*.rs").to_string_lossy()}),
        );
        assert!(!wildcard.is_safe_eval_edit(&workspace));
        let traversal = tool(
            "Edit",
            serde_json::json!({"file_path": workspace.join("src/../../outside.txt").to_string_lossy()}),
        );
        assert!(!traversal.is_safe_eval_edit(&workspace));
    }

    #[test]
    fn ask_user_question_always_decodes_as_question_attention() {
        let raw = br#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_q","name":"AskUserQuestion","input":{"questions":[{"question":"Choose?"}]}}]}}
"#;
        let Some((
            ClaudeRecord::NeedsUser {
                question, denials, ..
            },
            _,
        )) = first_record(raw).unwrap()
        else {
            panic!()
        };
        assert!(question);
        assert_eq!(denials[0].tool_use_id.as_deref(), Some("toolu_q"));
    }

    /// A compound line is granted as its parts, never as one rule carrying
    /// the separators: Claude Code splits the command itself and demands a
    /// rule for every part, so the parts are what a grant has to name.
    #[test]
    fn compound_bash_is_granted_as_one_rule_per_simple_command() {
        let denial = bash("printf x >> file && git diff");
        assert_eq!(
            denial.exact_rules().unwrap(),
            vec![
                "Bash(printf x >> file)".to_owned(),
                "Bash(git diff)".to_owned(),
            ],
        );
    }

    /// Rules are cut from the original text, not from parsed words: quote
    /// removal would hand Claude Code a command it never denied.
    #[test]
    fn compound_bash_rules_keep_the_operators_own_quoting() {
        let denial = bash(
            "grep -rn \"HELP_CENTER; VARIATION\" client/ | head -5; yarn eslint 'client/a b.tsx' 2>&1 | tail -20",
        );
        assert_eq!(
            denial.exact_rules().unwrap(),
            vec![
                "Bash(grep -rn \"HELP_CENTER; VARIATION\" client/)".to_owned(),
                "Bash(head -5)".to_owned(),
                "Bash(yarn eslint 'client/a b.tsx' 2>&1)".to_owned(),
                "Bash(tail -20)".to_owned(),
            ],
        );
    }

    /// Shell the parser will not read stays ungrantable, and says which
    /// command it refused so the operator can answer instead.
    #[test]
    fn unreadable_bash_is_still_refused_by_name() {
        let denial = bash("rm -rf \"$(cat target)\"");
        let Err(ClaudeProviderError::UnsafePermission(message)) = denial.exact_rules() else {
            panic!("command substitution must not be grantable");
        };
        assert!(message.contains("rm -rf \"$(cat target)\""), "{message}");
        for unreadable in [
            "(cd /repo && make)",
            "cat <<'EOF' > f\nbody\nEOF",
            "echo `whoami`",
        ] {
            assert!(
                bash(unreadable).exact_rules().is_err(),
                "must refuse: {unreadable}"
            );
        }
    }

    /// A rule may not carry the syntax that ends it, or a wildcard that would
    /// widen it past the call that was actually denied.
    #[test]
    fn rule_syntax_in_a_segment_makes_the_whole_line_ungrantable() {
        assert!(bash("ls *.rs; echo done").exact_rules().is_err());
        assert!(bash("ls; ls ?").exact_rules().is_err());
    }

    /// A wildcard refusal names the wildcard and the part carrying it, not
    /// shell the parser read perfectly well: the operator has to know what
    /// to ask the agent to change.
    #[test]
    fn a_wildcard_refusal_names_the_wildcard_and_its_part() {
        let denial = bash(
            "gh pr diff 1 --name-only; git diff trunk...HEAD -- 'client/pages/*' ; ls ../wpcom 2>/dev/null | head -3",
        );
        let Err(ClaudeProviderError::UnsafePermission(message)) = denial.exact_rules() else {
            panic!("a wildcard must not be grantable");
        };
        assert!(
            message.contains("'*' is permission-rule syntax")
                && message.contains("in: git diff trunk...HEAD -- 'client/pages/*'"),
            "{message}"
        );
        assert!(!message.contains("subshell"), "{message}");
    }

    /// An MCP call is grantable as its own tool name: the rule names that one
    /// tool on that one server, which is exactly the denied call.
    #[test]
    fn mcp_tool_is_granted_by_its_own_name() {
        let denial = tool(
            "mcp__linear-server__get_issue",
            serde_json::json!({ "id": "DOTSUP-99" }),
        );
        assert!(denial.is_grantable());
        assert_eq!(
            denial.exact_rules().unwrap(),
            vec!["mcp__linear-server__get_issue".to_owned()],
        );
    }

    /// The tool name still goes through the rule-value gate: a name carrying
    /// rule syntax would end the rule early and grant something else.
    #[test]
    fn mcp_tool_name_with_rule_syntax_is_refused() {
        assert!(
            tool("mcp__server__do(*)", serde_json::json!({}))
                .exact_rules()
                .is_err()
        );
    }

    #[test]
    fn failed_result_with_denials_still_carries_error() {
        let raw = b"{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"result\":\"\",\"permission_denials\":[{\"tool_name\":\"Bash\",\"tool_use_id\":\"t\",\"tool_input\":{\"command\":\"cargo test\"}}]}\n";
        let Some((ClaudeRecord::Result { success, error, .. }, _)) = first_record(raw).unwrap()
        else {
            panic!()
        };
        assert!(!success);
        assert_eq!(error.as_deref(), Some("error_max_turns"));
    }
}
