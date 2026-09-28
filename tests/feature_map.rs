//! Guards `docs/features/` against mechanical drift.
//!
//! The Feature Map promises that every path it cites exists, every command
//! and flag in its `Driving it` blocks is accepted by the binary, and every
//! key it names is one the TUI actually maps. These tests hold that promise
//! so a rename or a removed flag turns the map red instead of stale. Two of
//! them read the other way round: a command the binary offers or a key the
//! TUI binds that no feature file mentions is a coverage hole, and holes are
//! how a map goes quietly out of date while every line in it stays true.
//! Semantic drift (a gotcha that no longer applies, a missing sub-feature)
//! is still a reading job; see `docs/features/README.md`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn feature_files() -> Vec<(PathBuf, String)> {
    let dir = repo_root().join("docs/features");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("docs/features exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .map(|path| {
            let text = fs::read_to_string(&path).expect("feature file is readable");
            (path, text)
        })
        .collect();
    files.sort();
    assert!(files.len() > 1, "the Feature Map has no feature files");
    files
}

/// Every `` `token` `` in the text.
fn backtick_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split('`').skip(1).step_by(2)
}

/// Lines inside fenced bash code blocks.
fn bash_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("```") {
            inside = !inside && line.trim_start_matches('`').trim() == "bash";
            continue;
        }
        if inside {
            lines.push(line);
        }
    }
    lines
}

