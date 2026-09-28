//! Deterministic operational formatters for the Mission Deck.
//!
//! Durations render as human spans (never milliseconds) and provider-native
//! units render compactly. Unit values stay provider-native: they are never
//! normalized across providers and never imply cost.

use chrono::{DateTime, TimeDelta, Utc};

/// Wall-clock span of a stage or run, resolved against `now` for spans that
/// are still open. Returns `None` when the run carries no timing evidence,
/// so callers render nothing rather than a fabricated `0s`.
pub(crate) fn elapsed(
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<TimeDelta> {
    let started = started_at?;
    let end = finished_at.unwrap_or(now);
    Some(end.signed_duration_since(started).max(TimeDelta::zero()))
}

/// Human span: `<1s`, `14s`, `1m 08s`, `12m 41s`, `1h 04m`.
pub(crate) fn format_duration(span: TimeDelta) -> String {
    let total = span.num_seconds().max(0);
    if total == 0 {
        return "<1s".to_owned();
    }
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Prominent clock for the hero line: `04:32`, `1:02:48`.
pub(crate) fn format_clock(span: TimeDelta) -> String {
    let total = span.num_seconds().max(0);
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Compact provider-native unit count: exact below 1000, then `1.2k`, `2.3M`.
/// Integer arithmetic throughout, so large counts never lose precision to a
/// float conversion; the fraction truncates rather than rounds, which keeps
/// the displayed figure from ever overstating reported usage.
pub(crate) fn format_units(units: u64) -> String {
    let scaled = |divisor: u64, suffix: char| {
        format!(
            "{}.{}{suffix}",
            units / divisor,
            (units % divisor) / (divisor / 10)
        )
    };
    if units < 1_000 {
        units.to_string()
    } else if units < 1_000_000 {
        scaled(1_000, 'k')
    } else {
        scaled(1_000_000, 'M')
    }
}

/// Concise repository identity for operational rows; the full path stays in
/// technical mode.
/// One commit hash cut to the length a status line can carry.
pub(crate) fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

pub(crate) fn repository_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Safely shortens a task title to one line of at most `limit` characters.
pub(crate) fn truncate_title(task: &str, limit: usize) -> String {
    let first = task
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(task)
        .trim();
    let mut characters = first.chars();
    let head: String = characters.by_ref().take(limit).collect();
    if characters.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Columns a tab advances to, matching the terminal default.
const TAB_STOP: usize = 8;

/// Makes one line of external text safe to place in a terminal cell buffer.
///
/// Viewer content is not ours: diffs, artifacts, and provider logs carry
/// whatever the repository and the agent produced. Control characters must
/// never reach the buffer. Ratatui measures a cell by its display width, and
/// a control character measures zero, so it is written without advancing the
/// column — but the terminal still acts on it, moving the cursor. The two
/// disagree, every later cell lands in the wrong place, and the diff-based
/// repaint can no longer erase what it drew: content stays on screen after
/// the viewer is closed. Tabs expand to the next tab stop so alignment
/// survives; every other control character becomes a visible placeholder so
/// the line keeps its shape and nothing is silently dropped.
pub(crate) fn viewer_line(line: &str) -> String {
    if !line.chars().any(|character| {
        character.is_control() || matches!(character, '\u{7f}'..='\u{9f}' | '\u{200b}'..='\u{200f}')
    }) {
        return line.to_owned();
    }
    let mut safe = String::with_capacity(line.len());
    let mut column = 0usize;
    for character in line.chars() {
        match character {
            '\t' => {
                let spaces = TAB_STOP - (column % TAB_STOP);
                safe.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            character
                if character.is_control()
                    || matches!(character, '\u{7f}'..='\u{9f}' | '\u{200b}'..='\u{200f}') =>
            {
                safe.push('\u{fffd}');
                column += 1;
            }
            character => {
                safe.push(character);
                column += 1;
            }
        }
    }
    safe
}

/// Whether `truncate_title` had to drop anything: a task longer than the
/// rail, or one carrying more lines than the single one the rail shows.
#[must_use]
pub(crate) fn title_overflows(task: &str, limit: usize) -> bool {
    let mut prose = task.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = prose.next() else {
        return false;
    };
    first.trim().chars().count() > limit || prose.next().is_some()
}

/// The whole task, wrapped to `width`, for the expanded rail.
///
/// Wraps on whitespace and keeps the author's own line breaks, so a task
/// pasted as a list still reads as one. A word longer than the rail (a URL,
/// the usual case) is broken rather than allowed to overflow it.
#[must_use]
pub(crate) fn wrap_title(task: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped: Vec<String> = Vec::new();
    for line in task.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            // Collapse the author's blank runs to one separating row.
            if wrapped.last().is_some_and(|blank| !blank.is_empty()) {
                wrapped.push(String::new());
            }
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            let mut word = word;
            while word.chars().count() > width {
                if !current.is_empty() {
                    wrapped.push(std::mem::take(&mut current));
                }
                let head: String = word.chars().take(width).collect();
                let taken = head.len();
                wrapped.push(head);
                word = &word[taken..];
            }
            let extra = usize::from(!current.is_empty());
            if current.chars().count() + extra + word.chars().count() > width {
                wrapped.push(std::mem::take(&mut current));
            } else if extra == 1 {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() {
            wrapped.push(current);
        }
    }
    while wrapped.last().is_some_and(String::is_empty) {
        wrapped.pop();
    }
    wrapped
}

#[cfg(test)]
mod tests {
    #[test]
    fn wrapping_keeps_every_word_and_never_exceeds_the_rail() {
        let task =
            "Build a static landing page (index.html + styles.css) for a hiking-trail company";
        let wrapped = wrap_title(task, 30);
        assert!(wrapped.iter().all(|line| line.chars().count() <= 30));
        assert_eq!(wrapped.join(" "), task, "no word is lost or duplicated");
    }

    /// The author's own line breaks are structure, not accidents: a task
    /// pasted as a list stays a list.
    #[test]
    fn wrapping_keeps_the_authors_line_breaks_and_collapses_blank_runs() {
        let wrapped = wrap_title("Do the thing\n\n\n- first\n- second\n", 40);
        assert_eq!(wrapped, vec!["Do the thing", "", "- first", "- second"]);
    }

    /// A URL is the usual word longer than the rail; breaking it is ugly but
    /// it is the only option that keeps the rail's width honest.
    #[test]
    fn a_word_longer_than_the_rail_is_broken_rather_than_overflowing_it() {
        let wrapped = wrap_title(
            "see https://github.com/Automattic/wp-calypso/pull/12345",
            20,
        );
        assert!(wrapped.iter().all(|line| line.chars().count() <= 20));
        assert_eq!(
            wrapped.concat().replace("see", ""),
            "https://github.com/Automattic/wp-calypso/pull/12345"
        );
    }

    #[test]
    fn overflow_is_reported_for_a_long_line_or_a_second_one_only() {
        assert!(!title_overflows("short", 20));
        assert!(title_overflows(
            "a task far longer than the rail is wide",
            20
        ));
        // truncate_title shows the first line only, so a second one is lost
        // even when the first fits.
        assert!(title_overflows("short\nbut there is more", 20));
        assert!(!title_overflows("short\n\n   ", 20));
        assert!(!title_overflows("", 20));
    }

    use chrono::TimeZone as _;

    use super::*;

    fn at(hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, hour, minute, second)
            .single()
            .unwrap()
    }

    #[test]
    fn duration_formatter_covers_boundaries() {
        assert_eq!(format_duration(TimeDelta::milliseconds(400)), "<1s");
        assert_eq!(format_duration(TimeDelta::seconds(0)), "<1s");
        assert_eq!(format_duration(TimeDelta::seconds(1)), "1s");
        assert_eq!(format_duration(TimeDelta::seconds(14)), "14s");
        assert_eq!(format_duration(TimeDelta::seconds(59)), "59s");
        assert_eq!(format_duration(TimeDelta::seconds(60)), "1m 00s");
        assert_eq!(format_duration(TimeDelta::seconds(68)), "1m 08s");
        assert_eq!(format_duration(TimeDelta::seconds(761)), "12m 41s");
        assert_eq!(format_duration(TimeDelta::seconds(3600)), "1h 00m");
        assert_eq!(format_duration(TimeDelta::seconds(3840)), "1h 04m");
        assert_eq!(format_duration(TimeDelta::seconds(8280)), "2h 18m");
        assert_eq!(
            format_duration(TimeDelta::seconds(-5)),
            "<1s",
            "negative spans clamp instead of panicking"
        );
    }

    #[test]
    fn clock_formatter_is_prominent_and_stable() {
        assert_eq!(format_clock(TimeDelta::seconds(0)), "00:00");
        assert_eq!(format_clock(TimeDelta::seconds(272)), "04:32");
        assert_eq!(format_clock(TimeDelta::seconds(768)), "12:48");
        assert_eq!(format_clock(TimeDelta::seconds(3768)), "1:02:48");
    }

    #[test]
    fn running_span_measures_from_persisted_start_not_tui_open() {
        let started = at(12, 0, 0);
        let span = elapsed(Some(started), None, at(12, 4, 32)).unwrap();
        assert_eq!(format_clock(span), "04:32");
        // A detach and reopen much later keeps measuring from the same
        // persisted start.
        let span = elapsed(Some(started), None, at(13, 2, 48)).unwrap();
        assert_eq!(format_clock(span), "1:02:48");
    }

    #[test]
    fn completed_span_uses_persisted_finish_and_ignores_now() {
        let span = elapsed(Some(at(12, 0, 0)), Some(at(12, 2, 14)), at(18, 0, 0)).unwrap();
        assert_eq!(format_duration(span), "2m 14s");
    }

    #[test]
    fn missing_timing_evidence_is_absent_not_zero() {
        assert!(elapsed(None, None, at(12, 0, 0)).is_none());
        assert!(
            elapsed(None, Some(at(12, 0, 0)), at(12, 1, 0)).is_none(),
            "a finish without a start carries no measurable span"
        );
    }

    #[test]
    fn unit_formatter_keeps_provider_native_scale() {
        assert_eq!(format_units(0), "0");
        assert_eq!(format_units(999), "999");
        assert_eq!(format_units(1_000), "1.0k");
        assert_eq!(format_units(1_234), "1.2k");
        assert_eq!(
            format_units(12_288),
            "12.2k",
            "the fraction truncates so the figure never overstates usage"
        );
        assert_eq!(format_units(38_400), "38.4k");
        assert_eq!(format_units(999_999), "999.9k");
        assert_eq!(format_units(2_310_442), "2.3M");
        assert_eq!(
            format_units(u64::MAX),
            "18446744073709.5M",
            "integer scaling keeps huge counts exact"
        );
    }

    #[test]
    fn repository_identity_prefers_basename() {
        assert_eq!(
            repository_name(std::path::Path::new("/Users/e/Code/wp-calypso-2")),
            "wp-calypso-2"
        );
        assert_eq!(repository_name(std::path::Path::new("/")), "/");
    }

    #[test]
    fn titles_truncate_on_character_boundaries() {
        assert_eq!(truncate_title("  Add OAuth  \nmore", 40), "Add OAuth");
        assert_eq!(truncate_title("caffè è più", 5), "caffè…");
        assert_eq!(truncate_title("", 10), "");
    }

    #[test]
    fn viewer_line_expands_tabs_and_neutralises_control_characters() {
        // Tabs align to the next stop, measured from the start of the line.
        assert_eq!(viewer_line("\tx"), "        x");
        assert_eq!(viewer_line("ab\tc"), "ab      c");
        assert_eq!(viewer_line("abcdefgh\tc"), "abcdefgh        c");

        // Everything else that would move the cursor becomes one visible cell,
        // so the line keeps its shape instead of silently losing content.
        assert_eq!(viewer_line("a\u{1b}[31mb"), "a\u{fffd}[31mb");
        assert_eq!(viewer_line("a\rb"), "a\u{fffd}b");
        assert_eq!(viewer_line("a\u{0}b"), "a\u{fffd}b");

        // Ordinary text, including wide and accented characters, is untouched.
        assert_eq!(viewer_line("fn main() {}"), "fn main() {}");
        assert_eq!(viewer_line("caffè 日本語"), "caffè 日本語");
        assert_eq!(viewer_line(""), "");
    }
}
