//! POD — the Senate Operator Droid.
//!
//! One pixel companion, drawn as a real sprite. Every variant is a 26×14
//! pixel image rendered at double vertical resolution with half-block
//! glyphs: each terminal cell carries two pixels (`▀` with a foreground for
//! the top pixel and a background for the bottom one), which is what lets
//! POD read as a picture instead of a pile of box-drawing characters.
//!
//! Each pipeline job is a scene: the researcher wears reading glasses next
//! to a stack of books, the architect draws with a pencil, the builder wears
//! a hard hat and takes a hammer to a laptop, the quality reviewer holds a
//! magnifying glass, the spec reviewer works through a checklist, the crowned lead
//! weighs the balance scales in Synthesis and sits at the gavel in Decision.
//! The scene follows the *selected stage* in every state, while the state
//! owns the expression (eyes, brow, mouth), the body color and the label.
//! State comes from canonical `RunStatus`/`StageStatus` and the scene from
//! the typed `StageKind`; nothing is inferred from prose.
//!
//! Sprites are painted only with semantic theme tokens — body takes the
//! state color, props take `structure`, materials take `attention`, `paper`
//! and `muted` — so Mono collapses the art to a silhouette whose features
//! survive as punched-through holes, and Vivid/ANSI recolor it without the
//! art knowing a hue. Every variant occupies exactly `MASCOT_WIDTH` ×
//! `MASCOT_HEIGHT` cells so surrounding layout never shifts.
//!
//! POD may move, but only where the surface permits it and only while the
//! domain considers the work active: the caller hands in a [`MotionFrame`],
//! and a still frame draws exactly the resting art. While a stage Runs, POD
//! works: the prop plays its two-frame cycle — the lens sweeps, the pencil
//! draws, the scales tilt, the gavel lifts — and the eyes blink on their own
//! tick of the same loop. The movement reinforces a state written elsewhere
//! in words and glyphs — it is never the evidence for it.

use super::motion::MotionFrame;
use super::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::domain::{RunStatus, StageKind, StageStatus};

/// Every variant renders exactly this many terminal cells per art row:
/// a 15-pixel body column plus an 11-pixel panel where POD's arm holds the
/// job's tool.
pub(crate) const MASCOT_WIDTH: u16 = 26;

/// Seven art rows (14 pixel rows at two pixels per cell) plus one label row.
pub(crate) const MASCOT_HEIGHT: u16 = 8;

/// Pixels of the face rows kept beside a tool; the arm panel starts here.
const BODY_COLUMN: usize = 15;

/// Pixel rows in a sprite; always twice the art rows.
const SPRITE_ROWS: usize = 14;

/// Overall mood, projected from canonical run/stage state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MascotState {
    Idle,
    Running,
    Waiting,
    NeedsUser,
    Completed,
    Failed,
}

/// Current responsibility, projected from the typed stage role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MascotActivity {
    Research,
    Architecture,
    Implementation,
    QualityReview,
    SpecReview,
    Synthesis,
    Decision,
    /// The repository's own checks running in a terminal.
    Verify,
}

/// Maps canonical state to POD's mood. The selected stage wins when present;
/// the run status is the fallback. A pending/ready stage inside a running
/// run reads as Waiting (its turn has not come), otherwise Idle.
pub(crate) const fn mascot_state(
    run: Option<RunStatus>,
    stage: Option<StageStatus>,
) -> MascotState {
    match stage {
        Some(StageStatus::Running) => MascotState::Running,
        Some(StageStatus::NeedsUser) => MascotState::NeedsUser,
        Some(StageStatus::Completed) => MascotState::Completed,
        Some(StageStatus::Failed) => MascotState::Failed,
        Some(StageStatus::Paused | StageStatus::Interrupted) => MascotState::Waiting,
        Some(StageStatus::Pending | StageStatus::Ready) => match run {
            Some(RunStatus::Running) => MascotState::Waiting,
            _ => MascotState::Idle,
        },
        Some(StageStatus::Skipped) | None => match run {
            Some(RunStatus::Running) => MascotState::Running,
            Some(RunStatus::NeedsUser) => MascotState::NeedsUser,
            Some(RunStatus::Failed) => MascotState::Failed,
            Some(RunStatus::Completed | RunStatus::Applied) => MascotState::Completed,
            Some(RunStatus::Paused | RunStatus::Interrupted) => MascotState::Waiting,
            Some(
                RunStatus::Created | RunStatus::Preparing | RunStatus::Ready | RunStatus::Discarded,
            )
            | None => MascotState::Idle,
        },
    }
}

/// Maps the stage's kind to POD's scene. The kind, not the role, names the
/// job: the lead runs both Synthesis and Decision, and each deserves its own
/// scene. Legacy and independent reviews map to `QualityReview`; a fix is
/// implementation work; deep analysis is reading.
pub(crate) const fn mascot_activity(kind: StageKind) -> MascotActivity {
    match kind {
        StageKind::Research | StageKind::DeepAnalysis => MascotActivity::Research,
        StageKind::Architecture => MascotActivity::Architecture,
        StageKind::Implementation
        | StageKind::Simplification
        | StageKind::Fix
        | StageKind::FollowUp => MascotActivity::Implementation,
        StageKind::CodeQualityReview | StageKind::Review | StageKind::IndependentReview => {
            MascotActivity::QualityReview
        }
        StageKind::SpecReview => MascotActivity::SpecReview,
        StageKind::Verify => MascotActivity::Verify,
        StageKind::Synthesis | StageKind::Lead => MascotActivity::Synthesis,
        StageKind::Decision => MascotActivity::Decision,
    }
}