#[test]
fn every_cited_path_exists() {
    const PREFIXES: [&str; 5] = ["src/", "tests/", "evals/", ".github/", "docs/"];
    let root = repo_root();
    let mut missing = Vec::new();
    for (file, text) in feature_files() {
        for token in backtick_tokens(&text) {
            let candidate = token.split_whitespace().next().unwrap_or("");
            if !PREFIXES.iter().any(|prefix| candidate.starts_with(prefix)) {
                continue;
            }
            let cited = candidate.trim_end_matches(':');
            if !root.join(cited).exists() {
                missing.push(format!("{}: `{cited}`", file.display()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "paths cited by the Feature Map do not exist:\n{}",
        missing.join("\n")
    );
}

struct Invocation {
    subcommands: Vec<String>,
    flags: BTreeSet<String>,
}

/// Parses one `senate ...` line into the subcommand path and the long
/// flags it mentions. Comments, placeholders and bracketed alternatives are
/// ignored; only what the binary must recognise survives.
fn parse_invocation(line: &str) -> Option<Invocation> {
    let line = line.split('#').next()?.trim();
    let mut tokens = line.split_whitespace();
    if tokens.next()? != "senate" {
        return None;
    }
    let mut subcommands = Vec::new();
    let mut flags = BTreeSet::new();
    let mut reading_subcommands = true;
    for token in tokens {
        for piece in token.split(['[', ']', '|', '=']) {
            if piece.starts_with("--") {
                flags.insert(piece.to_owned());
                reading_subcommands = false;
            }
        }
        let is_word = token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if reading_subcommands && is_word && !token.starts_with('-') && subcommands.len() < 2 {
            subcommands.push(token.to_owned());
        } else {
            reading_subcommands = false;
        }
    }
    Some(Invocation { subcommands, flags })
}

fn help_text(subcommands: &[String]) -> Option<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_senate"))
        .args(subcommands)
        .arg("--help")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn every_driving_command_and_flag_is_accepted_by_the_binary() {
    let mut failures = Vec::new();
    for (file, text) in feature_files() {
        for line in bash_lines(&text) {
            let Some(invocation) = parse_invocation(line) else {
                continue;
            };
            let mut subcommands = invocation.subcommands.clone();
            let mut help = help_text(&subcommands);
            while help.is_none() && subcommands.pop().is_some() {
                help = help_text(&subcommands);
            }
            if subcommands != invocation.subcommands {
                failures.push(format!(
                    "{}: `{line}` — `senate {}` is not a command",
                    file.display(),
                    invocation.subcommands.join(" ")
                ));
                continue;
            }
            let help = help.expect("`senate --help` itself must succeed");
            for flag in &invocation.flags {
                if !help.contains(flag.as_str()) {
                    failures.push(format!(
                        "{}: `{line}` — `{flag}` is not a flag of `senate {}`",
                        file.display(),
                        subcommands.join(" ")
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "Feature Map commands the binary rejects:\n{}",
        failures.join("\n")
    );
}

/// The one printable character a `` `x` `` token names, if it names one.
fn single_key(token: &str) -> Option<char> {
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(key), None) if key.is_ascii_graphic() => Some(key),
        _ => None,
    }
}

/// Subcommand names the binary offers under `path`, read from its own help.
/// Continuation lines of a wrapped description are indented past the name
/// column, so only lines indented exactly two spaces carry a name.
fn subcommands_of(path: &[String]) -> Vec<String> {
    let Some(help) = help_text(path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut inside = false;
    for line in help.lines() {
        if line.starts_with("Commands:") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with("  ") || line.starts_with("   ") {
            break;
        }
        match line.split_whitespace().next() {
            Some("help") | None => {}
            Some(name) => names.push(name.to_owned()),
        }
    }
    names
}

/// The map is only as good as its coverage: a command the binary offers and
/// the map never mentions is drift the other tests cannot see, because they
/// only check that what the map says is true.
#[test]
fn every_command_the_binary_offers_appears_in_the_map() {
    let files = feature_files();
    let mut missing = Vec::new();
    let mut pending = vec![Vec::<String>::new()];
    while let Some(path) = pending.pop() {
        for name in subcommands_of(&path) {
            let mut child = path.clone();
            child.push(name);
            let invocation = format!("senate {}", child.join(" "));
            if !files.iter().any(|(_, text)| text.contains(&invocation)) {
                missing.push(invocation);
            }
            pending.push(child);
        }
    }
    assert!(
        missing.is_empty(),
        "commands the binary offers that no feature file mentions:\n{}",
        missing.join("\n")
    );
}

/// Same coverage rule for the control room: a key the TUI binds and the map
/// never names is a feature an agent cannot reach.
#[test]
fn every_key_the_tui_binds_appears_in_the_control_room_map() {
    let root = repo_root();
    let input = fs::read_to_string(root.join("src/tui/input.rs")).expect("input.rs exists");
    let bindings = input.split("#[cfg(test)]").next().unwrap_or_default();
    let doc = fs::read_to_string(root.join("docs/features/control-room.md"))
        .expect("control-room.md exists");
    let documented: BTreeSet<char> = backtick_tokens(&doc).filter_map(single_key).collect();
    let mut undocumented = BTreeSet::new();
    for binding in bindings.split("KeyCode::Char('").skip(1) {
        let Some(key) = binding.chars().next() else {
            continue;
        };
        if key.is_ascii_graphic() && !documented.contains(&key) {
            undocumented.insert(key);
        }
    }
    assert!(
        undocumented.is_empty(),
        "src/tui/input.rs binds keys control-room.md never names: {undocumented:?}"
    );
}

#[test]
fn every_key_in_the_control_room_map_is_bound() {
    let root = repo_root();
    let doc = fs::read_to_string(root.join("docs/features/control-room.md"))
        .expect("control-room.md exists");
    let input = fs::read_to_string(root.join("src/tui/input.rs")).expect("input.rs exists");
    let mut unbound = BTreeSet::new();
    for key in backtick_tokens(&doc).filter_map(single_key) {
        if !input.contains(&format!("KeyCode::Char('{key}')")) {
            unbound.insert(key);
        }
    }
    assert!(
        unbound.is_empty(),
        "control-room.md names keys src/tui/input.rs does not bind: {unbound:?}"
    );
}

#[test]
fn feature_map_parser_reads_the_documented_shapes() {
    let inv = parse_invocation(
        r#"senate fast "<task>" [--repo <path>] [--provider claude|codex|fake | --profile recommended] [--effort native|low|medium|high|xhigh]"#,
    )
    .unwrap();
    assert_eq!(inv.subcommands, ["fast"]);
    assert_eq!(
        inv.flags.iter().collect::<Vec<_>>(),
        ["--effort", "--profile", "--provider", "--repo"]
    );

    let inv = parse_invocation("senate eval run --provider fake   # suite defaults").unwrap();
    assert_eq!(inv.subcommands, ["eval", "run"]);
    assert_eq!(inv.flags.iter().collect::<Vec<_>>(), ["--provider"]);

    let inv =
        parse_invocation("senate resolve <run-id> <attention-id> --response \"<x>\"").unwrap();
    assert_eq!(inv.subcommands, ["resolve"]);
    assert_eq!(inv.flags.iter().collect::<Vec<_>>(), ["--response"]);

    assert!(parse_invocation("curl -fsSL https://example | sh").is_none());
    assert!(Path::new(env!("CARGO_BIN_EXE_senate")).exists());
}
