//! The Senate palette and the small set of surfaces built from it.
//!
//! Every color the TUI paints comes from here, so semantic meaning stays
//! stable across screens: one accent per screen plus semantic exceptions.
//! State is never communicated by color alone — each surface pairs its color
//! with a glyph or a word, so a monochrome terminal reads the same.

use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// What the terminal can actually paint. Detected once at startup; the
/// palette is chosen from it, so nothing downstream has to ask again.
///
/// Capability is not taste. It says what is renderable, never what should be
/// rendered — that is [`ThemeChoice`]. A terminal announcing truecolor has
/// not asked The Senate to stop using its own theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorCapability {
    /// No color at all: `NO_COLOR`, a dumb terminal, or output the user has
    /// asked to keep plain. Every semantic token collapses to the terminal's
    /// own foreground, so meaning has to survive on glyphs and words alone.
    Mono,
    /// The sixteen ANSI colors, named rather than specified — they resolve to
    /// whatever the user's terminal theme defines, so The Senate sits inside
    /// their palette instead of imposing one.
    Ansi16,
    /// Twenty-four bit color is available. Only [`ThemeChoice::Vivid`] spends
    /// it; on its own it changes nothing.
    TrueColor,
}

impl ColorCapability {
    /// Resolves capability from the environment, taking the values rather
    /// than reading them, so the decision is testable without mutating the
    /// process environment.
    ///
    /// `NO_COLOR` follows the convention at no-color.org: present and
    /// non-empty disables color regardless of its value, and it outranks any
    /// capability the terminal advertises.
    pub(crate) fn resolve(
        no_color: Option<&str>,
        term: Option<&str>,
        colorterm: Option<&str>,
    ) -> Self {
        if no_color.is_some_and(|value| !value.is_empty()) || term == Some("dumb") {
            return Self::Mono;
        }
        match colorterm.map(str::trim) {
            Some("truecolor" | "24bit") => Self::TrueColor,
            _ => Self::Ansi16,
        }
    }

    /// Reads the process environment. Kept separate from `resolve` so the
    /// policy stays a pure function.
    pub(crate) fn from_environment() -> Self {
        Self::resolve(
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
        )
    }
}

/// Which materialisation of the one design system to paint. Both spell the
/// same eight meanings; they disagree only about who owns the hue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ThemeChoice {
    /// The default. Named ANSI colors, so every token resolves to whatever
    /// the user's terminal theme defines and The Senate sits inside their
    /// aesthetic instead of imposing one. Stays ANSI even where truecolor is
    /// available: a capability is not permission to take the screen over.
    #[default]
    Native,
    /// The Senate's own colors, specified rather than named. Asked for
    /// explicitly, and only honoured where the terminal can render them.
    Vivid,
}

impl ThemeChoice {
    pub(crate) fn resolve(theme: Option<&str>) -> Self {
        match theme.map(str::trim) {
            Some("vivid") => Self::Vivid,
            // Anything else — unset, empty, misspelt — is the default. An
            // unreadable value must not silently restyle the interface.
            _ => Self::Native,
        }
    }

    pub(crate) fn from_environment() -> Self {
        Self::resolve(std::env::var("SENATE_THEME").ok().as_deref())
    }
}

/// The eight semantic tokens, materialised for one capability. Code names the
/// meaning; only this struct knows the hue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    accent: Color,
    success: Color,
    attention: Color,
    danger: Color,
    suspended: Color,
    muted: Color,
    structure: Color,
    paper: Color,
    chip_fg: Color,
}

impl Palette {
    /// The palette to paint with. Capability decides what is renderable and
    /// the choice decides what to render, and the two only meet here.
    ///
    /// Mono outranks everything: a user who asked for no colour does not get
    /// The Senate's own. Native stays ANSI even on a truecolor terminal, and
    /// Vivid on a terminal that cannot render it falls back to the same ANSI
    /// palette rather than to approximated hues — the meanings survive, only
    /// the specific colours are lost.
    pub(crate) const fn resolve(capability: ColorCapability, theme: ThemeChoice) -> Self {
        match (capability, theme) {
            (ColorCapability::Mono, _) => Self::MONO,
            (ColorCapability::TrueColor, ThemeChoice::Vivid) => Self::VIVID,
            (ColorCapability::Ansi16 | ColorCapability::TrueColor, ThemeChoice::Native)
            | (ColorCapability::Ansi16, ThemeChoice::Vivid) => Self::ANSI16,
        }
    }