/// One state's face at rest, in pixel slots: the brow above the eyes
/// (2 pixels, `K` = raised), the eye's top and bottom pixel rows (2 each),
/// and the mouth's upper and lower rows (6 each). `G` pixels are body,
/// `K` pixels are punched through to the terminal background.
struct Expression {
    brow: &'static str,
    eye_top: &'static str,
    eye_bottom: &'static str,
    mouth_upper: &'static str,
    mouth_lower: &'static str,
}

const fn resting_expression(state: MascotState) -> Expression {
    match state {
        // Open square eyes, an even mouth.
        MascotState::Idle => Expression {
            brow: "GG",
            eye_top: "KK",
            eye_bottom: "KK",
            mouth_upper: "GKKKKG",
            mouth_lower: "GGGGGG",
        },
        // Eyes lowered into the work, mouth set small.
        MascotState::Running => Expression {
            brow: "GG",
            eye_top: "GG",
            eye_bottom: "KK",
            mouth_upper: "GGKKGG",
            mouth_lower: "GGGGGG",
        },
        // Eyes lowered and the mouth slack: dozing, not working.
        MascotState::Waiting => Expression {
            brow: "GG",
            eye_top: "GG",
            eye_bottom: "KK",
            mouth_upper: "GGGGGG",
            mouth_lower: "GGKKGG",
        },
        // Raised brows, wide eyes, mouth open: POD is asking.
        MascotState::NeedsUser => Expression {
            brow: "KK",
            eye_top: "KK",
            eye_bottom: "KK",
            mouth_upper: "GGKKGG",
            mouth_lower: "GGKKGG",
        },
        // Smiling eyes and a smile.
        MascotState::Completed => Expression {
            brow: "GG",
            eye_top: "KK",
            eye_bottom: "GG",
            mouth_upper: "KGGGGK",
            mouth_lower: "GKKKKG",
        },
        // Open eyes over a frown.
        MascotState::Failed => Expression {
            brow: "GG",
            eye_top: "KK",
            eye_bottom: "KK",
            mouth_upper: "GKKKKG",
            mouth_lower: "KGGGGK",
        },
    }
}

/// The expression POD wears this frame.
///
/// A reaction outranks everything: something just happened and POD's eyes
/// go wide (or, where they already are wide, narrow — a reaction must be
/// visible from every face without inventing a third eye shape). Then the
/// blink, which is the eyes flipping between their two pixel rows and
/// exists only while Running — the one state whose motion means anything.
/// Everywhere else the state's resting face stands.
const fn expression(state: MascotState, blinking: bool, reacting: bool) -> Expression {
    let resting = resting_expression(state);
    if reacting {
        let already_wide =
            resting.eye_top.as_bytes()[0] == b'K' && resting.eye_bottom.as_bytes()[0] == b'K';
        return Expression {
            eye_top: if already_wide { "GG" } else { "KK" },
            eye_bottom: "KK",
            ..resting
        };
    }
    if matches!(state, MascotState::Running) && blinking {
        // The blink flips the eye between its two pixel rows: lowered eyes
        // look up, and the face stays the face.
        return Expression {
            eye_top: resting.eye_bottom,
            eye_bottom: resting.eye_top,
            ..resting
        };
    }
    resting
}

/// State label; for Running the activity label takes over (the state is
/// already obvious from color and the surrounding UI).
const fn state_label(state: MascotState) -> &'static str {
    match state {
        MascotState::Idle => "READY",
        MascotState::Running => "RUNNING",
        MascotState::Waiting => "WAITING",
        MascotState::NeedsUser => "NEEDS YOU",
        MascotState::Completed => "DONE",
        MascotState::Failed => "FAILED",
    }
}

pub(crate) const fn activity_label(activity: MascotActivity) -> &'static str {
    match activity {
        MascotActivity::Research => "READING",
        MascotActivity::Architecture => "DESIGNING",
        MascotActivity::Implementation => "BUILDING",
        MascotActivity::QualityReview => "INSPECTING",
        MascotActivity::SpecReview => "CHECKING",
        MascotActivity::Verify => "VERIFYING",
        MascotActivity::Synthesis => "WEIGHING",
        MascotActivity::Decision => "DECIDING",
    }
}

fn state_style(state: MascotState) -> Style {
    match state {
        MascotState::Idle | MascotState::Running => Style::new().fg(theme::accent()),
        MascotState::Waiting => Style::new().fg(theme::muted_color()),
        MascotState::NeedsUser => Style::new().fg(theme::attention()),
        MascotState::Completed => Style::new().fg(theme::success()),
        MascotState::Failed => Style::new().fg(theme::danger()),
    }
}

/// The label POD wears: state wins outright; only Running yields the line
/// to the current activity.
fn label(state: MascotState, activity: Option<MascotActivity>) -> &'static str {
    match (state, activity) {
        (MascotState::Running, Some(activity)) => activity_label(activity),
        _ => state_label(state),
    }
}

