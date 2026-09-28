//! Bounded terminal Markdown renderer for artifact viewing.
//!
//! Renders the subset that appears in The Senate artifacts (headings, bold,
//! italic, inline code, fenced code, lists, blockquotes, separators) into
//! Ratatui lines. Presentation only: the persisted artifact stays raw
//! Markdown. Malformed or partial input degrades to literal text and never
//! panics.

use super::theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

const CODE_INDENT: &str = "  ";

pub(crate) fn render_markdown(source: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_fence = false;
    for source_line in source.lines() {
        // Artifacts are agent output: sanitize once here so no control
        // character reaches a span, however the line is later classified.
        let owned = super::format::viewer_line(source_line);
        let raw = owned.as_str();
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            lines.push(Line::from(Span::styled(
                format!("{CODE_INDENT}{raw}"),
                Style::default().fg(theme::accent()),
            )));
            continue;
        }
        lines.push(block_line(raw, trimmed));
    }
    lines
}

fn block_line(raw: &str, trimmed: &str) -> Line<'static> {
    if trimmed.is_empty() {
        return Line::from("");
    }
    if is_separator(trimmed) {
        return Line::from(Span::styled(
            "─".repeat(32),
            Style::default().fg(theme::muted_color()),
        ));
    }
    if let Some(heading) = heading_line(trimmed) {
        return heading;
    }
    if let Some(rest) = trimmed
        .strip_prefix("> ")
        .or_else(|| trimmed.strip_prefix('>'))
    {
        let mut spans = vec![Span::styled(
            "│ ",
            Style::default().fg(theme::muted_color()),
        )];
        spans.extend(inline_spans(
            rest,
            Style::default()
                .fg(theme::muted_color())
                .add_modifier(Modifier::ITALIC),
        ));
        return Line::from(spans);
    }
    if let Some(rest) = unordered_item(trimmed) {
        let mut spans = vec![Span::raw(format!("{}• ", indent_of(raw)))];
        spans.extend(inline_spans(rest, Style::default()));
        return Line::from(spans);
    }
    if let Some((number, rest)) = ordered_item(trimmed) {
        let mut spans = vec![Span::raw(format!("{}{number} ", indent_of(raw)))];
        spans.extend(inline_spans(rest, Style::default()));
        return Line::from(spans);
    }
    Line::from(inline_spans(raw, Style::default()))
}

fn heading_line(trimmed: &str) -> Option<Line<'static>> {
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let text = trimmed[hashes..].strip_prefix(' ')?;
    let style = if hashes <= 2 {
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Some(Line::from(Span::styled(text.to_owned(), style)))
}

fn is_separator(trimmed: &str) -> bool {
    trimmed.len() >= 3
        && (trimmed.chars().all(|character| character == '-')
            || trimmed.chars().all(|character| character == '*')
            || trimmed.chars().all(|character| character == '_'))
}

fn unordered_item(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
}

fn ordered_item(trimmed: &str) -> Option<(&str, &str)> {
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let marker = trimmed.get(..=digits)?;
    if !marker.ends_with('.') && !marker.ends_with(')') {
        return None;
    }
    let rest = trimmed.get(digits + 1..)?.strip_prefix(' ')?;
    Some((marker, rest))
}

fn indent_of(raw: &str) -> String {
    raw.chars()
        .take_while(|character| character.is_whitespace())
        .map(|_| ' ')
        .collect()
}

fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut remainder = text;
    while let Some(index) = remainder.find(['`', '*']) {
        let (before, at) = remainder.split_at(index);
        plain.push_str(before);
        if let Some(after) = at.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                flush(&mut spans, &mut plain, base);
                spans.push(Span::styled(
                    after[..end].to_owned(),
                    base.fg(theme::attention()),
                ));
                remainder = &after[end + 1..];
            } else {
                plain.push('`');
                remainder = after;
            }
        } else if let Some(after) = at.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                flush(&mut spans, &mut plain, base);
                spans.push(Span::styled(
                    after[..end].to_owned(),
                    base.add_modifier(Modifier::BOLD),
                ));
                remainder = &after[end + 2..];
            } else {
                plain.push_str("**");
                remainder = after;
            }
        } else if let Some(after) = at.strip_prefix('*') {
            if let Some(end) = after.find('*') {
                flush(&mut spans, &mut plain, base);
                spans.push(Span::styled(
                    after[..end].to_owned(),
                    base.add_modifier(Modifier::ITALIC),
                ));
                remainder = &after[end + 1..];
            } else {
                plain.push('*');
                remainder = after;
            }
        } else {
            remainder = at;
        }
    }
    plain.push_str(remainder);
    flush(&mut spans, &mut plain, base);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn flush(spans: &mut Vec<Span<'static>>, plain: &mut String, base: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), base));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(source: &str) -> String {
        render_markdown(source)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headings_render_without_literal_hashes() {
        let text = rendered_text("## Design\n### Detail");
        assert!(text.contains("Design"));
        assert!(text.contains("Detail"));
        assert!(!text.contains('#'));
        let lines = render_markdown("# Top");
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(theme::accent()),
            "H1 is prominent"
        );
    }

    #[test]
    fn bold_renders_styled_without_literal_asterisks() {
        let lines = render_markdown("state is **verified** here");
        let text = rendered_text("state is **verified** here");
        assert!(text.contains("verified"));
        assert!(!text.contains("**"));
        let bold = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "verified")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inline_code_is_styled_without_backticks() {
        let lines = render_markdown("run `cargo test` now");
        let code = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "cargo test")
            .unwrap();
        assert_eq!(code.style.fg, Some(theme::attention()));
        assert!(!rendered_text("run `cargo test` now").contains('`'));
    }

    #[test]
    fn fenced_code_hides_fences_and_keeps_content_verbatim() {
        let source = "```rust\nlet **x** = 1;\n```";
        let text = rendered_text(source);
        assert!(!text.contains("```"));
        assert!(
            text.contains("let **x** = 1;"),
            "code content is not inline-parsed"
        );
    }

    #[test]
    fn lists_render_readable_bullets_and_numbers() {
        let text = rendered_text("- first\n* second\n1. third\n2) fourth");
        assert!(text.contains("• first"));
        assert!(text.contains("• second"));
        assert!(text.contains("1. third"));
        assert!(text.contains("2) fourth"));
    }

    #[test]
    fn blockquote_and_separator_render() {
        let text = rendered_text("> quoted\n---");
        assert!(text.contains("│ quoted"));
        assert!(text.contains('─'));
        assert!(!text.contains("---"));
    }

    #[test]
    fn malformed_partial_markdown_never_panics_and_degrades_to_literal() {
        for source in [
            "**unclosed bold",
            "`unclosed code",
            "*",
            "**",
            "```\nunclosed fence",
            "#",
            "####### seven",
            "1.",
            "> ",
            "",
            "text `a` **b** *c* mixed `d",
        ] {
            let _ = render_markdown(source);
        }
        assert!(rendered_text("**unclosed bold").contains("**unclosed bold"));
        assert!(rendered_text("`unclosed code").contains("`unclosed code"));
        assert!(rendered_text("####### seven").contains("####### seven"));
    }

    #[test]
    fn unicode_content_is_preserved() {
        let text = rendered_text("- caffè **è** `più`");
        assert!(text.contains("caffè"));
        assert!(text.contains('è'));
        assert!(text.contains("più"));
    }
}