    const ANSI16: Self = {
        Self {
            // Primary The Senate accent: active work, current stage,
            // product identity.
            accent: Color::Cyan,
            // Completion, verified artifacts, apply.
            success: Color::Green,
            // The run needs the user. Never used for failure.
            attention: Color::Yellow,
            // Failure and destructive actions.
            danger: Color::Red,
            // Suspended work: paused and interrupted.
            suspended: Color::Magenta,
            // Secondary labels, metadata, structure, inactive connectors.
            muted: Color::DarkGray,
            // Structural framing and the architecture role accent. Kept
            // distinct from the accent so framework reads as structure,
            // not focus, and never competes with the one active-work
            // accent per screen.
            structure: Color::Blue,
            // Light material detail in sprite art: paper, page edges,
            // the brim of a hard hat. Brighter than muted, quieter than
            // the terminal's own foreground.
            paper: Color::Gray,
            // Foreground of a reversed chip: black on the chip's own
            // color keeps its word legible on any accent hue.
            chip_fg: Color::Black,
        }
    };

    /// The Senate's own colours, specified rather than named.
    ///
    /// Every hue is constrained rather than chosen freely: each one holds at
    /// least a 3:1 contrast ratio against both a black and a white terminal
    /// background, because a specified colour — unlike a named ANSI one —
    /// cannot adapt to the user's background and would otherwise vanish on
    /// half of them. `CHIP_FG` is exempt from that rule and bound by a
    /// stricter one: it is read *on* the other tokens, so it is measured
    /// against them instead. Tests enforce both.
    const VIVID: Self = {
        Self {
            accent: Color::Rgb(0x1B, 0x9A, 0xAA),
            success: Color::Rgb(0x2E, 0x9E, 0x4F),
            attention: Color::Rgb(0xB5, 0x7E, 0x00),
            danger: Color::Rgb(0xD1, 0x3A, 0x3F),
            suspended: Color::Rgb(0xA9, 0x55, 0xC7),
            // Quieter by saturation rather than by luminance: it has to
            // recede without becoming unreadable on a light background.
            muted: Color::Rgb(0x76, 0x7C, 0x84),
            structure: Color::Rgb(0x3B, 0x7D, 0xD8),
            // Neutral gray held inside the 3:1 band against both black
            // and white, so paper stays paper on either background.
            paper: Color::Rgb(0x8F, 0x8F, 0x8F),
            chip_fg: Color::Rgb(0x00, 0x00, 0x00),
        }
    };

    /// Every token is the terminal's own foreground. Surfaces that relied on
    /// hue alone become indistinguishable here — which is the point: the
    /// monochrome test reads this palette to prove the interface still tells
    /// the truth without color.
    const MONO: Self = {
        Self {
            accent: Color::Reset,
            success: Color::Reset,
            attention: Color::Reset,
            danger: Color::Reset,
            suspended: Color::Reset,
            muted: Color::Reset,
            structure: Color::Reset,
            paper: Color::Reset,
            // A reversed chip still inverts, so its text takes the
            // background the terminal paints behind it.
            chip_fg: Color::Reset,
        }
    };
}

thread_local! {
    /// The palette this thread paints with. Thread-local rather than global
    /// so a test can render one variant monochrome without disturbing the
    /// others; the TUI resolves it once at startup and never changes it.
    static PALETTE: std::cell::Cell<Palette> =
        const { std::cell::Cell::new(Palette::resolve(ColorCapability::Ansi16, ThemeChoice::Native)) };
}

/// Installs the palette for this thread. Called once at TUI startup.
pub(crate) fn set_palette(palette: Palette) {
    PALETTE.with(|current| current.set(palette));
}

/// Runs `body` with `palette` installed, restoring the previous one even if
/// `body` panics. Tests render monochrome without leaking that choice into
/// whatever else the harness runs on this thread.
#[cfg(test)]
pub(crate) fn with_palette<T>(palette: Palette, body: impl FnOnce() -> T) -> T {
    struct Restore(Palette);
    impl Drop for Restore {
        fn drop(&mut self) {
            set_palette(self.0);
        }
    }
    let _restore = Restore(self::palette());
    set_palette(palette);
    body()
}

fn palette() -> Palette {
    PALETTE.with(std::cell::Cell::get)
}