/// What sits on POD's head, five pixel rows over the 17-pixel body column.
/// A bare head for most jobs; the builder wears the hard hat and the lead
/// the crown.
const fn hat_rows(activity: Option<MascotActivity>) -> [&'static str; 5] {
    match activity {
        Some(MascotActivity::Implementation) => [
            ".................",
            ".....YYYYYYYY....",
            "....YYYYYYYYYY...",
            "...YYYYYYYYYYYY..",
            "..WWWWWWWWWWWWWW.",
        ],
        Some(MascotActivity::Synthesis | MascotActivity::Decision) => [
            "...Y....Y....Y...",
            "...YY..YYY..YY...",
            "...YYYYYYYYYYY...",
            "...YYYYYYYYYYY...",
            ".................",
        ],
        _ => [
            ".................",
            ".................",
            ".................",
            ".................",
            ".................",
        ],
    }
}

/// POD's arm and the tool it holds, nine pixel rows of eleven pixels, in
/// two frames: frame 0 is the tool at rest, frame 1 is the tool being
/// worked — the pair the running loop alternates so the job looks *done*,
/// not just named. The first two columns sit against POD's side, so the
/// `G` arm pixels join the body; a tool on the ground ends on the feet row.
///
/// - Research: an open book held out to read, lines of text on both pages;
///   the right page turns over to a fresh one. It used to be a small book
///   held over a stack of books, which at this size read as a tray above a
///   box.
/// - Architecture: a pencil held to its line; the stroke moves on.
/// - Implementation: a hammer raised beside an open laptop; it comes down
///   and the screen cracks.
/// - `QualityReview`: a magnifying glass held by its handle; the lens sweeps.
/// - `SpecReview`: a clipboard checklist held by its edge; the next box gets
///   ticked.
/// - Verify: a terminal with POD's hand on the keyboard; the hand lifts and
///   output scrolls in.
/// - Synthesis: the balance scales gripped by the pole; the pans tilt.
/// - Decision: a gavel lifted over its block; it comes down to strike.
const fn prop_rows(activity: MascotActivity, frame: u8) -> [&'static str; 9] {
    if frame == 1 {
        working_prop_rows(activity)
    } else {
        resting_prop_rows(activity)
    }
}

/// Frame 0: every tool at rest.
const fn resting_prop_rows(activity: MascotActivity) -> [&'static str; 9] {
    match activity {
        MascotActivity::Research => [
            "...........",
            "..WWWW.WWWW",
            "..WDDWYWDDW",
            "GGWWWWYWWWW",
            "GGWDDWYWDDW",
            "..WWWWYWWWW",
            "..YYYYYYYYY",
            "...........",
            "...........",
        ],
        MascotActivity::Architecture => [
            ".....WW....",
            ".....WYY...",
            "GG.GGGYYY..",
            "GGGGGGYYYY.",
            "........YYY",
            ".........YD",
            "..........D",
            "...........",
            "...DDDDDDD.",
        ],
        MascotActivity::Implementation => [
            "..YYY......",
            "..YYY......",
            "...D.......",
            "...D.......",
            "GGGD.BBBBBB",
            "GGGD.BWWWWB",
            ".....BWWWWB",
            ".....BBBBBB",
            "...DDDDDDDD",
        ],
        MascotActivity::QualityReview => [
            ".....BBBBB.",
            "....BWWWWWB",
            "....BWWWWWB",
            "....BWWWWWB",
            "...GBBBBBB.",
            "GGGGGB.....",
            "GG.GG......",
            "...........",
            "...........",
        ],
        MascotActivity::SpecReview => [
            "......BB...",
            "...WWWWWWWW",
            "...WYYWDDDW",
            "GGGGWWWWWWW",
            "GGGGYYWDDDW",
            "...WWWWWWWW",
            "...WDDWDDDW",
            "...WWWWWWWW",
            "...........",
        ],
        MascotActivity::Verify => [
            "...BBBBBBBB",
            "...BDDDDDDB",
            "...BDYDDDDB",
            "...BDDDDDDB",
            "GG.BDDDDDDB",
            "GGGBBBBBBBB",
            "..GG...D...",
            "..GGG.DDD..",
            "..DDDDDDDDD",
        ],
        MascotActivity::Synthesis => [
            "...YYYYYYY.",
            "...Y..Y..Y.",
            "..YYY.Y.YYY",
            "......Y....",
            "......Y....",
            "......Y....",
            "......Y....",
            ".....YYY...",
            "....DDDDD..",
        ],
        MascotActivity::Decision => [
            "...YYYYY...",
            "...YYYYY...",
            ".....D.....",
            ".....D.....",
            ".....D.....",
            "...........",
            "...........",
            "..BBBBBBB..",
            ".DDDDDDDDD.",
        ],
    }
}

/// Frame 1: the same tool mid-use — same footprint, same materials, only
/// the movement differs from the resting frame.
const fn working_prop_rows(activity: MascotActivity) -> [&'static str; 9] {
    match activity {
        MascotActivity::Research => [
            "...........",
            "..WWWW.WWWW",
            "..WDDWYWWWW",
            "GGWWWWYWDDW",
            "GGWDDWYWWWW",
            "..WWWWYWDDW",
            "..YYYYYYYYY",
            "...........",
            "...........",
        ],
        MascotActivity::Architecture => [
            "....WW.....",
            "....WYY....",
            "GGGGGYYY...",
            "GGGGGYYYY..",
            ".......YYY.",
            "........YD.",
            ".........D.",
            "...........",
            "...DDDDD...",
        ],
        MascotActivity::Implementation => [
            "...........",
            "...........",
            "...........",
            ".....YY....",
            "GGGDDYYBBBB",
            "GGGDDYYWWKB",
            ".....BWKWWB",
            ".....BBBBBB",
            "...DDDDDDDD",
        ],
        MascotActivity::QualityReview => [
            "...........",
            ".....BBBBB.",
            "....BWWWWWB",
            "....BWWWWWB",
            "....BWWWWWB",
            "GGGGGBBBBB.",
            "GGGGB......",
            "...........",
            "...........",
        ],
        MascotActivity::SpecReview => [
            "......BB...",
            "...WWWWWWWW",
            "...WYYWDDDW",
            "GGGGWWWWWWW",
            "GGGGYYWDDDW",
            "...WWWWWWWW",
            "...WYYWDDDW",
            "...WWWWWWWW",
            "...........",
        ],
        MascotActivity::Verify => [
            "...BBBBBBBB",
            "...BDYDWWDB",
            "...BDDDDDDB",
            "...BDWWWDDB",
            "GG.BDDDDDDB",
            "GGGBBBBBBBB",
            "..GGG..D...",
            "......DDD..",
            "..DDDDDDDDD",
        ],
        MascotActivity::Synthesis => [
            "...YYYYYYY.",
            "..YYY.Y..Y.",
            "......Y..Y.",
            "......Y.YYY",
            "......Y....",
            "......Y....",
            "......Y....",
            ".....YYY...",
            "....DDDDD..",
        ],
        MascotActivity::Decision => [
            "...........",
            "...........",
            "...........",
            "...........",
            "......YY...",
            "..DDDDYY...",
            "......YY...",
            "..BBBBBBB..",
            ".DDDDDDDDD.",
        ],
    }
}

/// The nine face pixel rows of the 17-pixel body column, with the state
/// expression slotted in. Research wears its reading glasses — frames drawn
/// in `paper` around the same eye pixels, so the eyes (and the blink) stay
/// exactly where every other face keeps them.
fn face_rows(activity: Option<MascotActivity>, expr: &Expression) -> [String; 9] {
    let Expression {
        brow,
        eye_top,
        eye_bottom,
        mouth_upper,
        mouth_lower,
    } = expr;
    let glasses = matches!(activity, Some(MascotActivity::Research));
    let crown_row = format!("...GGG{brow}GGG{brow}GG..");
    let (top, eyes_top, eyes_bottom, under) = if glasses {
        (
            crown_row,
            format!("...GGW{eye_top}WGW{eye_top}WG.."),
            format!("...GGW{eye_bottom}WGW{eye_bottom}WG.."),
            "...GGWWWWWWWWWG..".to_owned(),
        )
    } else {
        (
            crown_row,
            format!("...GGG{eye_top}GGG{eye_top}GG.."),
            format!("...GGG{eye_bottom}GGG{eye_bottom}GG.."),
            "...GGGGGGGGGGGG..".to_owned(),
        )
    };
    [
        top,
        eyes_top,
        eyes_bottom,
        under,
        format!("...GGG{mouth_upper}GGG.."),
        format!("...GGG{mouth_lower}GGG.."),
        "....GGGGGGGGGG...".to_owned(),
        ".....GG....GG....".to_owned(),
        ".....GG....GG....".to_owned(),
    ]
}

/// The full 26×14 pixel grid for one frame. With a stage in hand the body
/// stands left with the prop beside it; without one the plain body stands
/// centered in the same footprint.
fn sprite_grid(
    state: MascotState,
    activity: Option<MascotActivity>,
    motion: MotionFrame,
) -> Vec<String> {
    let expr = expression(state, motion.is_blinking(), motion.is_reacting());
    // The prop is worked, not just worn: its two frames alternate only while
    // the stage is actually Running. Finished work holds the worked frame
    // still — the gavel has come down, the last box is ticked — and every
    // other state rests the tools.
    let prop_frame = match state {
        MascotState::Running => motion.prop_frame(),
        MascotState::Completed => 1,
        _ => 0,
    };
    let hat = hat_rows(activity);
    let face = face_rows(activity, &expr);
    let mut grid = Vec::with_capacity(SPRITE_ROWS);
    match activity {
        Some(activity) => {
            let prop = prop_rows(activity, prop_frame);
            for row in hat {
                grid.push(format!("{row}........."));
            }
            for (face_row, prop_row) in face.iter().zip(prop) {
                grid.push(format!("{}{prop_row}", &face_row[..BODY_COLUMN]));
            }
        }
        None => {
            for row in hat.iter().map(|row| (*row).to_owned()).chain(face) {
                grid.push(format!("....{row}....."));
            }
        }
    }
    grid
}

/// The plunger at rest, POD's hand on the raised handle and the charge still
/// quiet.
const PLUNGER_ARMED: [&str; 9] = [
    "GGGGBBBBB..",
    "GG....B....",
    "......B....",
    "......B....",
    "...DDDDDDD.",
    "...DDDDDDD.",
    "...DDDDDDD.",
    "...........",
    "......YYYYY",
];