/// Primary The Senate accent: active work, current stage, product identity.
pub(crate) fn accent() -> Color {
    palette().accent
}
/// Completion, verified artifacts, apply.
pub(crate) fn success() -> Color {
    palette().success
}
/// The run needs the user. Never used for failure.
pub(crate) fn attention() -> Color {
    palette().attention
}
/// Failure and destructive actions.
pub(crate) fn danger() -> Color {
    palette().danger
}
/// Suspended work: paused and interrupted.
pub(crate) fn suspended() -> Color {
    palette().suspended
}
/// Secondary labels, metadata, structure, inactive pipeline connectors.
pub(crate) fn muted_color() -> Color {
    palette().muted
}
/// Structural framing and the architecture role accent.
pub(crate) fn structure() -> Color {
    palette().structure
}
/// Light material detail in sprite art: paper, pages, a hat brim.
pub(crate) fn paper() -> Color {
    palette().paper
}
/// Foreground of a reversed chip.
pub(crate) fn chip_fg() -> Color {
    palette().chip_fg
}

/// Whether a colour is one The Senate specified rather than one the terminal
/// theme resolves. Lives here so a test elsewhere can ask the question
/// without naming a hue, which the palette fence forbids.
#[cfg(test)]
pub(crate) const fn is_specified(color: Color) -> bool {
    matches!(color, Color::Rgb(..))
}

/// Primary content: the terminal's own foreground.
pub(crate) fn text() -> Style {
    Style::default()
}

/// Secondary content: labels, metadata, connectors, quiet hints.
pub(crate) fn muted() -> Style {
    Style::default().fg(muted_color())
}

/// Section label inside a panel — the only structural device that replaces a
/// border, so it must read as a heading without shouting.
pub(crate) fn section(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_owned(),
        Style::default()
            .fg(muted_color())
            .add_modifier(Modifier::BOLD),
    ))
}

/// The one strong surface: a reversed block of color carrying a short word.
/// Used at most once per screen, for the state that must not be missed.
pub(crate) fn chip(label: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default()
            .bg(color)
            .fg(chip_fg())
            .add_modifier(Modifier::BOLD),
    )
}

/// Keyboard affordance drawn as a button: the key is a reversed keycap in
/// the action's colour, the verb sits beside it as plain content.
pub(crate) fn action(key: &str, label: &str, color: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default()
                .bg(color)
                .fg(chip_fg())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}"), text()),
    ]
}

/// Horizontal rule used where a border would be too heavy.
pub(crate) fn rule(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(muted_color()),
    ))
}

/// Diff hunk header — the `diff --git` line and the `@@` range: structural
/// framing in bold so it reads as a boundary, not as changed content.
pub(crate) fn diff_hunk() -> Style {
    Style::default()
        .fg(structure())
        .add_modifier(Modifier::BOLD)
}

/// Centered short rule, used to seat POD under the pipeline instead of
/// letting it float in whatever space is left over.
pub(crate) fn centered_rule(width: u16) -> Line<'static> {
    let span = width.saturating_sub(4).clamp(4, 14);
    Line::from(Span::styled(
        "─".repeat(span as usize),
        Style::default().fg(muted_color()),
    ))
    .alignment(Alignment::Center)
}