/// The plunger driven home: POD's hand pushed the handle down and the charge
/// has caught.
const PLUNGER_FIRED: [&str; 9] = [
    "...........",
    "GG.........",
    "GGGGBBBBB..",
    "......B....",
    "...DDDDDDD.",
    "...DDDDDDD.",
    "...DDDDDDD.",
    "........YWY",
    "......YYYYY",
];

/// POD at the plunger, for the one action that destroys a run.
///
/// The only scene that belongs to no stage: nothing in the pipeline deletes
/// anything, so this one is asked for by name, by the delete confirmation
/// alone. POD works the handle whenever the surface allows motion — the
/// charge is armed, not yet fired, which is exactly the moment the overlay
/// is asking about.
pub(crate) fn demolition_lines(motion: MotionFrame) -> Vec<Line<'static>> {
    let expr = expression(MascotState::Failed, motion.is_blinking(), false);
    let prop = if motion.prop_frame() == 0 {
        PLUNGER_ARMED
    } else {
        PLUNGER_FIRED
    };
    let mut grid: Vec<String> = hat_rows(None)
        .iter()
        .map(|row| format!("{row}........."))
        .collect();
    for (face_row, prop_row) in face_rows(None, &expr).iter().zip(prop) {
        grid.push(format!("{}{prop_row}", &face_row[..BODY_COLUMN]));
    }
    let body = theme::danger();
    let mut lines: Vec<Line<'static>> = grid
        .chunks(2)
        .map(|pair| render_pixel_pair(&pair[0], &pair[1], body))
        .collect();
    lines.push(
        Line::from(Span::styled(
            label_text("DEMOLITION", true),
            Style::new().fg(body).add_modifier(Modifier::BOLD),
        ))
        .alignment(ratatui::layout::Alignment::Center),
    );
    lines
}

/// Pixel column POD's body is centered on when a prop stands beside it.
const BODY_CENTER: usize = 10;

/// The label row, padded to the sprite's width. Beside a prop the text sits
/// under POD's body, not in the gap between POD and the prop; alone, POD
/// already stands centered, so the text does too.
fn label_text(text: &str, beside_prop: bool) -> String {
    let width = MASCOT_WIDTH as usize;
    if !beside_prop {
        return text.to_owned();
    }
    let lead = BODY_CENTER.saturating_sub(text.len().div_ceil(2));
    let lead = lead.min(width.saturating_sub(text.len()));
    format!("{:lead$}{text:<rest$}", "", rest = width - lead)
}

/// The color a pixel token resolves to; `None` is punched through to the
/// terminal background ('.' outside the sprite, 'K' for facial features).
fn pixel_color(token: u8, body: Color) -> Option<Color> {
    match token {
        b'G' => Some(body),
        b'B' => Some(theme::structure()),
        b'Y' => Some(theme::attention()),
        b'W' => Some(theme::paper()),
        b'D' => Some(theme::muted_color()),
        _ => None,
    }
}

/// Folds two pixel rows into one row of half-block cells, merging adjacent
/// cells of equal style into single spans.
fn render_pixel_pair(top_row: &str, bottom_row: &str, body: Color) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::new();
    for (top, bottom) in top_row.bytes().zip(bottom_row.bytes()) {
        let (glyph, style) = match (pixel_color(top, body), pixel_color(bottom, body)) {
            (None, None) => (' ', Style::new()),
            (Some(color), None) => ('▀', Style::new().fg(color)),
            (None, Some(color)) => ('▄', Style::new().fg(color)),
            (Some(upper), Some(lower)) if upper == lower => ('█', Style::new().fg(upper)),
            (Some(upper), Some(lower)) => ('▀', Style::new().fg(upper).bg(lower)),
        };
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(glyph);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans).alignment(ratatui::layout::Alignment::Center)
}

/// Renders POD: the sprite for the selected stage's scene wearing the
/// state's expression and body color, plus one label. Always
/// `MASCOT_HEIGHT` rows; art rows are exactly `MASCOT_WIDTH` cells, in
/// every phase the frame can hand out.
pub(crate) fn mascot_lines(
    state: MascotState,
    activity: Option<MascotActivity>,
    motion: MotionFrame,
) -> Vec<Line<'static>> {
    let grid = sprite_grid(state, activity, motion);
    // Every state style sets a foreground; paper is an inert fallback that
    // keeps the sprite visible if one ever stopped.
    let body = state_style(state).fg.unwrap_or_else(theme::paper);
    let mut lines: Vec<Line<'static>> = grid
        .chunks(2)
        .map(|pair| render_pixel_pair(&pair[0], &pair[1], body))
        .collect();
    lines.push(
        Line::from(Span::styled(
            label_text(label(state, activity), activity.is_some()),
            state_style(state).add_modifier(Modifier::BOLD),
        ))
        .alignment(ratatui::layout::Alignment::Center),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::motion::MotionAllowance;

    const ALL_STATES: [MascotState; 6] = [
        MascotState::Idle,
        MascotState::Running,
        MascotState::Waiting,
        MascotState::NeedsUser,
        MascotState::Completed,
        MascotState::Failed,
    ];
    const ALL_ACTIVITIES: [MascotActivity; 8] = [
        MascotActivity::Research,
        MascotActivity::Architecture,
        MascotActivity::Implementation,
        MascotActivity::QualityReview,
        MascotActivity::SpecReview,
        MascotActivity::Verify,
        MascotActivity::Synthesis,
        MascotActivity::Decision,
    ];

    /// A moving frame on the given tick of the animation loop.
    fn moving(phase: u8) -> MotionFrame {
        MotionFrame::new(MotionAllowance::ActiveStateAndTransitions, phase, false)
    }

    /// The tick the eyes blink on, found rather than hardcoded so these
    /// tests follow the clock in `motion` wherever it puts the blink.
    fn blink_frame() -> MotionFrame {
        (0..8).map(moving).find(|f| f.is_blinking()).unwrap()
    }

    /// A tick that swaps the prop and nothing else.
    fn work_frame() -> MotionFrame {
        (0..8)
            .map(moving)
            .find(|f| f.prop_frame() == 1 && !f.is_blinking())
            .unwrap()
    }

    /// Every kind of frame a surface can hand out: still, mid-blink, prop
    /// mid-swing, and reacting.
    fn all_phases() -> [MotionFrame; 4] {
        [
            MotionFrame::still(),
            blink_frame(),
            work_frame(),
            MotionFrame::new(MotionAllowance::ActiveStateAndTransitions, 0, true),
        ]
    }

    /// The half-block alphabet: the only glyphs a sprite row may render,
    /// all single-cell in the terminals Ratatui targets.
    const HALF_BLOCKS: [char; 4] = [' ', '▀', '▄', '█'];

    fn rows(state: MascotState, activity: Option<MascotActivity>) -> Vec<String> {
        rows_in(state, activity, MotionFrame::still())
    }

    fn rows_in(
        state: MascotState,
        activity: Option<MascotActivity>,
        motion: MotionFrame,
    ) -> Vec<String> {
        mascot_lines(state, activity, motion)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    /// The rendered scene without the label. Glyphs alone under-report the
    /// art (two colors can share a glyph), so scene comparisons also read
    /// the pixel grid.
    fn scene(state: MascotState, activity: Option<MascotActivity>) -> Vec<String> {
        sprite_grid(state, activity, MotionFrame::still())
    }

    #[test]
    fn every_variant_keeps_the_same_footprint_and_safe_glyphs() {
        for state in ALL_STATES {
            for activity in std::iter::once(None).chain(ALL_ACTIVITIES.map(Some)) {
                for motion in all_phases() {
                    let rows = rows_in(state, activity, motion);
                    assert_eq!(rows.len(), MASCOT_HEIGHT as usize);
                    for row in &rows[..7] {
                        assert_eq!(
                            row.chars().count(),
                            MASCOT_WIDTH as usize,
                            "unstable footprint for {state:?}/{activity:?}/{motion:?}: {row:?}"
                        );
                        for glyph in row.chars() {
                            assert!(
                                HALF_BLOCKS.contains(&glyph),
                                "glyph {glyph:?} is outside the half-block set: {row:?}"
                            );
                        }
                    }
                    assert!(rows[7].is_ascii());
                    assert!(rows[7].chars().count() <= MASCOT_WIDTH as usize);
                }
            }
        }
    }

    /// The grid is the ground truth the renderer folds: every pixel row must
    /// hold exactly `MASCOT_WIDTH` pixels of known tokens, in every state,
    /// scene and motion phase.
    #[test]
    fn every_sprite_grid_is_rectangular_with_known_tokens() {
        for state in ALL_STATES {
            for activity in std::iter::once(None).chain(ALL_ACTIVITIES.map(Some)) {
                for motion in all_phases() {
                    let grid = sprite_grid(state, activity, motion);
                    assert_eq!(grid.len(), SPRITE_ROWS);
                    for row in &grid {
                        assert_eq!(
                            row.len(),
                            MASCOT_WIDTH as usize,
                            "ragged pixel row for {state:?}/{activity:?}: {row:?}"
                        );
                        for token in row.bytes() {
                            assert!(
                                matches!(token, b'.' | b'K' | b'G' | b'B' | b'Y' | b'W' | b'D'),
                                "unknown pixel token {:?} in {row:?}",
                                token as char
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn state_mapping_prefers_selected_stage_over_run() {
        assert_eq!(mascot_state(None, None), MascotState::Idle);
        assert_eq!(
            mascot_state(Some(RunStatus::Completed), Some(StageStatus::Running)),
            MascotState::Running
        );
        assert_eq!(
            mascot_state(Some(RunStatus::Running), Some(StageStatus::NeedsUser)),
            MascotState::NeedsUser
        );
        assert_eq!(
            mascot_state(Some(RunStatus::Running), Some(StageStatus::Completed)),
            MascotState::Completed
        );
        assert_eq!(
            mascot_state(Some(RunStatus::Running), Some(StageStatus::Failed)),
            MascotState::Failed
        );
        assert_eq!(
            mascot_state(Some(RunStatus::Running), Some(StageStatus::Pending)),
            MascotState::Waiting
        );
        assert_eq!(
            mascot_state(Some(RunStatus::Completed), Some(StageStatus::Ready)),
            MascotState::Idle
        );
        assert_eq!(
            mascot_state(Some(RunStatus::Paused), None),
            MascotState::Waiting
        );
    }

    #[test]
    fn kind_mapping_is_semantic() {
        assert_eq!(
            mascot_activity(StageKind::Research),
            MascotActivity::Research
        );
        assert_eq!(
            mascot_activity(StageKind::DeepAnalysis),
            MascotActivity::Research
        );
        assert_eq!(
            mascot_activity(StageKind::Architecture),
            MascotActivity::Architecture
        );
        assert_eq!(
            mascot_activity(StageKind::Implementation),
            MascotActivity::Implementation
        );
        assert_eq!(
            mascot_activity(StageKind::Fix),
            MascotActivity::Implementation
        );
        assert_eq!(
            mascot_activity(StageKind::CodeQualityReview),
            MascotActivity::QualityReview
        );
        assert_eq!(
            mascot_activity(StageKind::Review),
            MascotActivity::QualityReview
        );
        assert_eq!(
            mascot_activity(StageKind::IndependentReview),
            MascotActivity::QualityReview
        );
        assert_eq!(
            mascot_activity(StageKind::SpecReview),
            MascotActivity::SpecReview
        );
        assert_eq!(
            mascot_activity(StageKind::Synthesis),
            MascotActivity::Synthesis
        );
        assert_eq!(
            mascot_activity(StageKind::Decision),
            MascotActivity::Decision
        );
    }

    #[test]
    fn state_overrides_activity_in_the_label() {
        let needs = rows(MascotState::NeedsUser, Some(MascotActivity::Implementation));
        assert_eq!(needs[7].trim(), "NEEDS YOU");

        let failed = rows(MascotState::Failed, Some(MascotActivity::Implementation));
        assert_eq!(failed[7].trim(), "FAILED");

        let done = rows(MascotState::Completed, Some(MascotActivity::Decision));
        assert_eq!(done[7].trim(), "DONE");

        let running = rows(MascotState::Running, Some(MascotActivity::Implementation));
        assert_eq!(running[7].trim(), "BUILDING");

        assert_eq!(rows(MascotState::Running, None)[7], "RUNNING");
        assert_eq!(rows(MascotState::Idle, None)[7], "READY");
        assert_eq!(rows(MascotState::Waiting, None)[7], "WAITING");
    }

    /// Beside a prop, the label centers under POD's body so it names POD,
    /// not the gap between POD and the tool.
    #[test]
    fn a_label_beside_a_prop_sits_under_the_body() {
        for activity in ALL_ACTIVITIES {
            for state in ALL_STATES {
                let row = &rows(state, Some(activity))[7];
                assert_eq!(row.chars().count(), MASCOT_WIDTH as usize);
                let text = row.trim();
                let start = row.find(text).unwrap();
                let center = start * 2 + text.len();
                assert!(
                    center.abs_diff(BODY_CENTER * 2) <= 1,
                    "{text:?} is not under POD's body: {row:?}"
                );
            }
        }
    }

    #[test]
    fn activity_labels_are_distinct_and_compact() {
        let labels: Vec<_> = ALL_ACTIVITIES.iter().map(|a| activity_label(*a)).collect();
        for (index, label) in labels.iter().enumerate() {
            assert!(label.len() <= 10);
            for other in labels.iter().skip(index + 1) {
                assert_ne!(label, other);
            }
        }
    }

    /// Seven jobs have to be seven *scenes*: whatever stage the operator
    /// selects, the hat, the glasses or the prop must say which job this is
    /// at a glance — and the prop panel alone must already tell them apart.
    #[test]
    fn each_job_has_a_scene_of_its_own_in_every_state() {
        for (index, activity) in ALL_ACTIVITIES.iter().enumerate() {
            for other in ALL_ACTIVITIES.iter().skip(index + 1) {
                for frame in [0, 1] {
                    assert_ne!(
                        prop_rows(*activity, frame),
                        prop_rows(*other, frame),
                        "{activity:?} and {other:?} share a prop in frame {frame}"
                    );
                }
            }
        }
        for state in ALL_STATES {
            let scenes: Vec<Vec<String>> = ALL_ACTIVITIES
                .iter()
                .map(|activity| scene(state, Some(*activity)))
                .collect();
            for (index, this) in scenes.iter().enumerate() {
                for (other_index, other) in scenes.iter().enumerate().skip(index + 1) {
                    assert_ne!(
                        this, other,
                        "{:?} and {:?} wear the same scene in {state:?}",
                        ALL_ACTIVITIES[index], ALL_ACTIVITIES[other_index]
                    );
                }
            }
        }
    }

    /// The scene is the job, so it follows the selected stage in every
    /// state: a finished Decision stage is a crowned POD with the gavel
    /// saying DONE, never the generic shell. This is the whole point of
    /// dressing POD.
    #[test]
    fn the_scene_follows_the_selected_stage_in_every_state() {
        for state in ALL_STATES {
            for activity in ALL_ACTIVITIES {
                assert_ne!(
                    scene(state, Some(activity)),
                    scene(state, None),
                    "{activity:?} looks generic in {state:?}"
                );
            }
        }
    }

    /// The state still owns the expression inside every costume: the same
    /// job in two different states may never render identically, or DONE
    /// would be indistinguishable from FAILED under the same crown. Pinned
    /// on the pixel grid, so it holds even where two states share a body
    /// color.
    #[test]
    fn no_state_hides_behind_a_costume() {
        for activity in std::iter::once(None).chain(ALL_ACTIVITIES.map(Some)) {
            for (index, state) in ALL_STATES.iter().enumerate() {
                for other in ALL_STATES.iter().skip(index + 1) {
                    assert_ne!(
                        scene(*state, activity),
                        scene(*other, activity),
                        "{state:?} and {other:?} look the same while {activity:?}"
                    );
                }
            }
        }
    }

    /// The guarantee that lets every other surface stay as it was: a frame
    /// that may not move draws exactly what POD wears standing still.
    #[test]
    fn a_still_frame_draws_the_resting_face_for_every_variant() {
        for state in ALL_STATES {
            for activity in std::iter::once(None).chain(ALL_ACTIVITIES.map(Some)) {
                assert_eq!(
                    rows_in(state, activity, MotionFrame::still()),
                    rows_in(
                        state,
                        activity,
                        MotionFrame::new(MotionAllowance::ActiveStateAndTransitions, 0, false)
                    ),
                    "a still frame changed {state:?}/{activity:?}"
                );
            }
        }
    }

    /// The repeating motion says one thing — The Senate considers this work
    /// active — so it may appear only on the state that carries that
    /// meaning. A face that moved while nothing was happening would be
    /// claiming progress that is not there. It is never evidence that the
    /// process behind the run is alive; see the contract in `motion`.
    #[test]
    fn only_running_moves_and_the_blink_stays_inside_the_footprint() {
        for state in ALL_STATES {
            let resting = rows_in(state, None, MotionFrame::still());
            let blinking = rows_in(state, None, blink_frame());
            if matches!(state, MascotState::Running) {
                assert_ne!(resting, blinking, "Running work has to look alive");
            } else {
                assert_eq!(
                    resting, blinking,
                    "{state:?} is not working, so it holds still"
                );
            }
        }
    }

    /// While a stage Runs, POD works its prop: every scene has a second
    /// frame and it differs from the resting one. Any other state holds its
    /// tools still — a prop swinging beside finished or failed work would
    /// claim activity that is not there.
    #[test]
    fn a_running_scene_works_its_prop_and_every_other_state_rests_it() {
        for activity in ALL_ACTIVITIES {
            assert_ne!(
                prop_rows(activity, 0),
                prop_rows(activity, 1),
                "{activity:?}'s prop has no working frame"
            );
            for state in ALL_STATES {
                let resting = scene(state, Some(activity));
                let working = sprite_grid(state, Some(activity), work_frame());
                if matches!(state, MascotState::Running) {
                    assert_ne!(resting, working, "{activity:?}'s prop never moves");
                } else {
                    assert_eq!(
                        resting, working,
                        "{state:?} is not working, yet {activity:?}'s prop moved"
                    );
                }
            }
        }
    }

    /// Done means the job was done: the finished scene shows the tool's
    /// worked frame — the gavel down, the last box ticked — while every
    /// state that has not finished shows it at rest.
    #[test]
    fn finished_work_shows_the_tool_after_the_job_and_nothing_else_does() {
        let panel = |state, activity| -> Vec<String> {
            scene(state, Some(activity))[5..]
                .iter()
                .map(|row| row[BODY_COLUMN..].to_owned())
                .collect()
        };
        for activity in ALL_ACTIVITIES {
            assert_eq!(
                panel(MascotState::Completed, activity),
                prop_rows(activity, 1),
                "{activity:?} is DONE with its tool still at rest"
            );
            for state in [
                MascotState::Idle,
                MascotState::Waiting,
                MascotState::NeedsUser,
                MascotState::Failed,
            ] {
                assert_eq!(panel(state, activity), prop_rows(activity, 0));
            }
        }
    }

    /// A reaction is POD noticing. It has to be visible from every state,
    /// including the ones whose resting eyes are already wide.
    #[test]
    fn a_reaction_is_visible_from_every_state() {
        for state in ALL_STATES {
            let resting = rows_in(state, None, MotionFrame::still());
            let reacting = rows_in(
                state,
                None,
                MotionFrame::new(MotionAllowance::ActiveStateAndTransitions, 0, true),
            );
            assert_ne!(
                resting, reacting,
                "{state:?} showed no sign of having noticed anything"
            );
        }
    }

    /// Something just happened outranks still working: a run that finishes
    /// mid-blink shows that it finished.
    #[test]
    fn a_reaction_outranks_a_blink() {
        let blink_tick = blink_frame();
        let blinking = rows_in(MascotState::Running, None, blink_tick);
        let reacting = rows_in(
            MascotState::Running,
            None,
            MotionFrame::new(
                MotionAllowance::ActiveStateAndTransitions,
                blink_tick.active_phase(),
                true,
            ),
        );
        assert_ne!(blinking, reacting);
        assert_eq!(
            reacting,
            rows_in(
                MascotState::Running,
                None,
                MotionFrame::new(MotionAllowance::ActiveStateAndTransitions, 0, true),
            )
        );
    }

    /// Every scene keeps a face that can blink and react — the glasses wrap
    /// the same eye pixels every other face uses, so no costume can be the
    /// one that cannot move.
    #[test]
    fn every_scene_can_still_blink_and_react() {
        for activity in ALL_ACTIVITIES {
            let resting = rows_in(MascotState::Running, Some(activity), MotionFrame::still());
            for moving in [
                blink_frame(),
                MotionFrame::new(MotionAllowance::ActiveStateAndTransitions, 0, true),
            ] {
                assert_ne!(
                    resting,
                    rows_in(MascotState::Running, Some(activity), moving),
                    "{activity:?} is a scene that cannot move: {moving:?}"
                );
            }
        }
    }
}