/// One row with content pushed to both edges. Used for headers and footers
/// where the secondary half is quiet and droppable.
pub(crate) fn spread(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: u16,
) -> Line<'static> {
    let used: usize = left
        .iter()
        .chain(right.iter())
        .map(|span| span.content.chars().count())
        .sum();
    let gap = (width as usize).saturating_sub(used).max(1);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named_tokens(palette: Palette) -> [(&'static str, Color); 9] {
        [
            ("accent", palette.accent),
            ("success", palette.success),
            ("attention", palette.attention),
            ("danger", palette.danger),
            ("suspended", palette.suspended),
            ("muted", palette.muted),
            ("structure", palette.structure),
            ("paper", palette.paper),
            ("chip_fg", palette.chip_fg),
        ]
    }

    fn rgb(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(red, green, blue) => (red, green, blue),
            other => panic!("{other:?} is not a specified colour"),
        }
    }

    /// WCAG relative luminance, and the contrast ratio built from it. The
    /// floor this enforces is 3:1, the large-text/UI threshold: POD's art and
    /// the status glyphs are heavy, and holding 4.5:1 against both a black
    /// and a white background at once leaves almost no usable hues.
    fn luminance((red, green, blue): (u8, u8, u8)) -> f64 {
        fn channel(value: u8) -> f64 {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
    }

    fn contrast(one: (u8, u8, u8), other: (u8, u8, u8)) -> f64 {
        let (first, second) = (luminance(one), luminance(other));
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    #[test]
    fn no_color_and_dumb_terminals_resolve_to_monochrome() {
        // The no-color.org convention: present and non-empty, whatever the
        // value. An empty NO_COLOR is explicitly not a request for mono.
        assert_eq!(
            ColorCapability::resolve(Some("1"), None, None),
            ColorCapability::Mono
        );
        assert_eq!(
            ColorCapability::resolve(Some("0"), None, None),
            ColorCapability::Mono
        );
        assert_eq!(
            ColorCapability::resolve(Some(""), None, None),
            ColorCapability::Ansi16
        );
        assert_eq!(
            ColorCapability::resolve(None, None, None),
            ColorCapability::Ansi16
        );
        assert_eq!(
            ColorCapability::resolve(None, Some("xterm-256color"), None),
            ColorCapability::Ansi16
        );
        assert_eq!(
            ColorCapability::resolve(None, Some("dumb"), None),
            ColorCapability::Mono
        );
    }

    #[test]
    fn truecolor_is_detected_but_never_assumed_to_be_a_request() {
        assert_eq!(
            ColorCapability::resolve(None, None, Some("truecolor")),
            ColorCapability::TrueColor
        );
        assert_eq!(
            ColorCapability::resolve(None, None, Some("24bit")),
            ColorCapability::TrueColor
        );
        assert_eq!(
            ColorCapability::resolve(None, None, Some("")),
            ColorCapability::Ansi16
        );

        // A terminal that can paint anything has still not asked for
        // anything: on its own, truecolor changes no colour the user sees.
        assert_eq!(
            Palette::resolve(ColorCapability::TrueColor, ThemeChoice::Native),
            Palette::resolve(ColorCapability::Ansi16, ThemeChoice::Native),
            "a capability became a restyle without anyone asking"
        );

        // And a user who asked for no colour does not get The Senate's own,
        // however much the terminal advertises.
        assert_eq!(
            ColorCapability::resolve(Some("1"), None, Some("truecolor")),
            ColorCapability::Mono
        );
        assert_eq!(
            Palette::resolve(ColorCapability::Mono, ThemeChoice::Vivid),
            Palette::resolve(ColorCapability::Mono, ThemeChoice::Native)
        );
    }

    #[test]
    fn the_theme_understands_vivid_and_ignores_the_rest() {
        assert_eq!(ThemeChoice::resolve(Some("vivid")), ThemeChoice::Vivid);
        assert_eq!(ThemeChoice::resolve(Some(" vivid ")), ThemeChoice::Vivid);
        for value in [
            None,
            Some(""),
            Some("native"),
            Some("Vivid"),
            Some("bright"),
        ] {
            assert_eq!(
                ThemeChoice::resolve(value),
                ThemeChoice::Native,
                "{value:?} is not a recognised theme, so it must not restyle anything"
            );
        }
    }

    /// Vivid where it can be rendered, and the same meanings in named ANSI
    /// where it cannot — never approximated hues, which would be The Senate's
    /// colours badly rather than the terminal's colours well.
    #[test]
    fn vivid_is_honoured_only_where_it_can_be_rendered() {
        assert_eq!(
            Palette::resolve(ColorCapability::TrueColor, ThemeChoice::Vivid),
            Palette::VIVID
        );
        assert_eq!(
            Palette::resolve(ColorCapability::Ansi16, ThemeChoice::Vivid),
            Palette::ANSI16
        );
        assert_ne!(
            Palette::VIVID,
            Palette::ANSI16,
            "asking for vivid on a capable terminal has to change something"
        );
    }

    /// A named ANSI colour adapts to the user's terminal theme; a specified
    /// one cannot. So every vivid token has to survive both a black and a
    /// white background, or The Senate's own palette becomes the reason half
    /// its users cannot read it.
    #[test]
    fn every_vivid_token_stays_legible_on_a_dark_and_a_light_terminal() {
        for (name, token) in named_tokens(Palette::VIVID) {
            if name == "chip_fg" {
                continue;
            }
            for (surface, background) in [("black", (0, 0, 0)), ("white", (255, 255, 255))] {
                let ratio = contrast(rgb(token), background);
                assert!(
                    ratio >= 3.0,
                    "{name} reaches {ratio:.2}:1 on {surface}, which is below the 3:1 floor"
                );
            }
        }
    }

    /// The chip inverts, so its foreground is read on top of whichever token
    /// the chip carries. It is exempt from the rule above and bound by this
    /// one instead.
    #[test]
    fn a_chip_word_stays_legible_on_every_token_it_can_sit_on() {
        let vivid = Palette::VIVID;
        for (name, token) in named_tokens(vivid) {
            if name == "chip_fg" {
                continue;
            }
            let ratio = contrast(rgb(vivid.chip_fg), rgb(token));
            assert!(
                ratio >= 3.0,
                "a chip word reaches only {ratio:.2}:1 on {name}"
            );
        }
    }

    /// Otherwise the monochrome collapse below would be measuring nothing.
    #[test]
    fn vivid_keeps_its_meanings_apart() {
        let tokens: Vec<Color> = named_tokens(Palette::VIVID)
            .into_iter()
            .filter(|(name, _)| *name != "chip_fg")
            .map(|(_, token)| token)
            .collect();
        for (index, token) in tokens.iter().enumerate() {
            for other in tokens.iter().skip(index + 1) {
                assert_ne!(token, other, "two vivid meanings share a colour");
            }
        }
    }

    /// Under Mono every token is the same colour, so anything that survives
    /// there is carried by a glyph or a word. This is what lets the palette
    /// grow later without the aesthetic layer quietly acquiring meaning.
    #[test]
    fn the_monochrome_palette_leaves_no_token_distinguishable_by_colour() {
        let mono = Palette::resolve(ColorCapability::Mono, ThemeChoice::Native);
        let tokens = [
            mono.accent,
            mono.success,
            mono.attention,
            mono.danger,
            mono.suspended,
            mono.muted,
            mono.structure,
            mono.chip_fg,
        ];
        for token in tokens {
            assert_eq!(
                token, tokens[0],
                "a token that keeps its own hue under Mono can still smuggle meaning in colour"
            );
        }

        // And the coloured palette must genuinely distinguish them, or the
        // monochrome check above would be vacuous.
        let ansi = Palette::resolve(ColorCapability::Ansi16, ThemeChoice::Native);
        let coloured = [
            ansi.accent,
            ansi.success,
            ansi.attention,
            ansi.danger,
            ansi.suspended,
            ansi.muted,
            ansi.structure,
        ];
        for (index, token) in coloured.iter().enumerate() {
            for other in coloured.iter().skip(index + 1) {
                assert_ne!(token, other, "semantic tokens must not share a hue");
            }
        }
    }

    #[test]
    fn spread_pushes_the_secondary_half_to_the_right_edge() {
        let line = spread(vec![Span::raw("left")], vec![Span::raw("right")], 20);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered.chars().count(), 20);
        assert!(rendered.starts_with("left"));
        assert!(rendered.ends_with("right"));
    }

    #[test]
    fn spread_never_overlaps_when_the_halves_do_not_fit() {
        let line = spread(
            vec![Span::raw("a very long left half")],
            vec![Span::raw("and a right half")],
            10,
        );
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(rendered.contains("a very long left half and a right half"));
    }

    #[test]
    fn chips_and_actions_carry_a_word_not_only_a_color() {
        assert!(
            chip("READY TO REVIEW", success())
                .content
                .contains("READY TO REVIEW")
        );
        let spans = action("a", "Apply changes", success());
        let rendered: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(rendered, " a  Apply changes");
    }

    #[test]
    fn centered_rule_stays_short_and_bounded() {
        assert!(centered_rule(200).spans[0].content.chars().count() <= 14);
        assert!(centered_rule(2).spans[0].content.chars().count() >= 4);
    }

    // Regression guard for the "one palette" invariant: a raw Color:: may live only
    // in theme.rs, the canonical source of truth. Every other tui source file must
    // reference colors solely through theme constants/helpers. The one permitted
    // exception is the ratatui Color::Reset assertion in render.rs, which proves a
    // rail was *not* painted rather than choosing a hue. Scanning recursively means
    // a future submodule under src/tui can't hide a leak either.
    #[test]
    fn raw_color_lives_only_in_theme() {
        // Nested helpers must be declared before any statement (items_after_statements).
        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_rs(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }

        // Permitted raw Color:: usages OUTSIDE theme.rs: the single entry is the reset
        // assertion in render.rs. Any occurrence not covered here fails with its location,
        // so an edit can't silently reintroduce a raw hue.
        let allow: &[(&str, &str)] = &[("render.rs", "Color::Reset")];

        let tui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui");
        let mut files: Vec<_> = Vec::new();
        collect_rs(&tui_dir, &mut files);
        files.sort();

        for path in &files {
            let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
            if file_name == "theme.rs" {
                continue; // canonical palette source of truth — raw Color:: belongs here
            }
            let contents = std::fs::read_to_string(path).unwrap_or_default();
            for (i, line) in contents.lines().enumerate() {
                if !line.contains("Color::") {
                    continue;
                }
                let allowed = allow
                    .iter()
                    .any(|(name, sub)| file_name == *name && line.contains(sub));
                assert!(
                    allowed,
                    "raw Color:: leaked outside theme.rs at {file_name}:{line_no}\n      {snippet}",
                    line_no = i + 1,
                    snippet = line.trim(),
                );
            }
        }
    }
}
