use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};

use chrono::{DateTime, Utc};

use crate::app::{
    BlockedDependencyRef, RunDetails, StageDependencyRef, StageSummary, StartProgress,
};
use crate::domain::{
    AttentionKind, DependencyOutcome, MissionStatus, RunStatus, StageKind, StageStatus,
    WorkPackageStatus,
};

use super::state::{Overlay, PublishOutcome, RetryRouteChoice, Screen, TuiState, UiMessageKind};
use super::{format, markdown, mascot, theme};
use crate::workspace::{PullRequestRef, PullRequestStatus};

const MIN_WIDTH: u16 = 50;
const MIN_HEIGHT: u16 = 10;

/// Below this width the header drops the run's workflow and repository and
/// keeps only product identity and top-level state.
const HEADER_IDENTITY_WIDTH: u16 = 96;

pub(crate) fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new("Terminal too small — resize to continue\n\nq quit/detach")
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title(" THE SENATE ")),
            area,
        );
        return;
    }
    // Footer grows for a notification; key hints are never replaced. A long
    // message — a refused permission naming the command it refused — wraps
    // over several rows instead of being cut off at the terminal's edge.
    let footer_height = state.message.as_ref().map_or(2, |message| {
        2 + u16::try_from(message_rows(&message.text, area.width).len()).unwrap_or(1)
    });
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(footer_height),
        ])
        .split(area);
    render_header(frame, rows[0], state);
    match state.screen {
        Screen::Runs => render_runs(frame, rows[1], state),
        Screen::RunDetail => render_detail(frame, rows[1], state),
        Screen::Artifact => render_artifact(frame, rows[1], state),
        Screen::Logs => render_logs(frame, rows[1], state),
        Screen::Diff => render_diff(frame, rows[1], state),
        Screen::NewRun => render_new_run(frame, rows[1], state),
        Screen::Missions => render_missions(frame, rows[1], state),
        Screen::MissionDetail => render_mission_detail(frame, rows[1], state),
    }
    render_footer(frame, rows[2], state);
    if let Some(overlay) = state.overlay {
        render_overlay(frame, area, state, overlay);
    }
}

/// Product signature on the left, the run's identity and top-level state on
/// the right. Nothing here repeats the hero: the header speaks for the run,
/// the hero speaks for the stage.
fn render_header(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let screen = match state.screen {
        Screen::Runs => "RUNS",
        Screen::RunDetail => "MISSION DECK",
        Screen::Artifact => "ARTIFACT",
        Screen::Logs => "LOGS",
        Screen::Diff => "DIFF",
        Screen::NewRun => "NEW RUN",
        Screen::Missions => "CAMPAIGNS",
        Screen::MissionDetail => "CAMPAIGN",
    };
    let mut left = vec![
        Span::styled(
            "THE SENATE",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ▌ ", theme::muted()),
        Span::styled(screen, Style::default().add_modifier(Modifier::BOLD)),
    ];
    if let Some(busy) = state.busy_label() {
        left.push(Span::styled(
            format!("  {busy}…"),
            Style::default().fg(theme::accent()),
        ));
    }
    // A run approving on the operator's behalf says so wherever that run is
    // open. Standing permission nobody can see is the failure mode worth
    // spending header space to avoid.
    if state.screen == Screen::RunDetail
        && state
            .details
            .as_ref()
            .is_some_and(|details| details.auto_approve)
    {
        left.push(Span::raw("  "));
        left.push(theme::chip("⚡ AUTO-APPROVE", theme::attention()));
    }
    let right = match state.screen {
        Screen::Missions | Screen::MissionDetail => {
            state.mission.as_ref().map_or_else(Vec::new, |mission| {
                vec![
                    Span::styled(format!("{}  ", mission.id), theme::muted()),
                    mission_visual(mission.status).badge(),
                ]
            })
        }
        Screen::NewRun => Vec::new(),
        _ => state
            .details
            .as_ref()
            .map_or_else(Vec::new, |details| header_identity(details, area.width)),
    };
    frame.render_widget(
        Paragraph::new(theme::spread(left, right, area.width)).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(theme::muted()),
        ),
        area,
    );
}

/// Run identity for the header: the run id itself first — the handle every
/// CLI command takes — then workflow and repository, then the canonical run
/// state and its wall-clock elapsed. Narrow terminals keep the state and drop
/// the identity.
fn header_identity(details: &RunDetails, width: u16) -> Vec<Span<'static>> {
    let now: DateTime<Utc> = std::time::SystemTime::now().into();
    let mut spans = Vec::new();
    if width >= HEADER_IDENTITY_WIDTH {
        let mut identity = details.id.to_string();
        identity.push_str(" · ");
        identity.push_str(&enum_text(details.workflow));
        if let Some(path) = details.repository.as_deref() {
            identity.push_str(" · ");
            identity.push_str(&format::repository_name(path));
        }
        identity.push_str(" · ");
        spans.push(Span::styled(identity, theme::muted()));
    }
    spans.push(run_visual(details.status).badge_bold());
    if let Some(span) = format::elapsed(details.started_at, details.finished_at, now) {
        spans.push(Span::styled(
            format!(" · {}", format::format_duration(span)),
            theme::muted(),
        ));
    }
    spans
}

fn render_runs(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    if state.runs.is_empty() {
        let mut lines = Vec::new();
        // Decoration yields to content: the mascot appears only when the
        // empty state has room for it.
        if area.width >= 60 && area.height >= 14 {
            lines.extend(mascot::mascot_lines(
                mascot::MascotState::Idle,
                None,
                state.motion_frame(),
            ));
            lines.push(Line::from(""));
        }
        if state.archived_count > 0 {
            // Not actually empty — everything is archived; say so instead of
            // pretending the operator never ran anything.
            let plural = if state.archived_count == 1 {
                "run"
            } else {
                "runs"
            };
            lines.push(Line::from(Span::styled(
                format!("{} archived {plural}.", state.archived_count),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(theme::action(
                "H",
                "Show archived runs",
                theme::accent(),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "No runs yet.",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(theme::action(
                "n",
                "Start your first run",
                theme::accent(),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .block(Block::default().padding(Padding::new(2, 2, 1, 0))),
            area,
        );
        return;
    }
    let now: DateTime<Utc> = std::time::SystemTime::now().into();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);
    let list_width = columns[0].width.saturating_sub(3);
    let items = state.runs.iter().enumerate().map(|(index, run)| {
        ListItem::new(run_row(
            run,
            index == state.selected_run_index,
            list_width,
            now,
        ))
    });
    // Stateful so the list scrolls to keep the cursor in view: a plain list
    // always draws from the first run, and a cursor past the last visible row
    // moved onto a run nobody could see.
    let mut list_state = ListState::default().with_selected(Some(state.selected_run_index));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme::muted())
                .padding(Padding::new(1, 1, 1, 0)),
        ),
        columns[0],
        &mut list_state,
    );
    if let Some(details) = state.details.as_ref() {
        render_run_overview(frame, columns[1], details, now);
    } else {
        frame.render_widget(
            Paragraph::new("Loading selected run…")
                .style(theme::muted())
                .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
            columns[1],
        );
    }
}

/// One run row: cursor, state glyph, task, then quiet workflow and age. The
/// run's ULID is never the row's identity; technical detail keeps it.
fn run_row(
    run: &crate::app::RunListItem,
    selected: bool,
    width: u16,
    now: DateTime<Utc>,
) -> Line<'static> {
    let age = format::elapsed(Some(run.updated_at), None, now)
        .map(|span| format!("{} ago", format::format_duration(span)))
        .unwrap_or_default();
    // Only ever visible while the list is showing archived runs.
    let archived_mark = if run.archived { "archived  " } else { "" };
    let meta = format!("{archived_mark}{}  {age}", enum_text(run.workflow));
    // Budget: cursor, glyph, the meta column, and a gap wide enough that the
    // ellipsis can never push the row past the rail.
    let task_width = (width as usize).saturating_sub(meta.chars().count() + 7);
    let task = format::truncate_title(&run.task_summary, task_width.max(8));
    let left = vec![
        Span::styled(
            if selected { "▸ " } else { "  " },
            Style::default().fg(theme::accent()),
        ),
        run_visual(run.status).glyph(),
        Span::styled(
            task,
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                theme::text()
            },
        ),
    ];
    theme::spread(left, vec![Span::styled(meta, theme::muted())], width)
}

/// The Runs screen's right column: enough of the selected run to decide
/// whether to open it, in the same visual language as the Mission Deck.
fn render_run_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    details: &RunDetails,
    now: DateTime<Utc>,
) {
    let width = area.width.saturating_sub(3);
    let mut lines = vec![
        Line::from(Span::styled(
            format::truncate_title(
                details
                    .task
                    .as_deref()
                    .unwrap_or("<legacy input unavailable>"),
                width.saturating_sub(1) as usize,
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            details.repository.as_deref().map_or_else(
                || enum_text(details.workflow),
                |path| {
                    format!(
                        "{} · {}",
                        enum_text(details.workflow),
                        format::repository_name(path)
                    )
                },
            ),
            theme::muted(),
        )),
        Line::from(""),
        theme::section("STAGES"),
    ];
    for stage in &details.stages {
        lines.push(pipeline_line(stage, false, width, now));
    }
    // A failed run says why here, before the user has to open it: the
    // blocking stage's reason is the one fact that decides what to do next.
    if details.status == RunStatus::Failed
        && let Some(reason) = details.failure_reason.as_deref()
    {
        lines.extend(failure_reason_lines(reason));
    }
    if details.status == RunStatus::Completed
        && details.workflow != crate::domain::WorkflowKind::Review
    {
        lines.push(Line::from(""));
        lines.push(Line::from(theme::chip("READY TO REVIEW", theme::success())));
        lines.push(Line::from(""));
        lines.push(Line::from(theme::action(
            "Enter",
            "Open the mission deck to review",
            theme::accent(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("run {}", details.id),
        theme::muted(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

// ---------------------------------------------------------------------------
// Missions
// ---------------------------------------------------------------------------

/// The Missions screen: every mission on the left, the selected one's plan
/// on the right. Packages are the unit here; runs are one level down.
fn render_missions(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    if state.missions.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                "No missions yet.",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "senate mission new \"<title>\" --goal \"<goal>\" plans one over a checkout.",
                theme::muted(),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .block(Block::default().padding(Padding::new(2, 2, 1, 0))),
            area,
        );
        return;
    }
    let now: DateTime<Utc> = std::time::SystemTime::now().into();
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);
    let list_width = columns[0].width.saturating_sub(3);
    let items = state.missions.iter().enumerate().map(|(index, mission)| {
        ListItem::new(mission_row(
            mission,
            index == state.selected_mission_index,
            list_width,
            now,
        ))
    });
    let mut list_state = ListState::default().with_selected(Some(state.selected_mission_index));
    frame.render_stateful_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme::muted())
                .padding(Padding::new(1, 1, 1, 0)),
        ),
        columns[0],
        &mut list_state,
    );
    if let Some(mission) = state.mission.as_ref() {
        render_mission_overview(frame, columns[1], mission);
    } else {
        frame.render_widget(
            Paragraph::new("Loading selected mission…")
                .style(theme::muted())
                .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
            columns[1],
        );
    }
}

/// One campaign row: cursor, state glyph, title, then settled/total Orders,
/// active Orders, need-you count and age.
fn mission_row(
    mission: &crate::app::MissionListItem,
    selected: bool,
    width: u16,
    now: DateTime<Utc>,
) -> Line<'static> {
    let age = format::elapsed(Some(mission.updated_at), None, now)
        .map(|span| format!("{} ago", format::format_duration(span)))
        .unwrap_or_default();
    let attention = if mission.attention == 0 {
        String::new()
    } else {
        format!("⚠ {}  ", mission.attention)
    };
    let meta = format!(
        "{attention}{}/{} settled  {} active  {age}",
        mission.integrated, mission.packages, mission.active
    );
    let title_width = (width as usize).saturating_sub(meta.chars().count() + 7);
    let title = format::truncate_title(&mission.title, title_width.max(8));
    let left = vec![
        Span::styled(
            if selected { "▸ " } else { "  " },
            Style::default().fg(theme::accent()),
        ),
        mission_visual(mission.status).glyph(),
        Span::styled(
            title,
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                theme::text()
            },
        ),
    ];
    theme::spread(left, vec![Span::styled(meta, theme::muted())], width)
}

/// The Missions screen's right column: the goal, every package as one line
/// of engineering state, and what needs the operator.
fn render_mission_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    mission: &crate::app::MissionDetails,
) {
    let width = area.width.saturating_sub(3);
    let mut lines = vec![
        Line::from(Span::styled(
            format::truncate_title(&mission.title, width.saturating_sub(1) as usize),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "{} · {}",
                mission_visual(mission.status).label.to_lowercase(),
                format::repository_name(&mission.repository)
            ),
            theme::muted(),
        )),
        Line::from(""),
        Line::from(Span::styled(mission.goal.clone(), theme::text())),
        Line::from(""),
        theme::section("ORDERS"),
    ];
    for package in &mission.packages {
        lines.push(package_line(mission, package, false, width));
    }
    lines.push(Line::from(""));
    lines.extend(attention_lines(mission, false));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("mission {}", mission.id),
        theme::muted(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

/// One package as a line: state glyph, title, then what it is doing in
/// plain words. The id stays for the CLI, quietly on the right.
fn package_line(
    mission: &crate::app::MissionDetails,
    package: &crate::app::WorkPackageSummary,
    selected: bool,
    width: u16,
) -> Line<'static> {
    let visual = package_visual(package.status);
    let phrase = package_phrase(mission, package);
    let id = package.id.to_string();
    let title_width = (width as usize)
        .saturating_sub(phrase.chars().count() + id.chars().count() + 9)
        .max(8);
    let title = format::truncate_title(&package.title, title_width);
    let left = vec![
        Span::styled(
            if selected { "▸ " } else { "  " },
            Style::default().fg(theme::accent()),
        ),
        visual.glyph(),
        Span::styled(
            title,
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                theme::text()
            },
        ),
        Span::styled(format!("  {phrase}"), theme::muted()),
    ];
    theme::spread(left, vec![Span::styled(id, theme::muted())], width)
}

/// What a package is doing, said the way the operator would say it. A
/// planned package names only the dependencies still outstanding.
fn package_phrase(
    mission: &crate::app::MissionDetails,
    package: &crate::app::WorkPackageSummary,
) -> String {
    match package.status {
        WorkPackageStatus::Planned => {
            let outstanding: Vec<String> = package
                .dependencies
                .iter()
                .filter(|id| {
                    mission
                        .package(id)
                        .is_none_or(|dependency| dependency.status != WorkPackageStatus::Integrated)
                })
                .map(ToString::to_string)
                .collect();
            if outstanding.is_empty() {
                "not started".to_owned()
            } else {
                format!("waits on {}", outstanding.join(", "))
            }
        }
        WorkPackageStatus::Ready => "ready to start".to_owned(),
        WorkPackageStatus::Running => "working".to_owned(),
        WorkPackageStatus::Blocked => "needs you".to_owned(),
        WorkPackageStatus::Delivered => "done, bring it in".to_owned(),
        WorkPackageStatus::Integrated => "settled".to_owned(),
        WorkPackageStatus::Failed => "failed".to_owned(),
        WorkPackageStatus::Cancelled => "cancelled".to_owned(),
    }
}

/// The mission's "Needs you" block, or the one line that says nothing does.
/// Each line is a sentence naming the package by title and the key that
/// answers it, so the operator acts from here without descending.
fn attention_lines(mission: &crate::app::MissionDetails, with_keys: bool) -> Vec<Line<'static>> {
    let attention = &mission.attention;
    if attention.is_empty() {
        return vec![Line::from(Span::styled(
            "Nothing needs you.",
            theme::muted(),
        ))];
    }
    let title = |id: &crate::domain::WorkPackageId| {
        mission
            .package(id)
            .map_or_else(|| id.to_string(), |package| package.title.clone())
    };
    let key = |key: &str, label: &str| -> Vec<Span<'static>> {
        if with_keys {
            let mut spans = vec![Span::raw("   ")];
            spans.extend(theme::action(key, label, theme::accent()));
            spans
        } else {
            Vec::new()
        }
    };
    let mut lines = vec![theme::section("NEEDS YOU")];
    for (package_id, reason) in &attention.blocked {
        let mut spans = vec![
            Span::styled("⚠ ", Style::default().fg(theme::attention())),
            Span::styled(
                format!("{} asks: {}", title(package_id), plain_reason(reason)),
                theme::text(),
            ),
        ];
        spans.extend(key("u", "answer"));
        lines.push(Line::from(spans));
    }
    for (package_id, reason) in &attention.failed {
        lines.push(Line::from(vec![
            Span::styled("✗ ", Style::default().fg(theme::danger())),
            Span::styled(
                format!("{} failed: {reason}", title(package_id)),
                theme::text(),
            ),
        ]));
    }
    for package_id in &attention.awaiting_integration {
        let mut spans = vec![
            Span::styled("◆ ", Style::default().fg(theme::success())),
            Span::styled(format!("{} is done", title(package_id)), theme::text()),
        ];
        spans.extend(key("I", "bring it in"));
        lines.push(Line::from(spans));
    }
    lines
}

/// The observe pass writes a blocked reason as `Permission: Claude Code
/// requests permission for: …`; the operator only needs what is asked.
fn plain_reason(reason: &str) -> String {
    let reason = reason
        .strip_prefix("Permission: ")
        .or_else(|| reason.strip_prefix("Question: "))
        .unwrap_or(reason);
    let reason = reason
        .strip_prefix("Claude Code requests permission for: ")
        .or_else(|| reason.strip_prefix("Codex requests permission for: "))
        .unwrap_or(reason);
    // The provider's own note on how the request can be answered belongs
    // to the attention overlay, where those answers are given.
    reason.split(" — ").next().unwrap_or(reason).to_owned()
}

/// One campaign, in two panes: the Consul (its lead's latest word) on the
/// left, the campaign itself — status, Orders current and settled, and
/// what acts on the selected one — on the right.
fn render_mission_detail(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(mission) = state.mission.as_ref() else {
        frame.render_widget(
            Paragraph::new("Loading campaign…")
                .style(theme::muted())
                .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
            area,
        );
        return;
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    render_consul_pane(frame, columns[0], mission, state.lead_answer.as_ref());
    render_campaign_pane(frame, columns[1], mission, state);
}

/// The Consul pane: the mission's lead, its subtitle spoken once, the
/// latest answer verbatim (never a chat log — the mission brief renders
/// fresh every turn), and a quiet prompt affordance. Chatting from the TUI
/// is a later milestone; this pane only shows what the lead last said.
fn render_consul_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    mission: &crate::app::MissionDetails,
    answer: Option<&crate::app::LeadAnswer>,
) {
    let mut lines = vec![
        theme::section("CONSUL"),
        Line::from(Span::styled("Engineering lead", theme::muted())),
        Line::from(""),
    ];
    let working = mission
        .lead
        .as_ref()
        .is_some_and(|lead| !lead.run_status.is_execution_finished());
    match answer {
        Some(answer) => {
            // Markdown headings are structure, not words to read: drop them,
            // and set the paragraph after "Bottom line" in bold as the gist.
            let mut emphasize = false;
            for line in answer.prose().lines() {
                if let Some(heading) = line.strip_prefix('#') {
                    emphasize = heading
                        .trim_start_matches('#')
                        .trim()
                        .eq_ignore_ascii_case("bottom line");
                    continue;
                }
                if line.trim().is_empty() {
                    emphasize = false;
                    if lines.last().is_some_and(|last| last.width() > 0) {
                        lines.push(Line::from(""));
                    }
                    continue;
                }
                let style = if emphasize {
                    theme::text().add_modifier(Modifier::BOLD)
                } else {
                    theme::text()
                };
                lines.push(Line::from(Span::styled(line.to_owned(), style)));
            }
        }
        None if working => {
            lines.push(Line::from(Span::styled("Conferring…", theme::muted())));
        }
        None => {
            lines.push(Line::from(Span::styled(
                format!(
                    "No conversation yet. Ask with `senate mission ask {} \"…\"`.",
                    mission.id
                ),
                theme::muted(),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("> ", theme::muted())));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme::muted())
                .padding(Padding::new(1, 1, 1, 0)),
        ),
        area,
    );
}

/// The Campaign pane: status, Orders settled/active/needing judgment, the
/// Orders in flight (with the selected one's compact summary), the ones
/// that settled, and the actions the campaign and the selected Order can
/// take right now.
fn render_campaign_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    mission: &crate::app::MissionDetails,
    state: &TuiState,
) {
    let width = area.width.saturating_sub(3);
    let settled_count = mission
        .packages
        .iter()
        .filter(|package| package.status == WorkPackageStatus::Integrated)
        .count();
    let active = mission
        .packages
        .iter()
        .filter(|package| package.status.has_active_run())
        .count();
    let mut lines = vec![
        campaign_status_line(mission.status),
        Line::from(Span::styled(
            format!(
                "{settled_count} / {} Orders settled",
                mission.packages.len()
            ),
            theme::text(),
        )),
        Line::from(Span::styled(format!("{active} active"), theme::text())),
        Line::from(Span::styled(
            format!("{} need your judgment", mission.attention.len()),
            theme::text(),
        )),
        Line::from(""),
        theme::section("CURRENT"),
    ];
    let mut any_current = false;
    for (index, package) in mission.packages.iter().enumerate() {
        if matches!(
            package.status,
            WorkPackageStatus::Integrated | WorkPackageStatus::Cancelled
        ) {
            continue;
        }
        any_current = true;
        let selected = index == state.selected_package_index;
        lines.push(package_line(mission, package, selected, width));
        if selected {
            lines.extend(selected_order_summary(package));
        }
    }
    if !any_current {
        lines.push(Line::from(Span::styled(
            "Nothing in flight — senate mission add",
            theme::muted(),
        )));
    }
    let mut any_settled = false;
    for (index, package) in mission.packages.iter().enumerate() {
        if package.status != WorkPackageStatus::Integrated {
            continue;
        }
        if !any_settled {
            lines.push(Line::from(""));
            lines.push(theme::section("SETTLED"));
        }
        any_settled = true;
        let selected = index == state.selected_package_index;
        lines.push(package_line(mission, package, selected, width));
        if selected {
            lines.extend(selected_order_summary(package));
        }
    }
    lines.push(Line::from(""));
    lines.push(campaign_actions_line(state));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

/// The status line atop the Campaign pane: a dot glyph and the plain word
/// for the mission's lifecycle, not the section-heading shout the badge
/// elsewhere uses.
fn campaign_status_line(status: MissionStatus) -> Line<'static> {
    let phrase = match status {
        MissionStatus::Planning => "Planning",
        MissionStatus::Active => "In progress",
        MissionStatus::Completed => "Complete",
        MissionStatus::Cancelled => "Cancelled",
    };
    Line::from(mission_visual(status).phrase(phrase))
}

/// A 2–3 line summary of the selected Order, shown right under its row in
/// CURRENT or SETTLED: its goal, then whatever matters most right now —
/// what it is waiting on or asking, how many acceptance criteria it has,
/// or what it delivered.
fn selected_order_summary(package: &crate::app::WorkPackageSummary) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("  {}", package.goal),
        theme::muted(),
    ))];
    if let Some(reason) = package.reason.as_deref() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("⚠ ", Style::default().fg(theme::attention())),
            Span::styled(reason.to_owned(), theme::muted()),
        ]));
    } else if !package.acceptance_criteria.is_empty() {
        lines.push(Line::from(Span::styled(
            match package.acceptance_criteria.len() {
                1 => "  1 acceptance criterion".to_owned(),
                n => format!("  {n} acceptance criteria"),
            },
            theme::muted(),
        )));
    }
    if let Some(bottom_line) = package
        .result
        .as_ref()
        .and_then(|result| result.bottom_line.as_deref())
    {
        lines.push(Line::from(Span::styled(
            format!("  said: {}", bottom_line.trim()),
            theme::muted(),
        )));
    }
    lines
}

/// The Campaign pane's own action row: `Enter the Senate` first, then
/// whatever the selected Order can take right now. Shares its package
/// conditions with the footer's `mission_detail_actions` so the two never
/// drift apart.
fn campaign_actions_line(state: &TuiState) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push = |key: &str, label: &str, color: Color| {
        if !spans.is_empty() {
            spans.push(Span::raw("   "));
        }
        spans.extend(theme::action(key, label, color));
    };
    // The Order's own actions live in the footer; the pane offers the one
    // thing that belongs to the whole campaign.
    let _ = state;
    push("W", "Enter the Senate", theme::muted_color());
    Line::from(spans)
}

fn mission_visual(status: MissionStatus) -> StatusVisual {
    let (glyph, label, color) = match status {
        MissionStatus::Planning => ("○", "PLANNING", theme::muted_color()),
        MissionStatus::Active => ("●", "ACTIVE", theme::accent()),
        MissionStatus::Completed => ("✓", "COMPLETED", theme::success()),
        MissionStatus::Cancelled => ("×", "CANCELLED", theme::muted_color()),
    };
    StatusVisual {
        glyph,
        label,
        color,
    }
}

fn package_visual(status: WorkPackageStatus) -> StatusVisual {
    let (glyph, label, color) = match status {
        WorkPackageStatus::Planned => ("·", "PLANNED", theme::muted_color()),
        WorkPackageStatus::Ready => ("○", "READY", theme::attention()),
        WorkPackageStatus::Running => ("●", "RUNNING", theme::accent()),
        WorkPackageStatus::Blocked => ("⚠", "BLOCKED", theme::attention()),
        WorkPackageStatus::Delivered => ("◆", "DELIVERED", theme::success()),
        WorkPackageStatus::Integrated => ("✓", "INTEGRATED", theme::success()),
        WorkPackageStatus::Failed => ("✗", "FAILED", theme::danger()),
        WorkPackageStatus::Cancelled => ("×", "CANCELLED", theme::muted_color()),
    };
    StatusVisual {
        glyph,
        label,
        color,
    }
}

/// The last stop before a finished package is brought in: what it is,
/// which run carries the change, and what happens to the checkout.
fn render_integrate_confirmation(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(package) = state.selected_package_summary() else {
        return;
    };
    let run = package
        .current_run
        .map_or_else(|| "no run".to_owned(), |run_id| run_id.to_string());
    let lines = vec![
        Line::from(theme::chip("BRING IT IN", theme::success())),
        Line::from(""),
        Line::from(Span::styled(
            package.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{} · run {run}", package.id),
            theme::muted(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Applies the run's change to the checkout, if it is not applied yet, and records the package as in. Packages that waited on it become ready. A run whose verification failed is refused.",
            theme::text(),
        )),
        Line::from(""),
        Line::from(theme::action("Enter", "Bring it in", theme::success())),
        Line::from(theme::action("Esc", "Cancel", theme::muted_color())),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" Bring it in ", theme::success())),
        area,
    );
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(details) = state.details.as_ref() else {
        frame.render_widget(
            Paragraph::new("Run unavailable")
                .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
            area,
        );
        return;
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);
    // The status strip yields before the pipeline does: a rail too short for
    // both spends every row on the stages themselves.
    let rail = if columns[0].height > STATUS_HEIGHT + 10 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(STATUS_HEIGHT)])
            .split(columns[0]);
        render_status(frame, rows[1], details);
        rows[0]
    } else {
        columns[0]
    };
    render_pipeline(frame, rail, state, details);
    if state.technical {
        render_technical(frame, columns[1], state, details);
    } else {
        render_hero(frame, columns[1], state, details);
    }
}

/// Rows the status strip needs: a rule, its section label, and two sentences.
const STATUS_HEIGHT: u16 = 5;

/// Bottom of the rail: the run's actual state in one or two plain sentences.
/// Typed and state-driven like `activity_message` — never model prose.
fn render_status(frame: &mut Frame<'_>, area: Rect, details: &RunDetails) {
    let now: DateTime<Utc> = std::time::SystemTime::now().into();
    let width = area.width.saturating_sub(3);
    let mut lines = vec![theme::centered_rule(width), theme::section("STATUS")];
    lines.extend(
        status_sentences(details, now, width)
            .into_iter()
            .map(|sentence| Line::from(Span::styled(sentence, theme::text()))),
    );
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme::muted())
                .padding(Padding::new(1, 1, 0, 0)),
        ),
        area,
    );
}

/// A char budget for `format::truncate_title` that leaves room for the
/// ellipsis it appends on top of whatever budget it is given: passing a hard
/// column width straight through would let a truncated line land one column
/// past it, which Ratatui then wraps — pushing whatever follows down a row.
/// Char count, not true display width, matching every other width budget in
/// this file (`span_width`, the footer's navigation-hint fitting): this
/// codebase has no display-width-aware helper, and every line it truncates
/// this way is already assumed to render as one column per `char`.
fn ellipsis_budget(width: usize) -> usize {
    width.saturating_sub(1).max(1)
}

/// The strip's `RunStatus::Failed` sentence for the blocking failed stage.
///
/// Bounds the *composed* sentence to `width`, not just the reason: at the
/// narrowest supported terminal (the 50-column minimum leaves the rail only
/// about 16 columns) even the bare `"<Stage> failed: "` prefix can already
/// exceed the available width — "Implementation failed: " alone is 23
/// characters. Truncating only the reason to whatever budget was left after
/// subtracting the prefix's length still let an oversized prefix push the
/// whole line past `width` once the reason and its ellipsis were appended.
/// When there is not even room for the prefix plus one character of reason,
/// the reason is dropped for the reasonless generic sentence instead of a
/// truncation that would land mid-word or mid-prefix.
fn failed_stage_sentence(
    stage_kind: StageKind,
    failure_reason: Option<&str>,
    width: u16,
) -> String {
    let title = stage_title(stage_kind);
    let Some(reason) = failure_reason else {
        return format!("{title} failed — its logs say why.");
    };
    let prefix = format!("{title} failed: ");
    let composed = format!("{prefix}{reason}");
    let width = width as usize;
    if composed.chars().count() <= width {
        return composed;
    }
    let budget = ellipsis_budget(width);
    if prefix.chars().count() >= budget {
        return format!("{title} failed — its logs say why.");
    }
    format::truncate_title(&composed, budget)
}

/// What the run is doing right now, then how far along it is. The first
/// sentence follows the run's canonical status; the second counts stages, so
/// both stay facts the domain already asserted. `width` bounds the failure
/// reason: the strip is a fixed [`STATUS_HEIGHT`] rows, so a long reason must
/// be cut to fit its line rather than wrap and push the stage-count sentence
/// out of the box.
fn status_sentences(details: &RunDetails, now: DateTime<Utc>, width: u16) -> Vec<String> {
    let running = details
        .stages
        .iter()
        .find(|stage| stage.status == StageStatus::Running);
    let first = match details.status {
        RunStatus::Running => running.map_or_else(
            || "Run is moving between stages.".to_owned(),
            |stage| {
                format::elapsed(stage.started_at, None, now).map_or_else(
                    || format!("{} is running.", stage_title(stage.kind)),
                    |span| {
                        format!(
                            "{} has been running for {}.",
                            stage_title(stage.kind),
                            format::format_duration(span)
                        )
                    },
                )
            },
        ),
        RunStatus::NeedsUser => "Waiting on you to resolve a request.".to_owned(),
        RunStatus::Completed => completed_sentence(details),
        RunStatus::Applied => "Changes applied to the repository.".to_owned(),
        RunStatus::Discarded => "Run discarded.".to_owned(),
        // The blocking stage, never merely the first failed one in workflow
        // order: an optional dependency can fail without stopping the run,
        // so a stage that failed on the way to the real, completion-blocking
        // failure elsewhere must not be mistaken for the cause.
        RunStatus::Failed => details
            .stages
            .iter()
            .find(|stage| stage.status == StageStatus::Failed && stage.blocking)
            .map_or_else(
                || "Run failed.".to_owned(),
                |stage| failed_stage_sentence(stage.kind, stage.failure_reason.as_deref(), width),
            ),
        RunStatus::Paused | RunStatus::Interrupted => {
            "Run suspended — resume when ready.".to_owned()
        }
        RunStatus::Created | RunStatus::Preparing | RunStatus::Ready => {
            "Run has not started yet.".to_owned()
        }
    };
    let completed = details
        .stages
        .iter()
        .filter(|stage| stage.status == StageStatus::Completed)
        .count();
    let failed = failed_stage_titles(details).len();
    vec![
        first,
        if failed == 0 {
            format!("{completed} of {} stages complete.", details.stages.len())
        } else {
            format!(
                "{completed} of {} stages complete, {failed} failed.",
                details.stages.len()
            )
        },
    ]
}

/// The titles of every failed stage, in workflow order.
fn failed_stage_titles(details: &RunDetails) -> Vec<&'static str> {
    details
        .stages
        .iter()
        .filter(|stage| stage.status == StageStatus::Failed)
        .map(|stage| stage_title(stage.kind))
        .collect()
}

/// A run can complete with an optional stage failed on the way — the
/// workflow allows it, and the decision may well have ruled over the gap.
/// Saying only "run complete" would hide that, so the sentence names the
/// failure and the fact that the decision still concluded. What the decision
/// *ruled* stays out of it: the verdict is prose the result panel quotes
/// from the decision's own artifact, never something this strip restates.
fn completed_sentence(details: &RunDetails) -> String {
    let failed = failed_stage_titles(details);
    if failed.is_empty() {
        return "Run complete — the result is ready to review.".to_owned();
    }
    let failed = failed.join(" and ");
    let decided = details
        .stages
        .iter()
        .any(|stage| stage.kind == StageKind::Decision && stage.status == StageStatus::Completed);
    if decided {
        format!(
            "Run complete — {failed} failed, but the decision was still reached. Its verdict is ready to review."
        )
    } else {
        format!("Run complete — the result is ready to review, though {failed} failed on the way.")
    }
}

/// Left rail: the run's task and its stages in workflow order with their
/// semantic durations. One vertical rule separates the rail from the hero —
/// no boxes.
fn render_pipeline(frame: &mut Frame<'_>, area: Rect, state: &TuiState, details: &RunDetails) {
    let now: DateTime<Utc> = std::time::SystemTime::now().into();
    let width = area.width.saturating_sub(3);
    let task = details.task.as_deref().unwrap_or("<legacy input>");
    let title_width = width.saturating_sub(1) as usize;
    // The rail shows one line of the task so the pipeline stays visible; the
    // whole thing is one key away whenever that line dropped something.
    let overflows = format::title_overflows(task, title_width);
    let mut lines: Vec<Line<'static>> = if state.task_expanded && overflows {
        format::wrap_title(task, title_width)
            .into_iter()
            .map(|line| {
                Line::from(Span::styled(
                    line,
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            })
            .collect()
    } else {
        vec![Line::from(Span::styled(
            format::truncate_title(task, title_width),
            Style::default().add_modifier(Modifier::BOLD),
        ))]
    };
    if overflows {
        let label = if state.task_expanded {
            "Collapse task"
        } else {
            "Full task"
        };
        lines.push(Line::from(theme::action("e", label, theme::muted_color())));
    }
    lines.push(Line::from(""));
    lines.push(theme::section("PIPELINE"));
    // Connector segments cost one row per gap; they are the first thing to
    // go when the rail is short.
    let stage_rows = details.stages.len() * 2 - details.stages.len().min(1);
    let connectors = area.height as usize > stage_rows + 6;
    for (index, stage) in details.stages.iter().enumerate() {
        if connectors && index > 0 {
            lines.push(Line::from(Span::styled("  │", theme::muted())));
        }
        lines.push(pipeline_line(
            stage,
            index == state.selected_stage_index,
            width,
            now,
        ));
    }
    // A booked fix is work the operator has already decided on, so the rail
    // shows it where that work will appear. Muted and unselectable: it is an
    // intention about this run, not yet a stage of it.
    if state.fix_is_booked() {
        if connectors {
            lines.push(Line::from(Span::styled("  ┆", theme::muted())));
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("◇ ", theme::muted()),
            Span::styled("Fix (booked)", theme::muted()),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(theme::muted())
                .padding(Padding::new(1, 1, 1, 0)),
        ),
        area,
    );
}

/// One rail row. The cursor marks what is *selected*; the glyph and its color
/// carry what the stage is actually *doing* — the two are independent, since
/// the user can read a finished stage while another one runs.
fn pipeline_line(
    stage: &StageSummary,
    selected: bool,
    width: u16,
    now: DateTime<Utc>,
) -> Line<'static> {
    let name = stage_title(stage.kind);
    let mut duration = format::elapsed(stage.started_at, stage.finished_at, now)
        .map(format::format_duration)
        .unwrap_or_default();
    // Narrow rails give up the duration column rather than clipping either it
    // or the stage name.
    if 5 + name.chars().count() + duration.chars().count() > width as usize {
        duration.clear();
    }
    let left = vec![
        Span::styled(
            if selected { "▸ " } else { "  " },
            Style::default().fg(theme::accent()),
        ),
        stage_visual(stage.status).glyph(),
        Span::styled(name, stage_name_style(stage.status, selected)),
    ];
    theme::spread(left, vec![Span::styled(duration, theme::muted())], width)
}

/// Completed work stays calm and pending work recedes, so the live stage is
/// the only row with full contrast.
fn stage_name_style(status: StageStatus, selected: bool) -> Style {
    let base = match status {
        StageStatus::Running => Style::default().add_modifier(Modifier::BOLD),
        StageStatus::NeedsUser => Style::default()
            .fg(theme::attention())
            .add_modifier(Modifier::BOLD),
        StageStatus::Failed => Style::default().fg(theme::danger()),
        StageStatus::Pending | StageStatus::Ready | StageStatus::Skipped => theme::muted(),
        StageStatus::Completed | StageStatus::Paused | StageStatus::Interrupted => theme::text(),
    };
    if selected {
        base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        base
    }
}

/// Right panel, operational view: what is happening, for how long, on which
/// runtime, what needs the user, what came out, and what can be done next.
fn render_hero(frame: &mut Frame<'_>, area: Rect, state: &TuiState, details: &RunDetails) {
    let now: DateTime<Utc> = std::time::SystemTime::now().into();
    let width = area.width.saturating_sub(3);
    let Some(selected) = details.stages.get(state.selected_stage_index) else {
        frame.render_widget(
            Paragraph::new("No stage selected")
                .style(theme::muted())
                .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
            area,
        );
        return;
    };
    let applyable = state.run_is_applyable();
    let cycle = cycle_label(state);
    let publish = publish_action(details);
    let mut lines = if applyable {
        completed_hero(details, width, now)
    } else {
        stage_hero(selected, width, now)
    };
    // Attention outranks every remaining section, runtime included.
    if let Some(attention) = details.attention.first() {
        lines.push(Line::from(""));
        lines.push(Line::from(theme::chip(
            "⚠ ACTION REQUIRED",
            theme::attention(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            attention.summary.clone(),
            theme::text(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(theme::action(
            "u",
            "Review and resolve",
            theme::attention(),
        )));
    }
    if applyable {
        lines.push(Line::from(""));
        lines.push(Line::from(theme::chip("READY TO REVIEW", theme::success())));
        lines.push(Line::from(""));
        lines.extend(hero_actions(applyable, cycle, &publish, selected.status));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        runtime_summary(selected),
        theme::muted(),
    )));
    if details.attention.is_empty() && !applyable {
        if let Some(reason) = failed_stage_reason(selected) {
            lines.extend(failure_reason_lines(reason));
        } else if let Some(activity) = activity_message(selected) {
            lines.push(Line::from(Span::styled(activity, theme::text())));
        }
    }
    lines.push(Line::from(""));
    // After the pivot the panel speaks for the run, so the result section
    // says which stage's artifact it is offering.
    lines.push(if applyable {
        theme::section(&format!(
            "RESULT · {}",
            stage_title(selected.kind).to_uppercase()
        ))
    } else {
        theme::section("RESULT")
    });
    lines.extend(result_lines(state, selected, width));
    lines.push(Line::from(""));
    lines.push(theme::section("RESOURCES"));
    lines.extend(
        resource_lines(details)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, theme::text()))),
    );
    if !applyable {
        let actions = hero_actions(applyable, cycle, &publish, selected.status);
        if actions
            .iter()
            .all(|line| span_width(&line.spans) <= width as usize)
        {
            lines.push(Line::from(""));
            lines.extend(actions);
        }
    }
    seat_mascot(&mut lines, area, width, state, details, selected);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

/// POD's seat: one blank row, a short rule, another blank row, then POD —
/// under the panel that narrates the stage POD is acting out. Decoration
/// yields first: any doubt about the room, and the seat stays empty. Rows
/// are counted post-wrap, since the hero wraps and a quoted headline can
/// spend more rows than it has lines.
fn seat_mascot(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    width: u16,
    state: &TuiState,
    details: &RunDetails,
    selected: &StageSummary,
) {
    let caption = pull_request_caption(state, details, width);
    let seat = mascot::MASCOT_HEIGHT as usize + 3 + usize::from(caption.is_some());
    let wrapped_rows: usize = lines
        .iter()
        .map(|line| {
            span_width(&line.spans)
                .div_ceil((width as usize).max(1))
                .max(1)
        })
        .sum();
    let inner_height = area.height.saturating_sub(1) as usize;
    if area.width >= mascot::MASCOT_WIDTH + 20 && inner_height > wrapped_rows + seat {
        lines.push(Line::from(""));
        lines.push(theme::centered_rule(width));
        lines.push(Line::from(""));
        lines.extend(mascot::mascot_lines(
            mascot::mascot_state(Some(details.status), Some(selected.status)),
            Some(mascot::mascot_activity(selected.kind)),
            state.motion_frame(),
        ));
        if let Some(caption) = caption {
            lines.push(caption);
        }
    }
}

/// The pull request POD is working on, under POD's label: its title once
/// `gh` has answered, and the repository and number until then — or for
/// good, when it cannot answer. One row, cut to the panel's width.
fn pull_request_caption(
    state: &TuiState,
    details: &RunDetails,
    width: u16,
) -> Option<Line<'static>> {
    let pull_request = details
        .task
        .as_deref()
        .and_then(crate::workspace::PullRequestRef::parse)?;
    let number = format!("#{}", pull_request.number);
    let text = match state
        .pull_request_titles
        .get(&pull_request.url())
        .and_then(Option::as_deref)
    {
        Some(title) => format!("{number} {}", format::viewer_line(title)),
        None => format!("{} {number}", pull_request.repository),
    };
    Some(
        Line::from(Span::styled(
            format::truncate_title(&text, (width as usize).saturating_sub(1).max(1)),
            theme::muted(),
        ))
        .alignment(Alignment::Center),
    )
}

/// The hero's opening statement: which stage, in what state, for how long.
fn stage_hero(stage: &StageSummary, width: u16, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let clock = format::elapsed(stage.started_at, stage.finished_at, now)
        .map(hero_clock)
        .unwrap_or_default();
    vec![
        theme::spread(
            vec![Span::styled(
                stage_title(stage.kind).to_uppercase(),
                Style::default().add_modifier(Modifier::BOLD),
            )],
            vec![Span::styled(
                clock,
                Style::default().add_modifier(Modifier::BOLD),
            )],
            width,
        ),
        Line::from(stage_visual(stage.status).badge_bold()),
    ]
}

/// Once the run is applyable the panel stops monitoring and starts offering
/// review.
fn completed_hero(details: &RunDetails, width: u16, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let clock = format::elapsed(details.started_at, details.finished_at, now)
        .map(hero_clock)
        .unwrap_or_default();
    let completed = details
        .stages
        .iter()
        .filter(|stage| stage.status == StageStatus::Completed)
        .count();
    vec![
        theme::spread(
            vec![Span::styled(
                "RUN COMPLETE",
                Style::default().add_modifier(Modifier::BOLD),
            )],
            vec![Span::styled(
                clock,
                Style::default().add_modifier(Modifier::BOLD),
            )],
            width,
        ),
        Line::from({
            let mut spans = vec![Span::styled(
                format!("✓ {completed} of {} stages completed", details.stages.len()),
                Style::default()
                    .fg(theme::success())
                    .add_modifier(Modifier::BOLD),
            )];
            let failed = failed_stage_titles(details);
            if !failed.is_empty() {
                spans.push(Span::styled(
                    format!(" · {} failed", failed.join(" and ")),
                    Style::default()
                        .fg(theme::danger())
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans
        }),
    ]
}

/// The prominent figure. A span the clock cannot express honestly reads as a
/// span, never as a fabricated `00:00`.
fn hero_clock(span: chrono::TimeDelta) -> String {
    if span.num_seconds() < 1 {
        format::format_duration(span)
    } else {
        format::format_clock(span)
    }
}

/// Typed, state-driven activity text. Never inferred from logs or model prose,
/// and never repeating an action the actions row already offers. A completed
/// stage says nothing: its badge already reads COMPLETED, and the result
/// section below speaks for what it produced.
fn activity_message(stage: &StageSummary) -> Option<String> {
    match stage.status {
        StageStatus::Running => Some("Agent is working…".to_owned()),
        StageStatus::Pending | StageStatus::Ready => Some(waiting_message(stage)),
        StageStatus::Completed => None,
        StageStatus::Failed => Some("The provider ended this stage before it completed".to_owned()),
        StageStatus::Paused | StageStatus::Interrupted => Some("Stage suspended".to_owned()),
        StageStatus::NeedsUser => Some("Waiting on you".to_owned()),
        StageStatus::Skipped => Some("Stage skipped by the workflow".to_owned()),
    }
}

/// Specific reason a Pending/Ready stage isn't running, one line. A required
/// dependency having failed or been skipped outranks the others: this stage
/// is about to be skipped in turn, which matters more than what it was
/// otherwise waiting on. Degraded (optional deps failed/skipped) is
/// informational and only appended once there is something to wait on.
fn waiting_message(stage: &StageSummary) -> String {
    use std::fmt::Write as _;

    const FALLBACK: &str = "Waiting for the previous stage";
    let Some(waiting) = &stage.waiting else {
        return FALLBACK.to_owned();
    };
    if !waiting.blocked_by.is_empty() {
        return blocked_message(&waiting.blocked_by);
    }
    if waiting.waiting_on.is_empty() {
        return FALLBACK.to_owned();
    }
    let mut message = format!("Waiting on: {}", dependency_names(&waiting.waiting_on));
    if !waiting.degraded.is_empty() {
        let _ = write!(
            message,
            " (degraded: {})",
            dependency_names(&waiting.degraded)
        );
    }
    message
}

/// Human titles for a bucket of dependency stages, comma-joined.
fn dependency_names(dependencies: &[StageDependencyRef]) -> String {
    dependencies
        .iter()
        .map(|dependency| stage_title(dependency.kind))
        .collect::<Vec<_>>()
        .join(", ")
}

/// "Blocked: X failed, Y was skipped" — each blocked dependency states its
/// own outcome rather than assuming every one of them failed, since a
/// required dependency can also land here by being skipped in turn.
fn blocked_message(blocked: &[BlockedDependencyRef]) -> String {
    let parts = blocked
        .iter()
        .map(|dependency| {
            format!(
                "{} {}",
                stage_title(dependency.kind),
                outcome_phrase(dependency.outcome)
            )
        })
        .collect::<Vec<_>>();
    format!("Blocked: {}", parts.join(", "))
}

const fn outcome_phrase(outcome: DependencyOutcome) -> &'static str {
    match outcome {
        DependencyOutcome::Failed => "failed",
        DependencyOutcome::Skipped => "was skipped",
    }
}

/// A failed stage's own reason, when it has one. The reason outranks the
/// generic [`activity_message`] wherever it is shown: it is the one thing
/// that says *why*, not just *that*. Every other status, Pending/Ready
/// included, falls through to `activity_message`, which derives its own
/// waiting/blocked-on text from `stage.waiting`.
fn failed_stage_reason(stage: &StageSummary) -> Option<&str> {
    (stage.status == StageStatus::Failed)
        .then_some(stage.failure_reason.as_deref())
        .flatten()
}

/// The failure block shared by the hero and the Runs-screen overview: a
/// section header, then the whole reason. Deliberately *not* cut to one
/// line the way the status strip cuts it — both panels are wrapped
/// paragraphs with room to spare, and the reason was already capped at
/// [`crate::app::query`]'s 200-character limit and collapsed to one line
/// of sanitized text, so at worst it spends two or three rows. Reading
/// the message is the whole point of looking at a failed run; sending the
/// user to the raw logs for it was the complaint this fixes.
fn failure_reason_lines(reason: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        theme::section("WHY IT FAILED"),
        Line::from(Span::styled(reason.to_owned(), theme::text())),
    ]
}

/// Operational runtime line: which agent is doing the work, at what effort.
/// Configured and actual targets are only both shown when they disagree.
fn runtime_summary(stage: &StageSummary) -> String {
    let configured = format!(
        "{} · {}{}",
        stage.configured_provider,
        stage
            .configured_model
            .as_deref()
            .unwrap_or("native default"),
        if stage.route_overridden {
            " (override)"
        } else {
            ""
        }
    );
    let effort = stage.requested_effort.label();
    // Drift is only meaningful when the runtime departed from an explicit
    // configuration. Confirming a concrete model where the route asked for
    // the native default is normal operation, not a mismatch.
    let provider_drift = stage
        .actual_provider
        .as_deref()
        .is_some_and(|actual| actual != stage.configured_provider);
    let model_drift = matches!(
        (stage.configured_model.as_deref(), stage.actual_model.as_deref()),
        (Some(configured), Some(actual)) if configured != actual
    );
    if provider_drift || model_drift {
        return format!(
            "{configured} configured → {} · {} actual · {effort} effort",
            stage.actual_provider.as_deref().unwrap_or("unknown"),
            stage.actual_model.as_deref().unwrap_or("unconfirmed")
        );
    }
    // Once the runtime confirms a concrete model, that is what the operator
    // wants to read instead of "native default".
    match stage.actual_model.as_deref() {
        Some(model) if stage.configured_model.is_none() => {
            format!("{} · {model} · {effort} effort", stage.configured_provider)
        }
        _ => format!("{configured} · {effort} effort"),
    }
}

/// How much of the artifact's opening line the panel quotes: two wrapped
/// rows. Past that the line stops being a glance and starts being reading,
/// which is what opening the artifact is for.
const HEADLINE_ROWS: usize = 2;

fn result_lines(state: &TuiState, selected: &StageSummary, width: u16) -> Vec<Line<'static>> {
    if state.stages_with_artifacts.contains(&selected.id) {
        let mut lines = vec![Line::from(Span::styled(
            format!("✓ {}", result_statement(selected.kind)),
            Style::default()
                .fg(theme::success())
                .add_modifier(Modifier::BOLD),
        ))];
        lines.extend(headline_lines(state, selected, width));
        lines.push(Line::from(theme::action(
            "Enter/o",
            "Open result",
            theme::success(),
        )));
        return lines;
    }
    if let Some(reason) = &state.artifacts_unavailable {
        return vec![Line::from(Span::styled(
            format!("Results unavailable — {reason}"),
            Style::default().fg(theme::danger()),
        ))];
    }
    // Expected absence stays informational; only a completed stage without an
    // artifact is a real problem, and opening it reports that as an error.
    let text = match selected.status {
        StageStatus::Running => "Not available yet — stage is still running",
        StageStatus::Pending | StageStatus::Ready => "Not available yet — stage has not started",
        StageStatus::Failed => "No completed result",
        _ => "No verified artifact",
    };
    vec![Line::from(Span::styled(text, theme::muted()))]
}

/// The result line in the stage's own terms: what kind of outcome arrived,
/// in a few words. It names the shape of the result — a verdict, a plan, a
/// change — and leaves the judgment itself to the quoted bottom line below,
/// because the panel never states a verdict the artifact did not state.
const fn result_statement(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Research => "Research findings ready",
        StageKind::Architecture => "Architecture plan ready",
        StageKind::Implementation => "Implementation ready",
        StageKind::Simplification => "Simplification ready",
        StageKind::CodeQualityReview => "Quality review verdict in",
        StageKind::SpecReview => "Spec review verdict in",
        StageKind::Review | StageKind::IndependentReview => "Review verdict in",
        StageKind::DeepAnalysis => "Analysis ready",
        StageKind::Synthesis => "Synthesis ready",
        StageKind::Decision => "Decision reached",
        StageKind::Fix => "Fix ready",
        StageKind::FollowUp => "Follow-up ready",
        StageKind::Lead => "Lead answered",
        StageKind::Verify => "Verification result in",
    }
}

/// The artifact's opening line, quoted. Never a summary this panel wrote:
/// the words are the agent's, either from the `## Bottom line` the stage
/// contract asks for, or — for an artifact that predates the contract or
/// ignored it — from its first paragraph, which reads as the excerpt it is.
fn headline_lines(state: &TuiState, selected: &StageSummary, width: u16) -> Vec<Line<'static>> {
    let Some(headline) = state
        .headline
        .as_ref()
        .filter(|headline| headline.stage_id == selected.id)
    else {
        return Vec::new();
    };
    let Some(quoted) = headline.text.as_ref() else {
        return Vec::new();
    };
    let budget = (width as usize).max(24) * HEADLINE_ROWS;
    let style = if headline.contracted {
        theme::text()
    } else {
        theme::muted().add_modifier(Modifier::ITALIC)
    };
    vec![Line::from(Span::styled(
        format::truncate_title(quoted, budget),
        style,
    ))]
}

/// Provider-native units in human words, one line per runtime that reported
/// any. Never summed across runtimes and never presented as cost.
///
/// A run routes different roles to different runtimes, and those runtimes do
/// not report the same quantity under the name "input": one line each is what
/// the numbers actually support.
fn resource_lines(details: &RunDetails) -> Vec<String> {
    if details.usage.is_empty() {
        return vec!["No usage reported yet".to_owned()];
    }
    details
        .usage
        .providers()
        .map(|entry| provider_resources(&entry))
        .collect()
}

/// The selected stage's own usage, attributed to the runtime that actually
/// reported it. Falls back to the configured runtime only while nothing has
/// started, when there is no reported usage to misattribute anyway.
fn stage_usage(evidence: &crate::app::StageExecutionEvidence) -> crate::app::ProviderUsage {
    let provider = evidence
        .actual_provider
        .clone()
        .unwrap_or_else(|| evidence.configured_provider.clone());
    crate::app::ProviderUsage {
        accounting: crate::providers::input_accounting(&provider),
        provider,
        usage: evidence.usage,
    }
}

/// One runtime's reported units, written so no token is counted twice.
///
/// A runtime whose input total already contains its cache reads says so in
/// place, and its cache read is not repeated as if it were a further
/// quantity. A runtime that keeps them disjoint lists both.
fn provider_resources(entry: &crate::app::ProviderUsage) -> String {
    use std::fmt::Write as _;
    let mut line = format!(
        "{} · {} input",
        entry.provider,
        format::format_units(entry.usage.input_units)
    );
    let folded_cache_read = entry
        .input_contains_cache_reads()
        .then_some(entry.usage.cache_read_units)
        .flatten();
    if let Some(cached) = folded_cache_read {
        let _ = write!(line, " ({} of it cached)", format::format_units(cached));
    }
    let _ = write!(
        line,
        " · {} output",
        format::format_units(entry.usage.output_units)
    );
    for (label, value) in [
        (
            "cache read",
            if folded_cache_read.is_some() {
                None
            } else {
                entry.usage.cache_read_units
            },
        ),
        ("cache write", entry.usage.cache_write_units),
        ("reasoning output", entry.usage.reasoning_output_units),
    ] {
        if let Some(value) = value {
            let _ = write!(line, " · {} {label}", format::format_units(value));
        }
    }
    line
}

/// Actions offered by the panel, gated on canonical state: apply and discard
/// appear only for a run the workspace layer would accept.
/// What the interface may offer about a fix, resolved once so the panel and
/// the footer cannot disagree about it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FixOffer {
    /// No verdict to answer, or no way to answer one yet.
    Unavailable,
    /// The run has reached its verdict; the key starts the fix now.
    Now,
    /// The run is still working; the key books the fix against its verdict.
    Book,
    /// Already booked. The key cancels it.
    Booked,
}

impl FixOffer {
    fn of(state: &TuiState) -> Self {
        if state.run_can_be_fixed() {
            Self::Now
        } else if !state.run_can_book_a_fix() {
            Self::Unavailable
        } else if state.fix_is_booked() {
            Self::Booked
        } else {
            Self::Book
        }
    }

    const fn label(self) -> Option<&'static str> {
        match self {
            Self::Unavailable => None,
            Self::Now => Some("Fix it"),
            Self::Book => Some("Fix when done"),
            Self::Booked => Some("Fix booked — cancel"),
        }
    }
}

/// The one key that starts another cycle on a run's verdict. A finished run
/// may be fixed, continued or sent after its Follow-ups, and `[f]` opens the
/// choice between them; a run still working can only book a fix.
fn cycle_label(state: &TuiState) -> Option<&'static str> {
    if state.run_can_be_fixed() || state.run_can_be_continued() {
        Some("Next cycle…")
    } else {
        FixOffer::of(state).label()
    }
}

/// What `[P]` does for this run, and how it says so: push a run never
/// published, update a pull request a later cycle has left behind, or open
/// the one that already carries everything.
fn publish_action(details: &crate::app::RunDetails) -> (String, Color) {
    match details.publication.as_ref() {
        None => ("Push to PR".to_owned(), theme::success()),
        Some(publication) if publication.outdated => (
            format!("Update {} · behind", publication_name(publication)),
            theme::attention(),
        ),
        Some(publication) => (
            format!("Pushed ✓ {}", publication_name(publication)),
            theme::muted_color(),
        ),
    }
}

/// `#123` when the pull request is known, the branch otherwise.
fn publication_name(publication: &crate::app::Publication) -> String {
    publication
        .pull_request_url
        .as_deref()
        .and_then(|url| url.rsplit('/').next())
        .filter(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        .map_or_else(|| publication.branch.clone(), |number| format!("#{number}"))
}

fn hero_actions(
    applyable: bool,
    cycle: Option<&'static str>,
    publish: &(String, Color),
    status: StageStatus,
) -> Vec<Line<'static>> {
    let mut spans = Vec::new();
    // The three ways a finished run can go. Its diff is one key away in
    // either confirmation, and discard waits in the footer.
    if applyable {
        spans.extend(theme::action("a", "Apply changes", theme::success()));
        spans.push(Span::raw("   "));
        spans.extend(theme::action("P", &publish.0, publish.1));
        if let Some(label) = cycle {
            spans.push(Span::raw("   "));
            spans.extend(theme::action("f", label, theme::attention()));
        }
    } else if status == StageStatus::Failed {
        spans.extend(theme::action("l", "Logs", theme::accent()));
        spans.push(Span::raw("   "));
        spans.extend(theme::action("t", "Retry", theme::attention()));
        spans.push(Span::raw("   "));
        spans.extend(theme::action("d", "Diff", theme::accent()));
    } else {
        spans.extend(theme::action("o", "Result", theme::accent()));
        spans.push(Span::raw("   "));
        spans.extend(theme::action("l", "Logs", theme::accent()));
        spans.push(Span::raw("   "));
        spans.extend(theme::action("d", "Diff", theme::accent()));
        // A review reaches a verdict without ever becoming applyable, so this
        // is the only place its next cycle is ever offered.
        if let Some(label) = cycle {
            spans.push(Span::raw("   "));
            spans.extend(theme::action("f", label, theme::attention()));
        }
    }
    vec![Line::from(spans)]
}

/// Human stage name for operational rows; technical mode keeps the raw
/// serialized kind.
const fn stage_title(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Research => "Research",
        StageKind::Architecture => "Architecture",
        StageKind::Implementation => "Implementation",
        StageKind::Simplification => "Simplification",
        StageKind::CodeQualityReview => "Quality review",
        StageKind::SpecReview => "Spec review",
        StageKind::Review => "Review",
        StageKind::IndependentReview => "Independent review",
        StageKind::DeepAnalysis => "Deep analysis",
        StageKind::Synthesis => "Synthesis",
        StageKind::Decision => "Decision",
        StageKind::Fix => "Fix",
        StageKind::FollowUp => "Follow-up",
        StageKind::Lead => "Lead",
        StageKind::Verify => "Verify",
    }
}

/// One aligned `label  value` row inside a technical group.
fn technical_row(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<13}"), theme::muted()),
        Span::styled(value, theme::text()),
    ])
}

/// Right panel, technical view: every diagnostic the operational view hides,
/// grouped rather than listed.
#[allow(
    clippy::too_many_lines,
    reason = "one diagnostic panel keeps execution, runtime, evidence, workspace, and routing aligned"
)]
fn render_technical(frame: &mut Frame<'_>, area: Rect, state: &TuiState, details: &RunDetails) {
    let width = area.width.saturating_sub(3);
    let Some(selected) = details.stages.get(state.selected_stage_index) else {
        frame.render_widget(
            Paragraph::new("No stage selected")
                .style(theme::muted())
                .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
            area,
        );
        return;
    };
    let mut lines = vec![
        theme::spread(
            vec![Span::styled(
                "TECHNICAL",
                Style::default().add_modifier(Modifier::BOLD),
            )],
            vec![Span::styled(selected.id.to_string(), theme::muted())],
            width,
        ),
        Line::from(""),
        theme::section("EXECUTION"),
        technical_row("Stage", selected.id.to_string()),
        technical_row("Kind", enum_text(selected.kind)),
        technical_row("Role", enum_text(selected.role)),
        Line::from(vec![
            Span::styled(format!("  {:<13}", "Status"), theme::muted()),
            stage_visual(selected.status).badge(),
        ]),
        technical_row(
            "Process",
            selected
                .process_status
                .as_deref()
                .unwrap_or("unavailable")
                .to_owned(),
        ),
        technical_row("Effort", selected.requested_effort.label().to_owned()),
        Line::from(""),
        theme::section("RUNTIME"),
        technical_row(
            "Configured",
            format!(
                "{} / {}",
                selected.configured_provider,
                selected
                    .configured_model
                    .as_deref()
                    .unwrap_or("native default")
            ),
        ),
        technical_row(
            "Actual",
            format!(
                "{} / {}",
                selected.actual_provider.as_deref().unwrap_or("not started"),
                selected.actual_model.as_deref().unwrap_or("unconfirmed")
            ),
        ),
        technical_row(
            "Session",
            selected
                .provider_session_status
                .as_deref()
                .unwrap_or("unavailable")
                .to_owned(),
        ),
        technical_row(
            "Native",
            selected
                .native_session
                .as_deref()
                .map_or("unavailable", short_id)
                .to_owned(),
        ),
        Line::from(""),
        theme::section("RESOURCE EVIDENCE"),
    ];
    // Per-stage execution evidence: every row below is scoped to the selected
    // stage. Usage in particular is the stage's own, not the run's — one
    // stage runs on one runtime, so the units here need no disclaimer. The
    // run-wide, per-runtime breakdown belongs to the RESOURCES section.
    // Provider latency stays here and is never presented as the stage's
    // wall-clock elapsed time.
    if let Some(evidence) = state.evidence.as_ref() {
        lines.push(technical_row(
            "Usage",
            provider_resources(&stage_usage(evidence)),
        ));
        lines.push(technical_row(
            "Invocations",
            evidence.invocation_count.to_string(),
        ));
        lines.push(technical_row(
            "Latency",
            evidence.latency_ms.map_or_else(
                || "unavailable".to_owned(),
                |ms| format!("{ms} ms provider"),
            ),
        ));
        lines.push(technical_row(
            "Prompt",
            evidence.injected_prompt_bytes.map_or_else(
                || "unavailable".to_owned(),
                |bytes| format!("{bytes} injected bytes"),
            ),
        ));
        // Requested effort and observed effort are different facts. A
        // native-default request asks for nothing, so the level the runtime
        // then chose is visible only here, and only when it recorded one.
        lines.push(technical_row(
            "Effort",
            format!(
                "{} requested → {} observed",
                selected.requested_effort.label(),
                evidence.native_effort.as_deref().unwrap_or("unobserved")
            ),
        ));
        if let Some(version) = evidence.provider_cli_version.as_deref() {
            lines.push(technical_row("CLI", version.to_owned()));
        }
    }
    lines.extend([
        Line::from(""),
        theme::section("WORKSPACE"),
        technical_row(
            "State",
            details
                .workspace_status
                .map_or("unavailable".to_owned(), |status| {
                    format!("{status:?}").to_lowercase()
                }),
        ),
        technical_row(
            "Base commit",
            details
                .base_commit
                .as_deref()
                .unwrap_or("unavailable")
                .to_owned(),
        ),
        technical_row(
            "Repository",
            details
                .repository
                .as_deref()
                .map_or("unavailable".to_owned(), |path| path.display().to_string()),
        ),
        Line::from(""),
        theme::section("ROUTING"),
        technical_row("Run", details.id.to_string()),
        technical_row(
            "Profile",
            format!("{} ({})", details.profile, details.profile_version),
        ),
    ]);
    for route in &details.routes {
        // Role names run long enough to collide with an aligned value column,
        // so routes read as one sentence per line instead.
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", enum_text(route.role)), theme::text()),
            Span::styled(
                format!(
                    "→ {} / {} ({})",
                    route.configured_provider,
                    route
                        .configured_model
                        .as_deref()
                        .unwrap_or("native default"),
                    route.reason
                ),
                theme::muted(),
            ),
        ]));
    }
    lines.extend([
        Line::from(""),
        Line::from(theme::action("i", "operational view", theme::muted_color())),
    ]);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

/// A viewer's own heading: what is being read, and in which mode. The screen
/// is already framed by the header and footer rules, so no box is added.
fn viewer_heading(title: String, meta: String, width: u16) -> Vec<Line<'static>> {
    vec![
        theme::spread(
            vec![Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            )],
            vec![Span::styled(meta, theme::muted())],
            width,
        ),
        theme::rule(width),
    ]
}

fn render_artifact(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let width = area.width.saturating_sub(3);
    let mut lines = state.artifact.as_ref().map_or_else(
        || viewer_heading("ARTIFACT".to_owned(), "unavailable".to_owned(), width),
        |artifact| {
            viewer_heading(
                stage_title_for(&artifact.summary.stage_id.to_string()),
                format!(
                    "attempt {} · {}",
                    artifact.summary.attempt,
                    if state.artifact_raw {
                        "raw · [m] rendered"
                    } else {
                        "rendered · [m] raw"
                    }
                ),
                width,
            )
        },
    );
    if let Some(artifact) = state.artifact.as_ref() {
        if state.artifact_raw {
            lines.extend(
                artifact
                    .text
                    .lines()
                    .map(|line| Line::from(format::viewer_line(line))),
            );
        } else {
            lines.extend(markdown::render_markdown(&artifact.text));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((state.scroll, 0))
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

/// Artifact headings read as the stage that produced them.
fn stage_title_for(stage_id: &str) -> String {
    stage_id.replace('_', " ").to_uppercase()
}

fn render_logs(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let width = area.width.saturating_sub(3);
    let mut lines = viewer_heading(
        "RAW OUTPUT".to_owned(),
        state.logs.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |logs| {
                format!(
                    "{} · {} · read-only",
                    short_id(&logs.process_id.to_string()),
                    logs.process_status
                )
            },
        ),
        width,
    );
    if let Some(logs) = state.logs.as_ref() {
        for (label, stream) in [("STDOUT", &logs.stdout), ("STDERR", &logs.stderr)] {
            lines.push(Line::from(""));
            lines.push(theme::section(label));
            if stream.truncated {
                lines.push(Line::from(Span::styled("[tail truncated]", theme::muted())));
            }
            lines.extend(
                stream
                    .text
                    .lines()
                    .map(|line| Line::from(format::viewer_line(line))),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((state.scroll, 0))
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

fn render_diff(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let width = area.width.saturating_sub(3);
    let mut lines = viewer_heading(
        "WORKSPACE DIFF".to_owned(),
        state.diff.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |diff| format!("{} files · read-only", diff.changed_files.len()),
        ),
        width,
    );
    if let Some(diff) = state.diff.as_ref() {
        if diff.truncated {
            lines.push(Line::from(Span::styled(
                format!(
                    "[preview truncated at 2 MiB; total {} bytes]",
                    diff.total_bytes
                ),
                Style::default().fg(theme::attention()),
            )));
        }
        for line in diff.text.lines() {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                Style::default().fg(theme::accent())
            } else if line.starts_with('+') {
                Style::default().fg(theme::success())
            } else if line.starts_with('-') {
                Style::default().fg(theme::danger())
            } else if line.starts_with("diff --git") || line.starts_with("@@") {
                theme::diff_hunk()
            } else {
                theme::text()
            };
            lines.push(Line::styled(format::viewer_line(line), style));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((state.scroll, 0))
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

/// The entry point to the Mission Deck: the same field semantics, presented
/// as a briefing rather than a raw form.
fn render_new_run(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let form = &state.new_run;
    let width = area.width.saturating_sub(3);
    let workflow = enum_text(form.workflow);
    let task = field_display(&form.task, form.focus == 0);
    let repository = field_display(&form.repository, form.focus == 2);
    let mut lines = vec![
        Line::from(Span::styled(
            "START A RUN",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "One task, routed through specialist agents.",
            theme::muted(),
        )),
        theme::rule(width),
    ];
    for (label, value, focused) in [
        ("TASK", task.as_str(), form.focus == 0),
        ("WORKFLOW", workflow.as_str(), form.focus == 1),
        ("REPOSITORY", repository.as_str(), form.focus == 2),
        ("EXECUTION", form.execution.label(), form.focus == 3),
        (
            "EFFORT",
            super::state::effort_label(form.effort),
            form.focus == 4,
        ),
    ] {
        lines.push(Line::from(""));
        lines.push(theme::section(label));
        lines.push(Line::from(vec![
            Span::styled(
                if focused { "▸ " } else { "  " },
                Style::default().fg(theme::accent()),
            ),
            Span::styled(
                value.to_owned(),
                if focused {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    theme::text()
                },
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(theme::rule(width));
    lines.push(Line::from(""));
    let mut actions = theme::action("Enter", "Start run", theme::accent());
    actions.push(Span::raw("   "));
    actions.extend(theme::action("Esc", "Cancel", theme::muted_color()));
    lines.push(Line::from(actions));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::new(2, 1, 1, 0))),
        area,
    );
}

/// The screen's contextual actions, gated on canonical state so the footer
/// never advertises something the domain would refuse. Apply and discard
/// appear only for an applyable run.
/// The Runs list's own keys. Archiving is always on offer; deleting is
/// offered only where it is allowed, so the footer never advertises a key
/// that answers with a refusal.
fn runs_actions(state: &TuiState, push: &mut impl FnMut(&str, &str, Color)) {
    push("Enter", "Open", theme::accent());
    push("n", "New run", theme::accent());
    if let Some(run) = state.runs.get(state.selected_run_index) {
        push(
            "h",
            if run.archived { "Unarchive" } else { "Archive" },
            theme::muted_color(),
        );
        if run.archived {
            push("D", "Delete forever", theme::danger());
        }
    }
    if state.show_archived {
        push("H", "Hide archived", theme::muted_color());
    } else if state.archived_count > 0 {
        push("H", "Show archived", theme::muted_color());
    }
}

/// The run screen's contextual actions.
///
/// Split out of `primary_actions` so each screen's offer reads on its own.
fn run_detail_actions(state: &TuiState, push: &mut impl FnMut(&str, &str, Color)) {
    let needs_user = state
        .details
        .as_ref()
        .is_some_and(|details| !details.attention.is_empty());
    let stage_status = state
        .details
        .as_ref()
        .and_then(|details| details.stages.get(state.selected_stage_index))
        .map(|stage| stage.status);
    let run_status = state.details.as_ref().map(|details| details.status);
    let armed = state
        .details
        .as_ref()
        .is_some_and(|details| details.auto_approve);
    if needs_user {
        push("u", "Resolve attention", theme::attention());
        push(
            "A",
            if armed {
                "Ask me again"
            } else {
                "Auto-approve"
            },
            theme::attention(),
        );
        push("l", "Logs", theme::accent());
        push("s", "Stop", theme::attention());
    } else if state.run_is_applyable() {
        // The panel's own row carries apply, push and the next cycle; the
        // footer holds only what that row leaves out.
        push("X", "Discard", theme::danger());
    } else {
        push("o", "Result", theme::accent());
        push("l", "Logs", theme::accent());
        push("d", "Diff", theme::accent());
        if stage_status == Some(StageStatus::Failed) {
            push("t", "Retry", theme::attention());
        }
        if let Some(label) = cycle_label(state) {
            push("f", label, theme::attention());
        }
        // A running run normally has its driver in this process and
        // needs no key; one nobody holds was left behind by a dead
        // instance, and resume is how it gets a driver again.
        let orphaned = run_status == Some(RunStatus::Running)
            && state
                .selected_run
                .is_some_and(|run_id| !state.run_is_held(run_id));
        if matches!(
            run_status,
            Some(RunStatus::Paused | RunStatus::Interrupted | RunStatus::Ready)
        ) || orphaned
        {
            push("r", "Resume", theme::attention());
        }
        if state.run_is_stoppable() {
            push("s", "Stop", theme::attention());
        }
        // Arming is offered on the screen where it is felt — a run stopped
        // on a request, above. Here only the way back is, and only while it
        // is armed: a standing permission must never be one nobody can see
        // how to withdraw, but an unarmed run has nothing to say about it and
        // the row's space belongs to the keys that act on this run.
        if armed {
            push("A", "Ask me again", theme::muted_color());
        }
    }
    push(
        "i",
        if state.technical {
            "Operational"
        } else {
            "Details"
        },
        theme::muted_color(),
    );
}

fn primary_actions(screen: Screen, state: &TuiState) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push = |key: &str, label: &str, color: Color| {
        if !spans.is_empty() {
            spans.push(Span::raw("   "));
        }
        spans.extend(theme::action(key, label, color));
    };
    match screen {
        Screen::Runs => runs_actions(state, &mut push),
        Screen::RunDetail => run_detail_actions(state, &mut push),
        Screen::Missions => {
            push("Enter", "Open", theme::accent());
            push("W", "Enter the Senate", theme::muted_color());
            push("R", "Runs", theme::muted_color());
        }
        Screen::MissionDetail => mission_detail_actions(state, &mut push),
        Screen::Artifact => push("m", "Raw/rendered", theme::accent()),
        Screen::Logs | Screen::Diff => push("Esc", "Back", theme::accent()),
        Screen::NewRun => {
            push("Enter", "Start run", theme::accent());
            push("Esc", "Cancel", theme::muted_color());
        }
    }
    spans
}

/// The mission screen's contextual actions: only what the selected package
/// can take right now.
fn mission_detail_actions(state: &TuiState, push: &mut impl FnMut(&str, &str, Color)) {
    selected_order_actions(state, push);
    push("W", "Enter the Senate", theme::muted_color());
}

/// What the selected Order can take right now, plus the campaign's own
/// auto-approve toggle. Shared by the footer and the Campaign pane so the
/// two action rows never drift apart.
fn selected_order_actions(state: &TuiState, push: &mut impl FnMut(&str, &str, Color)) {
    if let Some(package) = state.selected_package_summary() {
        match package.status {
            WorkPackageStatus::Ready => push("S", "Start", theme::accent()),
            WorkPackageStatus::Blocked => push("u", "Answer", theme::attention()),
            WorkPackageStatus::Delivered => push("I", "Bring in", theme::success()),
            _ => {}
        }
        if package.current_run.is_some() {
            push("Enter", "Open run", theme::accent());
        }
    }
    let armed = state
        .mission
        .as_ref()
        .is_some_and(|mission| state.auto_approve_missions.contains(&mission.id));
    push(
        "A",
        if armed {
            "Auto-approve off"
        } else {
            "Auto-approve"
        },
        theme::muted_color(),
    );
}

/// Quiet navigation, in full and compact shapes. Navigation is what gets
/// dropped when the row is tight — never the contextual actions.
const fn navigation_hints(screen: Screen, from_mission: bool) -> (&'static str, &'static str) {
    match screen {
        Screen::RunDetail if from_mission => (
            "↑↓ stages · → open · ←/Esc mission · ? help · q quit/detach",
            "↑↓ · ←/Esc mission",
        ),
        Screen::Runs => (
            "↑↓ runs · → open · n new · M missions · ? help · q quit/detach",
            "↑↓ · → open",
        ),
        Screen::Missions => (
            "↑↓ missions · → open · ←/Esc runs · ? help · q quit/detach",
            "↑↓ · → open",
        ),
        Screen::MissionDetail => (
            "↑↓ Orders · → open run · ←/Esc campaigns · ? help · q quit/detach",
            "↑↓ · ←/Esc campaigns",
        ),
        Screen::RunDetail => (
            "↑↓ stages · → open · ←/Esc runs · ? help · q quit/detach",
            "↑↓ · ←/Esc runs",
        ),
        Screen::Artifact | Screen::Logs | Screen::Diff => (
            "↑↓/PgUp/PgDn scroll · ←/Esc run detail · ? help",
            "↑↓ scroll · ←/Esc run detail",
        ),
        Screen::NewRun => (
            "Tab/Shift-Tab fields · ←→ choices/edit · Ctrl-U clear line · ? help",
            "Tab fields",
        ),
    }
}

fn span_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

/// Footer row: contextual actions on the left, quiet navigation right-aligned
/// and dropped progressively as the terminal narrows.
fn footer_line(screen: Screen, state: &TuiState, width: u16) -> Line<'static> {
    let actions = primary_actions(screen, state);
    let (full, compact) = navigation_hints(screen, state.run_opened_from_mission);
    let used = span_width(&actions);
    let navigation = if used + full.chars().count() + 3 <= width as usize {
        full
    } else if used + compact.chars().count() + 3 <= width as usize {
        compact
    } else {
        ""
    };
    theme::spread(
        actions,
        vec![Span::styled(navigation, theme::muted())],
        width,
    )
}

fn message_presentation(kind: UiMessageKind) -> (&'static str, Style) {
    match kind {
        UiMessageKind::Info => ("ℹ", Style::default().fg(theme::accent())),
        UiMessageKind::Success => ("✓", Style::default().fg(theme::success())),
        UiMessageKind::Warning => ("⚠", Style::default().fg(theme::attention())),
        UiMessageKind::Error => (
            "✗",
            Style::default()
                .fg(theme::danger())
                .add_modifier(Modifier::BOLD),
        ),
    }
}

/// The notification split into the rows it needs.
///
/// A message the operator has to act on — which command was refused, why an
/// action failed — is worthless truncated, so it wraps on word boundaries
/// rather than running off the edge. Capped: the footer is a footer, and a
/// runaway provider string must never eat the screen.
fn message_rows(text: &str, width: u16) -> Vec<String> {
    const MAX_ROWS: usize = 4;
    // Two columns for the glyph, then room for the dismiss hint on the right.
    let usable = usize::from(width).saturating_sub(2 + DISMISS_HINT.chars().count() + 2);
    if usable < 8 {
        return vec![text.to_owned()];
    }
    let mut rows: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match rows.last_mut() {
            Some(row) if row.chars().count() + 1 + word.chars().count() <= usable => {
                row.push(' ');
                row.push_str(word);
            }
            _ => rows.push(word.to_owned()),
        }
    }
    if rows.len() > MAX_ROWS {
        rows.truncate(MAX_ROWS);
        if let Some(row) = rows.last_mut() {
            row.push('\u{2026}');
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

const DISMISS_HINT: &str = "x dismiss";

const HELP_TEXT: &str = "Global  ↑↓/j/k navigate · Enter/→ open · Esc/← back
n new · R runs · M campaigns · x dismiss · ? help
q/Ctrl-C quit
Run  Enter/o artifact · r resume · s stop · t retry
u attention · A approve · l logs · e task · d diff
a apply · b rebase · P PR · X discard
f cycle · c/w continue · i technical
Campaign  ↑↓/j/k Orders · Enter run · S start
u answer · I bring in · A approve · W Senate
Runs  h archive · H archived · D delete
Text  Ctrl-U/K/W/Alt-Backspace edit
Artifact  m raw/rendered";

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let mut lines = Vec::new();
    if let Some(message) = state.message.as_ref() {
        let (glyph, style) = message_presentation(message.kind);
        let rows = message_rows(&message.text, area.width);
        let last = rows.len().saturating_sub(1);
        for (index, row) in rows.into_iter().enumerate() {
            let prefix = if index == 0 {
                format!("{glyph} ")
            } else {
                "  ".to_owned()
            };
            // The dismiss hint sits once, beside the message's last row.
            let hint = if index == last {
                vec![Span::styled(DISMISS_HINT, theme::muted())]
            } else {
                Vec::new()
            };
            lines.push(theme::spread(
                vec![Span::styled(prefix, style), Span::styled(row, style)],
                hint,
                area.width,
            ));
        }
    }
    lines.push(footer_line(state.screen, state, area.width));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme::muted()),
        ),
        area,
    );
}

fn render_overlay(frame: &mut Frame<'_>, area: Rect, state: &TuiState, overlay: Overlay) {
    let popup = if overlay == Overlay::Help {
        centered_rect(90, 90, area)
    } else {
        centered_rect(78, 70, area)
    };
    frame.render_widget(Clear, popup);
    match overlay {
        Overlay::Help => frame.render_widget(
            Paragraph::new(HELP_TEXT)
                .block(overlay_block(" Help · Esc closes ", theme::muted_color())),
            popup,
        ),
        Overlay::Attention => render_attention(frame, popup, state),
        Overlay::Update => render_update(frame, area, state),
        Overlay::ApplyConfirm => render_confirmation(frame, popup, state, Confirmation::Apply),
        Overlay::IntegrateConfirm => render_integrate_confirmation(frame, popup, state),
        Overlay::RebaseConfirm => render_confirmation(frame, popup, state, Confirmation::Rebase),
        Overlay::PublishConfirm => render_confirmation(frame, popup, state, Confirmation::Publish),
        Overlay::DiscardConfirm => render_confirmation(frame, popup, state, Confirmation::Discard),
        Overlay::DeleteConfirm => render_delete_confirmation(frame, popup, state),
        Overlay::Continue => render_continue(frame, popup, state),
        Overlay::FollowUps => render_follow_ups(frame, popup, state),
        Overlay::NextCycle => render_next_cycle(frame, popup, state),
        Overlay::RetryRoute => render_retry_route(frame, popup, state),
        Overlay::Publishing => render_publishing(frame, area, state),
        Overlay::Published => render_published(frame, area, state),
        Overlay::Starting => render_starting(frame, area, state),
        Overlay::StartFailed => render_start_failed(frame, area, state),
    }
}

/// How long a pull request check may go unanswered before the starting card
/// says what usually causes that. `gh` gives up on its own after about thirty
/// seconds; this is well inside that, and well past a healthy answer.
const SLOW_PULL_REQUEST_CHECK: u64 = 8;

/// A compact band that says a run is on its way: which step the start is on,
/// for how long, and that hiding the card does not stop it.
fn render_starting(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(starting) = state.starting.as_ref() else {
        return;
    };
    let spinner = PUBLISH_SPINNER[usize::from(state.motion_frame().active_phase()) % 8];
    let elapsed = starting.started.elapsed().as_secs();
    let step = match &starting.progress {
        None => "Checking the repository and the task.".to_owned(),
        Some(StartProgress::FindingCheckout { repository }) => {
            format!("Looking for a local checkout of {repository}.")
        }
        Some(StartProgress::CheckingPullRequest { url, .. }) => {
            format!("Checking that gh can read {url}.")
        }
        Some(StartProgress::PreparingWorkspace(_)) => "Creating the run's worktree.".to_owned(),
        Some(StartProgress::Running(_)) => "Starting the first stage.".to_owned(),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "STARTING RUN",
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {elapsed}s"), theme::muted()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            starting.task.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(step, theme::text())),
    ];
    if let Some(StartProgress::CheckingPullRequest { host, .. }) = &starting.progress
        && elapsed >= SLOW_PULL_REQUEST_CHECK
    {
        lines.push(Line::from(Span::styled(
            format!("No answer from {host} yet. VPN or proxy off? gh gives up after about 30s."),
            Style::default().fg(theme::attention()),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Esc hides this card; the run keeps starting in the background.",
        theme::muted(),
    )));
    let popup = update_rect(area, &lines);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" NEW RUN ", theme::accent())),
        popup,
    );
}

/// Why a start was refused, in full. Refusals carry their own fix on the
/// lines after the first, so every line is kept.
fn render_start_failed(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(failure) = state.start_failure.as_ref() else {
        return;
    };
    let mut lines = vec![
        Line::from(theme::chip("RUN NOT STARTED", theme::danger())),
        Line::from(""),
        Line::from(Span::styled(
            failure.task.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (index, line) in failure.error.lines().enumerate() {
        let style = if index == 0 {
            Style::default().fg(theme::danger())
        } else {
            theme::text()
        };
        lines.push(Line::from(Span::styled(line.trim().to_owned(), style)));
    }
    if failure.draft_restored {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Your task is back in the new run form: press n to edit it and try again.",
            theme::muted(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(theme::action(
        "Enter",
        "close",
        theme::muted_color(),
    )));
    let popup = update_rect(area, &lines);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" NEW RUN ", theme::danger())),
        popup,
    );
}

/// The eight-tick spinner the publishing card turns while the branch
/// travels. Tick 0 is the resting glyph, like every other piece of motion.
const PUBLISH_SPINNER: [&str; 8] = ["◐", "◓", "◑", "◒", "◐", "◓", "◑", "◒"];

/// A compact band that says the pull request is on its way: what is being
/// done, for how long, and that hiding the card does not stop it. There is
/// no step list because the workspace does not report steps; the elapsed
/// time is what tells the operator the thing is still moving.
fn render_publishing(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let spinner = PUBLISH_SPINNER[usize::from(state.motion_frame().active_phase()) % 8];
    let elapsed = state
        .publishing
        .map(|publishing| publishing.started.elapsed().as_secs())
        .unwrap_or_default();
    let task = state
        .details
        .as_ref()
        .and_then(|details| details.task.clone())
        .unwrap_or_else(|| "this run".to_owned());
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "PUSHING TO PR",
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {elapsed}s"), theme::muted()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            task,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Committing the workspace, pushing the branch to origin, opening the pull request with gh.",
            theme::text(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Esc hides this card; the publish carries on in the background.",
            theme::muted(),
        )),
    ];
    let popup = update_rect(area, &lines);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" PUSH TO PR ", theme::accent())),
        popup,
    );
}

/// What the publish did, with the URL as the headline. The card stays until
/// dismissed so the URL can be read at leisure, opened with `o`, or copied
/// with `y`; a footer message alone was gone in four seconds.
fn render_published(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(outcome) = state.published.as_ref() else {
        return;
    };
    let (title, color) = match outcome {
        PublishOutcome::Pushed {
            pull_request: PullRequestStatus::Created(_),
            ..
        } => ("PULL REQUEST CREATED", theme::success()),
        PublishOutcome::Pushed {
            pull_request: PullRequestStatus::AlreadyExists(_),
            ..
        } => ("PULL REQUEST UPDATED", theme::success()),
        PublishOutcome::Pushed {
            pull_request: PullRequestStatus::Unavailable(_),
            ..
        } => ("BRANCH PUSHED", theme::attention()),
        PublishOutcome::Failed { .. } => ("PUBLISH FAILED", theme::danger()),
    };
    let mut lines = vec![Line::from(theme::chip(title, color)), Line::from("")];
    match outcome {
        PublishOutcome::Pushed {
            branch,
            commit,
            pull_request,
            note,
            ..
        } => {
            match pull_request {
                PullRequestStatus::Created(url) | PullRequestStatus::AlreadyExists(url) => {
                    lines.push(Line::from(Span::styled(
                        url.clone(),
                        Style::default()
                            .fg(theme::accent())
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    )));
                    if matches!(pull_request, PullRequestStatus::AlreadyExists(_)) {
                        lines.push(Line::from(Span::styled(
                            "The pull request was already open; the branch now carries the latest commit.",
                            theme::muted(),
                        )));
                    }
                }
                PullRequestStatus::Unavailable(reason) => {
                    lines.push(Line::from(Span::styled(
                        "The branch is on origin, but no pull request was opened:",
                        theme::text(),
                    )));
                    lines.push(Line::from(Span::styled(
                        reason.clone(),
                        Style::default().fg(theme::attention()),
                    )));
                }
            }
            if let Some(note) = note {
                // The operator asked to fix a pull request and got a branch of
                // its own instead; the card is where that has to be said.
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    note.clone(),
                    Style::default().fg(theme::attention()),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("branch  ", theme::muted()),
                Span::styled(branch.clone(), theme::text()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("commit  ", theme::muted()),
                Span::styled(commit.chars().take(12).collect::<String>(), theme::text()),
            ]));
        }
        PublishOutcome::Failed { error, .. } => {
            lines.push(Line::from(Span::styled(
                "Nothing reached origin.",
                theme::text(),
            )));
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::default().fg(theme::danger()),
            )));
        }
    }
    lines.push(Line::from(""));
    let mut actions = Vec::new();
    if outcome.url().is_some() {
        actions.extend(theme::action("o", "open in browser", color));
        actions.push(Span::raw("   "));
        actions.extend(theme::action("y", "copy URL", color));
        actions.push(Span::raw("   "));
    }
    actions.extend(theme::action("Enter", "close", theme::muted_color()));
    lines.push(Line::from(actions));
    let popup = update_rect(area, &lines);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" PULL REQUEST ", color)),
        popup,
    );
}

fn render_retry_route(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let selected = state
        .details
        .as_ref()
        .and_then(|details| details.stages.get(state.selected_stage_index));
    let mut lines = vec![
        Line::from(theme::chip("RETRY", theme::attention())),
        Line::from(""),
    ];
    if let Some(stage) = selected {
        lines.push(Line::from(vec![
            Span::styled(stage_title_for(stage.id.as_str()), theme::text()),
            Span::styled(
                format!(" · configured {}", stage.configured_provider),
                theme::muted(),
            ),
        ]));
        lines.push(Line::from(""));
    }
    for choice in state.retry_route_choices() {
        let highlighted = state.retry_route_choice == choice;
        let label = match choice {
            RetryRouteChoice::Configured => selected.map_or_else(
                || "Configured provider".to_owned(),
                |stage| format!("Configured provider ({})", stage.configured_provider),
            ),
            RetryRouteChoice::CodexModel(model) => {
                format!("Codex on {model} (the account refused the model it ran on)")
            }
            RetryRouteChoice::Claude => "Claude (native default model)".to_owned(),
            RetryRouteChoice::Codex => "Codex (native default model)".to_owned(),
        };
        lines.push(Line::from(vec![
            Span::styled(
                if highlighted { "  → " } else { "    " },
                Style::default().fg(theme::attention()),
            ),
            Span::styled(
                label,
                if highlighted {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    theme::muted()
                },
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Only this stage moves; the run's routing stays as configured.",
        theme::muted(),
    )));
    lines.push(Line::from(Span::styled(
        "↑/↓ choose · Enter retry · Esc cancel",
        theme::muted(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" Retry stage ", theme::attention())),
        area,
    );
}

/// Overlays are the one place a full border earns its keep: they float over
/// the deck and need their own edge.
fn overlay_block(title: &'static str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(title)
        .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .padding(Padding::new(1, 1, 0, 0))
}

/// The update prompt: compact, Mission Deck-native, and never larger than it
/// needs to be. It states both versions, says what installing would mean, and
/// — when The Senate cannot install safely — says so instead of offering a
/// button that would lie.
fn render_update(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(info) = state.update.as_ref() else {
        return;
    };
    let installable = state.update_is_installable();
    let mut lines = vec![
        Line::from(Span::styled(
            "UPDATE AVAILABLE",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("The Senate ", theme::text()),
            Span::styled(info.current_version.to_string(), theme::muted()),
            Span::styled(" → ", theme::muted()),
            Span::styled(
                info.available_version.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];
    if installable {
        lines.push(Line::from(Span::styled("Install now?", theme::text())));
        lines.push(Line::from(Span::styled(
            "It applies when The Senate restarts.",
            theme::muted(),
        )));
        lines.push(Line::from(""));
        for (selected, label) in [
            (state.update_install_selected, "Yes"),
            (!state.update_install_selected, "No"),
        ] {
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "  → " } else { "    " },
                    Style::default().fg(theme::accent()),
                ),
                Span::styled(
                    label,
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        theme::muted()
                    },
                ),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            state
                .update_install
                .map_or_else(String::new, |source| source.strategy().guidance()),
            theme::text(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(theme::action(
            "Enter",
            "Continue",
            theme::accent(),
        )));
    }
    // The prompt is an aside, not a takeover: it occupies a compact band
    // rather than the whole screen.
    let popup = update_rect(area, &lines);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" UPDATE ", theme::accent())),
        popup,
    );
}

/// A band wide enough for one version line and tall enough for its content
/// *after wrapping*, centered, and always inside the terminal.
fn update_rect(area: Rect, lines: &[Line<'_>]) -> Rect {
    let width = area.width.saturating_sub(4).clamp(20, 62);
    // Borders take two columns and the block pads one on each side, so the
    // band must be measured against the width the text actually wraps to.
    let inner = usize::from(width).saturating_sub(4).max(1);
    let rows: usize = lines
        .iter()
        .map(|line| span_width(&line.spans).div_ceil(inner).max(1))
        .sum();
    let height = u16::try_from(rows + 2).unwrap_or(u16::MAX).min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn render_attention(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(details) = state.details.as_ref() else {
        return;
    };
    let mut lines = vec![
        Line::from(theme::chip("⚠ NEEDS YOU", theme::attention())),
        Line::from(""),
    ];
    for (index, attention) in details.attention.iter().enumerate() {
        let selected = index == state.attention_index;
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "▸ " } else { "  " },
                Style::default().fg(theme::attention()),
            ),
            Span::styled(
                format!("{} · {}", enum_text(attention.kind), attention.summary),
                if selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    theme::text()
                },
            ),
        ]));
    }
    lines.push(Line::from(""));
    let selected_kind = details
        .attention
        .get(state.attention_index)
        .map(|attention| attention.kind);
    if selected_kind == Some(AttentionKind::Permission) {
        lines.push(Line::from(Span::styled(
            "Permission request — Enter approves · Ctrl-S skips it · a typed response continues without granting",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!(
            "Response (optional): {}",
            field_display(&state.attention_response, true)
        )));
        lines.push(Line::from(Span::styled(
            "↑/↓ select request · Enter approve · Ctrl-S skip · type to answer instead · Esc cancel",
            theme::muted(),
        )));
    } else {
        lines.push(Line::from(format!(
            "Response: {}",
            field_display(&state.attention_response, true)
        )));
        lines.push(Line::from(Span::styled(
            "↑/↓ select request · type response · Enter submit · Ctrl-U clear line · Esc cancel",
            theme::muted(),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" Attention ", theme::attention())),
        area,
    );
}

/// Single-field prompt for `[c]` Continue: an operator instruction that,
/// once submitted, becomes the follow-up stage's own task.
fn render_continue(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let lines = vec![
        Line::from(theme::chip("CONTINUE", theme::attention())),
        Line::from(""),
        Line::from("How should the agent continue?"),
        Line::from(""),
        Line::from(field_display(&state.continue_instruction, true)),
        Line::from(""),
        Line::from(Span::styled(
            "type instruction · Enter submit · Ctrl-U clear line · Esc cancel",
            theme::muted(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" Continue ", theme::attention())),
        area,
    );
}

/// Two-option chooser for `[w]` Work on follow-ups: continue the operator's
/// own extracted text in this run, or hand it to a new run's composer.
/// Mirrors the update prompt's own up/down toggle.
fn render_next_cycle(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let mut lines = vec![
        Line::from(theme::chip("NEXT CYCLE", theme::attention())),
        Line::from(""),
    ];
    for (index, choice) in state.next_cycle_choices.iter().enumerate() {
        let selected = index == state.next_cycle_selected;
        let mut spans = vec![Span::styled(
            if selected { "  → " } else { "    " },
            Style::default().fg(theme::attention()),
        )];
        spans.extend(theme::action(choice.key(), "", theme::attention()));
        spans.push(Span::styled(
            choice.label(),
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                theme::muted()
            },
        ));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ choose · Enter or key confirm · Esc cancel",
        theme::muted(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" Start another cycle ", theme::attention())),
        area,
    );
}

fn render_follow_ups(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let mut lines = vec![
        Line::from(theme::chip("FOLLOW-UPS", theme::attention())),
        Line::from(""),
    ];
    if let Some(text) = state.follow_ups_text.as_ref() {
        for line in text.lines() {
            lines.push(Line::from(Span::styled(
                format::viewer_line(line),
                theme::text(),
            )));
        }
        lines.push(Line::from(""));
    }
    for (selected, label) in [
        (!state.follow_ups_as_new_run, "In this run"),
        (state.follow_ups_as_new_run, "As new run"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "  → " } else { "    " },
                Style::default().fg(theme::attention()),
            ),
            Span::styled(
                label,
                if selected {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    theme::muted()
                },
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ choose · Enter confirm · Esc cancel",
        theme::muted(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" Work on follow-ups ", theme::attention())),
        area,
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirmation {
    Apply,
    Rebase,
    Publish,
    Discard,
}

/// Why the rebase confirmation is open, when an apply refusal opened it.
fn apply_refusal_lines(state: &TuiState, confirmation: Confirmation) -> Vec<Line<'static>> {
    let Some(reason) = state
        .rebase_reason
        .as_ref()
        .filter(|_| confirmation == Confirmation::Rebase)
    else {
        return Vec::new();
    };
    vec![
        Line::from(Span::styled(
            "Apply was refused: the checkout moved on since this run started.",
            theme::attention(),
        )),
        Line::from(Span::styled(reason.clone(), theme::muted())),
        Line::from(""),
    ]
}

/// What a publish is about to do, in the terms of this run's own task.
///
/// A run asked about a pull request pushes onto that pull request when it
/// can, so promising "opens a pull request" read as a threat to open a second
/// one for work that already has one. Where the task names no pull request,
/// the run's own branch and a new pull request remain the whole story.
fn publish_confirmation_line(task: Option<&str>) -> String {
    match task.and_then(PullRequestRef::parse) {
        Some(reference) => format!(
            "Commits on the run's branch and pushes onto {}, unless that branch moved ahead of \
             this run — then it opens a pull request of its own and says so. Your checkout is \
             untouched. [d] shows the full diff. Enter confirms.",
            reference.url()
        ),
        None => "Commits on the run's branch, pushes to origin, opens a pull request. Your \
                 checkout is untouched. [d] shows the full diff. Enter confirms."
            .to_owned(),
    }
}

fn render_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiState,
    confirmation: Confirmation,
) {
    let Some(details) = state.details.as_ref() else {
        return;
    };
    let (action, color) = match confirmation {
        Confirmation::Apply => ("APPLY", theme::success()),
        Confirmation::Rebase => ("REBASE", theme::attention()),
        Confirmation::Publish => ("PUSH TO PR", theme::success()),
        Confirmation::Discard => ("DISCARD", theme::danger()),
    };
    let mut lines = vec![
        Line::from(theme::chip(action, color)),
        Line::from(""),
        Line::from(Span::styled(
            details
                .task
                .as_deref()
                .unwrap_or("<legacy input unavailable>")
                .to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            details
                .repository
                .as_deref()
                .map_or("unavailable".to_owned(), |path| path.display().to_string()),
            theme::muted(),
        )),
        Line::from(Span::styled(format!("run {}", details.id), theme::muted())),
        Line::from(""),
    ];
    lines.extend(apply_refusal_lines(state, confirmation));
    if confirmation == Confirmation::Discard {
        lines.push(Line::from(
            "Discard is logical disposition; owned cleanup follows application semantics.",
        ));
        lines.push(Line::from("Enter confirms discard."));
    } else {
        if let Some(diff) = state.diff.as_ref() {
            lines.push(theme::section(&format!(
                "{} FILES",
                diff.changed_files.len()
            )));
            for file in diff.changed_files.iter().take(8) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}{}",
                        file.path,
                        if file.binary { " [binary]" } else { "" }
                    ),
                    theme::text(),
                )));
            }
            lines.push(Line::from(""));
        }
        if confirmation == Confirmation::Publish
            && let Some(stage) = state.failed_verification()
        {
            lines.push(Line::from(Span::styled(
                format!(
                    "Verification did not pass: stage {} is {}.",
                    stage.id,
                    format!("{:?}", stage.status).to_lowercase()
                ),
                theme::danger(),
            )));
            lines.push(Line::from(Span::styled(
                "Publishing anyway puts the change on a branch, not in your checkout.",
                theme::muted(),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(match confirmation {
            Confirmation::Apply => "[d] shows the full diff. Enter confirms apply.".to_owned(),
            Confirmation::Rebase => {
                "Replays this run's change on your checkout's current HEAD, on the run's own \
                 branch. A conflict stops and changes nothing. Verification has to run again \
                 afterwards before apply. Enter confirms."
                    .to_owned()
            }
            _ => publish_confirmation_line(details.task.as_deref()),
        }));
    }
    lines.push(Line::from(Span::styled("Esc cancels", theme::muted())));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(
                match confirmation {
                    Confirmation::Apply => " APPLY ",
                    Confirmation::Rebase => " REBASE ",
                    Confirmation::Publish => " PUSH TO PR ",
                    Confirmation::Discard => " DISCARD ",
                },
                color,
            )),
        area,
    );
}

/// The last stop before a run is gone.
///
/// Every other confirmation takes Enter, the key a hand rests on. This one
/// takes `D` again — the same key that opened it — so no reflex carries a
/// run past the point of return. POD stands at the plunger while the
/// overlay lists, in plain words, each thing that is about to stop existing.
fn render_delete_confirmation(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let selected = state.runs.get(state.selected_run_index);
    let task = selected.map_or_else(
        || {
            state
                .details
                .as_ref()
                .and_then(|details| details.task.clone())
                .unwrap_or_else(|| "this run".to_owned())
        },
        |run| run.task_summary.clone(),
    );
    let run_id = selected.map(|run| run.id).or(state.selected_run);
    let mut lines = vec![Line::from(theme::chip("DELETE FOREVER", theme::danger()))];
    if area.height >= mascot::MASCOT_HEIGHT + 12 {
        lines.push(Line::from(""));
        lines.extend(mascot::demolition_lines(state.motion_frame()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        task,
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if let Some(run_id) = run_id {
        lines.push(Line::from(Span::styled(
            format!("run {run_id}"),
            theme::muted(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "This run stops existing: its worktree and branch, its artifacts and \
         logs on disk, and every row The Senate keeps about it.",
    ));
    lines.push(Line::from(
        "Nothing it already applied or pushed is touched — your repository and \
         any pull request stay exactly as they are.",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "There is no undo. D deletes.",
        Style::default()
            .fg(theme::danger())
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled("Esc cancels", theme::muted())));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(overlay_block(" DELETE FOREVER ", theme::danger())),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// One state made visible: the glyph, the word, and the colour, bound
/// together.
///
/// Nothing here hands out a bare colour. A caller that wants to show a state
/// gets a span that already carries the state in characters, so stripping
/// every colour — `NO_COLOR`, a monochrome terminal, a colour-blind reader —
/// leaves the meaning intact. That rule used to hold by discipline, with
/// `run_glyph` and `status_style` sitting side by side and nothing stopping a
/// caller from reaching for the second alone; it now holds by construction.
///
/// It is also the single source for the glyph. The run status line used to
/// spell out its own `"✗ FAILED"`, duplicating the glyph table a few hundred
/// lines away and free to drift from it.
#[derive(Clone, Copy)]
struct StatusVisual {
    glyph: &'static str,
    label: &'static str,
    color: Color,
}

impl StatusVisual {
    /// The glyph alone, for rails where a neighbouring column carries the word.
    fn glyph(self) -> Span<'static> {
        Span::styled(format!("{} ", self.glyph), Style::new().fg(self.color))
    }

    /// Glyph and word together, for a single span that must stand on its own.
    fn badge(self) -> Span<'static> {
        Span::styled(
            format!("{} {}", self.glyph, self.label),
            Style::new().fg(self.color),
        )
    }

    /// As `badge`, emphasised where the state is the headline of its panel.
    fn badge_bold(self) -> Span<'static> {
        Span::styled(
            format!("{} {}", self.glyph, self.label),
            Style::new().fg(self.color).add_modifier(Modifier::BOLD),
        )
    }

    /// Glyph and a caller-chosen word, for a panel whose vocabulary reads
    /// plainer than the shouted `label` (the Campaign pane says "In
    /// progress", not "ACTIVE").
    fn phrase(self, phrase: &str) -> Span<'static> {
        Span::styled(
            format!("{} {phrase}", self.glyph),
            Style::new().fg(self.color).add_modifier(Modifier::BOLD),
        )
    }
}

fn run_visual(status: RunStatus) -> StatusVisual {
    let (glyph, label, color) = match status {
        RunStatus::Completed => ("✓", "COMPLETED", theme::success()),
        RunStatus::Applied => ("✓", "APPLIED", theme::success()),
        RunStatus::Running => ("●", "RUNNING", theme::accent()),
        RunStatus::NeedsUser => ("⚠", "NEEDS YOU", theme::attention()),
        RunStatus::Failed => ("✗", "FAILED", theme::danger()),
        RunStatus::Paused => ("‖", "PAUSED", theme::suspended()),
        RunStatus::Interrupted => ("↻", "INTERRUPTED", theme::suspended()),
        RunStatus::Ready => ("○", "WAITING", theme::attention()),
        RunStatus::Created | RunStatus::Preparing => ("○", "WAITING", theme::muted_color()),
        RunStatus::Discarded => ("×", "DISCARDED", theme::muted_color()),
    };
    StatusVisual {
        glyph,
        label,
        color,
    }
}

fn stage_visual(status: StageStatus) -> StatusVisual {
    let (glyph, label, color) = match status {
        StageStatus::Completed => ("✓", "COMPLETED", theme::success()),
        StageStatus::Running => ("●", "RUNNING", theme::accent()),
        StageStatus::NeedsUser => ("⚠", "NEEDS YOU", theme::attention()),
        StageStatus::Failed => ("✗", "FAILED", theme::danger()),
        StageStatus::Paused => ("‖", "PAUSED", theme::suspended()),
        StageStatus::Interrupted => ("↻", "INTERRUPTED", theme::suspended()),
        StageStatus::Ready => ("○", "READY", theme::attention()),
        StageStatus::Pending => ("○", "PENDING", theme::muted_color()),
        StageStatus::Skipped => ("○", "SKIPPED", theme::muted_color()),
    };
    StatusVisual {
        glyph,
        label,
        color,
    }
}

fn enum_text(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn field_display(field: &super::state::TextField, selected: bool) -> String {
    if !selected {
        return field.text().to_owned();
    }
    let byte = field
        .text()
        .char_indices()
        .nth(field.cursor())
        .map_or(field.text().len(), |(index, _)| index);
    let mut value = field.text().to_owned();
    value.insert(byte, '│');
    value
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{RouteSummary, RunListItem, StageSummary, StageWaitingSummary, UsageSummary};
    use crate::domain::{EffortSetting, Role, RunId, StageId, StageKind, WorkflowKind};
    use crate::tui::state::StageHeadline;

    // POD's legs, folded to half-blocks: the one art fragment every scene
    // keeps, so it marks "POD is on screen" regardless of costume or prop.
    const POD_SHELL: &str = "██    ██";

    /// The contract guarded where it can actually be reopened.
    ///
    /// `every_state_stays_distinguishable_with_all_colour_removed` proves the
    /// states are distinguishable today. It cannot prove they will stay that
    /// way: a caller only has to reach past the glyph for the colour, and that
    /// test keeps passing while the interface quietly loses its meaning for
    /// anyone without colour. So the two ways of reaching past it are closed
    /// here — `StatusVisual` may only produce spans, and production code may
    /// not read its colour field directly.
    /// The publish confirmation has to describe the publish this run will
    /// actually do: a run asked about a pull request pushes onto that pull
    /// request, and saying "opens a pull request" read as a promise to open a
    /// second one for work that already has one.
    #[test]
    fn the_publish_confirmation_names_the_pull_request_the_task_carries() {
        let named =
            publish_confirmation_line(Some("Review https://github.com/owner/repo/pull/7 please"));
        assert!(
            named.contains("https://github.com/owner/repo/pull/7"),
            "{named}"
        );
        assert!(named.contains("pushes onto"), "{named}");

        let plain = publish_confirmation_line(Some("Tidy the parser"));
        assert!(plain.contains("opens a pull request"), "{plain}");
        assert!(!plain.contains("pushes onto"), "{plain}");
        assert_eq!(plain, publish_confirmation_line(None));
    }

    #[test]
    fn a_status_visual_never_hands_out_a_bare_colour() {
        let source = include_str!("render.rs");
        let implementation = source
            .split("impl StatusVisual {")
            .nth(1)
            .expect("StatusVisual has an impl block")
            .split("\n}\n")
            .next()
            .expect("impl block body");
        for signature in implementation
            .lines()
            .filter(|line| line.trim_start().starts_with("fn "))
        {
            assert!(
                signature.contains("-> Span<'static>"),
                "a status must reach the screen carrying its glyph: {signature}"
            );
        }

        // Outside its own impl block, where reading the field is how the
        // spans get built in the first place.
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production half")
            .replace(implementation, "");
        assert!(
            !production.contains(".color"),
            "production code must take a span from StatusVisual, never its colour"
        );
    }

    /// The contract that lets the palette get richer later: strip every
    /// colour and the interface must still say which state each thing is in.
    /// Under Mono all tokens collapse to one value, so anything still legible
    /// is carried by a glyph or a word — never by hue. Without this, adding a
    /// vivid theme or motion would quietly let the aesthetic layer own
    /// meaning, and a colour-blind or piped reader would lose it.
    #[test]
    fn every_state_stays_distinguishable_with_all_colour_removed() {
        theme::with_palette(
            theme::Palette::resolve(theme::ColorCapability::Mono, theme::ThemeChoice::Native),
            || {
                // The states a reader must never confuse, whatever the terminal.
                let runs = [
                    RunStatus::Running,
                    RunStatus::Completed,
                    RunStatus::Failed,
                    RunStatus::NeedsUser,
                    RunStatus::Paused,
                    RunStatus::Interrupted,
                ];
                for (index, status) in runs.iter().enumerate() {
                    for other in runs.iter().skip(index + 1) {
                        assert_ne!(
                            run_visual(*status).glyph,
                            run_visual(*other).glyph,
                            "{status:?} and {other:?} are told apart only by colour"
                        );
                    }
                    // And colour genuinely carries nothing here, so the glyph above
                    // is doing the whole job rather than merely helping.
                    assert_eq!(
                        run_visual(*status).color,
                        run_visual(runs[0]).color,
                        "a status keeping its own colour under Mono is still using hue"
                    );
                }

                let stages = [
                    StageStatus::Running,
                    StageStatus::Completed,
                    StageStatus::Failed,
                    StageStatus::NeedsUser,
                    StageStatus::Paused,
                    StageStatus::Interrupted,
                ];
                for (index, status) in stages.iter().enumerate() {
                    for other in stages.iter().skip(index + 1) {
                        assert_ne!(
                            stage_visual(*status).glyph,
                            stage_visual(*other).glyph,
                            "{status:?} and {other:?} are told apart only by colour"
                        );
                    }
                    assert_eq!(stage_visual(*status).color, stage_visual(stages[0]).color);
                }
            },
        );
    }

    fn render_text(state: &TuiState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    /// Every foreground colour a frame actually paints.
    fn painted_colours(state: &TuiState, width: u16, height: u16) -> Vec<Color> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.fg)
            .collect()
    }

    /// One symbol per rendered cell, so a test can compare two frames cell by
    /// cell rather than as a run-together string.
    fn render_symbols(state: &TuiState, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect()
    }

    /// The footer's contextual half as plain text.
    fn actions_text(state: &TuiState) -> String {
        primary_actions(state.screen, state)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn at(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, hour, minute, second)
            .single()
            .unwrap()
    }

    fn stage(id: &str, kind: StageKind, role: Role, status: StageStatus) -> StageSummary {
        StageSummary {
            id: StageId::new(id).unwrap(),
            kind,
            role,
            status,
            configured_provider: "codex".to_owned(),
            requested_effort: EffortSetting::NativeDefault,
            observed_effort: None,
            configured_model: None,
            route_overridden: false,
            actual_provider: Some("codex".to_owned()),
            actual_model: None,
            provider_session_record: Some("session-record".to_owned()),
            native_session: Some("native-session-id".to_owned()),
            provider_session_status: Some("completed".to_owned()),
            process_status: Some("exited".to_owned()),
            started_at: None,
            finished_at: None,
            waiting: None,
            failure_reason: None,
            blocking: false,
        }
    }

    /// A skipped required dependency must never read as "failed": each
    /// blocked dependency states its own outcome.
    #[test]
    fn blocked_message_states_each_dependencys_own_outcome() {
        let mut pending = stage(
            "decision",
            StageKind::Decision,
            Role::EngineeringLead,
            StageStatus::Pending,
        );
        pending.waiting = Some(StageWaitingSummary {
            waiting_on: Vec::new(),
            blocked_by: vec![
                BlockedDependencyRef {
                    id: StageId::new("quality_review").unwrap(),
                    kind: StageKind::CodeQualityReview,
                    outcome: DependencyOutcome::Failed,
                },
                BlockedDependencyRef {
                    id: StageId::new("spec_review").unwrap(),
                    kind: StageKind::SpecReview,
                    outcome: DependencyOutcome::Skipped,
                },
            ],
            degraded: Vec::new(),
        });

        assert_eq!(
            waiting_message(&pending),
            "Blocked: Quality review failed, Spec review was skipped"
        );
    }

    fn details(status: RunStatus, stages: Vec<StageSummary>) -> RunDetails {
        RunDetails {
            id: RunId::from_u128(3),
            auto_approve: false,
            task: Some("Add OAuth provider support".to_owned()),
            workflow: WorkflowKind::Standard,
            status,
            repository: Some(std::path::PathBuf::from("/Users/e/Code/wp-calypso-2")),
            workspace_mode: Some(crate::workspace::WorkspaceMode::Branch),
            workspace_status: Some(crate::workspace::WorkspaceStatus::Ready),
            base_commit: Some("abc1234".to_owned()),
            profile: "recommended".to_owned(),
            profile_version: "recommended_v2".to_owned(),
            routes: vec![RouteSummary {
                role: Role::Implementer,
                configured_provider: "codex".to_owned(),
                configured_model: None,
                reason: "recommended_role_assignment".to_owned(),
                requested_effort: Some(crate::domain::EffortSetting::NativeDefault),
            }],
            revision: crate::store::RunRevision::initial(),
            created_at: at(12, 0, 0),
            updated_at: at(12, 5, 0),
            stages,
            attention: Vec::new(),
            usage: crate::app::RunUsage::from_totals([
                (
                    "claude".to_owned(),
                    UsageSummary {
                        input_units: 128,
                        output_units: 50_435,
                        cache_read_units: Some(4_373_955),
                        cache_write_units: Some(308_947),
                        reasoning_output_units: None,
                    },
                ),
                (
                    "codex".to_owned(),
                    UsageSummary {
                        input_units: 9_246_322,
                        output_units: 63_345,
                        cache_read_units: Some(8_885_760),
                        cache_write_units: Some(0),
                        reasoning_output_units: Some(31_932),
                    },
                ),
            ]),
            started_at: None,
            finished_at: None,
            failure_reason: None,
            image_generations: Vec::new(),
            publication: None,
        }
    }

    /// A run whose implementation stage is in flight.
    fn running_state() -> TuiState {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.screen = Screen::RunDetail;
        state.selected_run = Some(RunId::from_u128(3));
        let mut stages = vec![
            stage(
                "architecture",
                StageKind::Architecture,
                Role::Architect,
                StageStatus::Completed,
            ),
            stage(
                "implementation",
                StageKind::Implementation,
                Role::Implementer,
                StageStatus::Running,
            ),
            stage(
                "quality_review",
                StageKind::CodeQualityReview,
                Role::CodeQualityReviewer,
                StageStatus::Pending,
            ),
        ];
        stages[0].started_at = Some(at(12, 0, 0));
        stages[0].finished_at = Some(at(12, 2, 14));
        stages[1].started_at = Some(at(12, 2, 14));
        let mut run = details(RunStatus::Running, stages);
        run.started_at = Some(at(12, 0, 0));
        state.replace_details(run);
        state.selected_stage_index = 1;
        state.selected_stage = Some(StageId::new("implementation").unwrap());
        state
    }

    /// A completed, applyable run.
    fn completed_state() -> TuiState {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.screen = Screen::RunDetail;
        state.selected_run = Some(RunId::from_u128(3));
        let mut stages = vec![stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Completed,
        )];
        stages[0].started_at = Some(at(12, 0, 0));
        stages[0].finished_at = Some(at(12, 4, 32));
        let mut run = details(RunStatus::Completed, stages);
        run.started_at = Some(at(12, 0, 0));
        run.finished_at = Some(at(12, 12, 48));
        state.replace_details(run);
        state
    }

    /// The review workflow's real shape when an optional review fails and
    /// the run still completes: the decision ruled over the gap.
    fn completed_with_failure_details() -> RunDetails {
        let stages = vec![
            stage(
                "research",
                StageKind::Research,
                Role::Researcher,
                StageStatus::Completed,
            ),
            stage(
                "quality_review",
                StageKind::CodeQualityReview,
                Role::CodeQualityReviewer,
                StageStatus::Completed,
            ),
            stage(
                "spec_review",
                StageKind::SpecReview,
                Role::SpecReviewer,
                StageStatus::Failed,
            ),
            stage(
                "synthesis",
                StageKind::Synthesis,
                Role::EngineeringLead,
                StageStatus::Completed,
            ),
            stage(
                "decision",
                StageKind::Decision,
                Role::EngineeringLead,
                StageStatus::Completed,
            ),
        ];
        let mut run = details(RunStatus::Completed, stages);
        run.started_at = Some(at(12, 0, 0));
        run.finished_at = Some(at(12, 12, 48));
        run
    }

    fn completed_with_failure_state() -> TuiState {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.screen = Screen::RunDetail;
        state.selected_run = Some(RunId::from_u128(3));
        state.replace_details(completed_with_failure_details());
        state
    }

    #[test]
    fn empty_runs_and_small_terminal_render_without_panicking() {
        let state = TuiState::new(std::path::Path::new("/repo"));
        assert!(render_text(&state, 90, 24).contains("No runs yet"));
        assert!(render_text(&state, 49, 9).contains("Terminal too small"));
    }

    /// The rail's one task line is what the operator reads to know which run
    /// they are looking at; a long task lost everything past the first ~40
    /// characters with no way to see the rest.
    #[test]
    fn a_long_task_can_be_opened_in_the_rail_and_closed_again() {
        let task = "Build a static landing page (index.html + styles.css) for a \
                    hiking-trail company, with a hero, three trail cards and a \
                    contact form";
        let mut state = running_state();
        if let Some(details) = state.details.as_mut() {
            details.task = Some(task.to_owned());
        }

        let collapsed = render_text(&state, 160, 40);
        assert!(collapsed.contains('…'), "the rail line is still truncated");
        assert!(
            !collapsed.contains("contact form"),
            "the tail is not shown until it is asked for"
        );
        assert!(collapsed.contains("Full task"), "the key is offered");

        state.task_expanded = true;
        let expanded = render_text(&state, 160, 40);
        assert!(
            expanded.contains("contact form"),
            "expanding shows the whole task"
        );
        assert!(
            expanded.contains("Collapse task"),
            "the same key closes it again"
        );
        // The pipeline is what the rail exists for; opening the task never
        // costs it.
        assert!(expanded.contains("PIPELINE"));
        assert!(expanded.contains("Implementation"));
    }

    /// A task that already fits is not something to expand, so the key is not
    /// advertised and pressing it changes nothing.
    #[test]
    fn a_task_that_fits_is_never_offered_an_expansion() {
        let mut state = running_state();
        let collapsed = render_text(&state, 160, 40);
        assert!(!collapsed.contains("Full task"));
        state.task_expanded = true;
        assert_eq!(
            render_text(&state, 160, 40),
            collapsed,
            "the toggle is inert for a task the rail already shows whole"
        );
    }

    #[test]
    fn header_carries_product_identity_and_run_state() {
        let text = render_text(&running_state(), 160, 40);
        assert!(text.contains("THE SENATE"));
        assert!(text.contains("MISSION DECK"));
        assert!(text.contains("wp-calypso-2"), "concise repository identity");
        assert!(
            !text.contains("/Users/e/Code/wp-calypso-2"),
            "the full path stays in technical mode"
        );
        // The run id is the handle every CLI command takes, so entering a run
        // shows it without hunting for technical mode.
        assert!(
            text.contains(&RunId::from_u128(3).to_string()),
            "header names the run id"
        );
        // Narrow terminals keep the state and drop the identity.
        let narrow = render_text(&running_state(), 70, 24);
        assert!(narrow.contains("THE SENATE"));
        assert!(!narrow.contains("wp-calypso-2"));
    }

    #[test]
    fn help_keeps_campaign_keys_visible_on_small_terminals() {
        let mut state = running_state();
        state.overlay = Some(Overlay::Help);
        let text = render_text(&state, 80, 24);
        for key in ["I bring in", "A approve", "W Senate", "Artifact"] {
            assert!(text.contains(key), "missing {key}: {text}");
        }
    }

    #[test]
    fn runs_screen_renders_summary_and_needs_user_prominently() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        let id = RunId::from_u128(1);
        state.replace_runs(vec![RunListItem {
            id,
            workflow: WorkflowKind::Standard,
            status: RunStatus::NeedsUser,
            task_summary: "OAuth provider".to_owned(),
            repository: Some(std::path::PathBuf::from("/repo")),
            updated_at: at(12, 0, 0),
            archived: false,
        }]);
        state.details = Some(details(RunStatus::NeedsUser, Vec::new()));
        let text = render_text(&state, 120, 30);
        assert!(text.contains("OAuth provider"));
        assert!(text.contains("NEEDS YOU"));
        assert!(text.contains("▸ "), "the selected run carries a cursor");
        state.overlay = Some(Overlay::Help);
        assert!(render_text(&state, 120, 30).contains("Help · Esc closes"));
    }

    /// A cursor past the last row that fits must scroll the list, not walk off
    /// the bottom of it onto a run nobody can see.
    #[test]
    fn the_selected_run_stays_on_screen_in_a_list_taller_than_the_terminal() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.replace_runs(
            (1..=40)
                .map(|index| RunListItem {
                    id: RunId::from_u128(index),
                    workflow: WorkflowKind::Standard,
                    status: RunStatus::Completed,
                    task_summary: format!("task number {index:02}"),
                    repository: Some(std::path::PathBuf::from("/repo")),
                    updated_at: at(12, 0, 0),
                    archived: false,
                })
                .collect(),
        );
        state.selected_run_index = 39;
        state.selected_run = Some(RunId::from_u128(40));
        let text = render_text(&state, 120, 24);
        assert!(text.contains("task number 40"), "{text}");
        assert!(
            !text.contains("task number 01"),
            "scrolled past the top: {text}"
        );
    }

    /// Archiving has four visible surfaces: the footer offers the keys, a run
    /// shown by the all-runs view carries an "archived" mark, a list where
    /// everything is archived says so instead of claiming there are no runs,
    /// and delete is offered on an archived run alone.
    #[test]
    fn archived_runs_are_advertised_marked_counted_and_deletable() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.replace_runs(vec![RunListItem {
            id: RunId::from_u128(7),
            workflow: WorkflowKind::Standard,
            status: RunStatus::Completed,
            task_summary: "Ship the widget".to_owned(),
            repository: None,
            updated_at: at(12, 0, 0),
            archived: false,
        }]);
        state.details = Some(details(RunStatus::Completed, Vec::new()));
        let text = render_text(&state, 120, 30);
        assert!(text.contains(" h  Archive"), "footer offers archiving");
        assert!(!text.contains("Show archived"), "nothing is archived yet");
        assert!(
            !text.contains("Delete forever"),
            "delete belongs to archived runs alone"
        );

        // Something archived exists: the footer says how to see it.
        state.archived_count = 1;
        assert!(render_text(&state, 120, 30).contains(" H  Show archived"));

        // The all-runs view marks the archived run, offers to bring it back,
        // and only now offers to delete it.
        state.show_archived = true;
        state.runs[0].archived = true;
        let showing = render_text(&state, 120, 30);
        assert!(
            showing.contains("archived  "),
            "archived run carries its mark"
        );
        assert!(showing.contains(" h  Unarchive"));
        assert!(showing.contains(" H  Hide archived"));
        assert!(showing.contains(" D  Delete forever"));

        // Everything archived: the empty state tells the truth.
        state.show_archived = false;
        state.replace_runs(Vec::new());
        state.archived_count = 2;
        let empty = render_text(&state, 120, 30);
        assert!(empty.contains("2 archived runs."), "{empty}");
        assert!(empty.contains("Show archived runs"));
        assert!(!empty.contains("No runs yet"));
    }

    /// Publish no longer refuses a run whose checks failed, so the
    /// confirmation is the only place the operator learns they are pushing
    /// past a red verification. It names the stage and says where the change
    /// is going — a branch, not their checkout.
    #[test]
    fn the_publish_confirmation_warns_when_verification_did_not_pass() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        let verify = stage(
            "verify_2",
            StageKind::Verify,
            Role::Verifier,
            StageStatus::Failed,
        );
        state.details = Some(details(RunStatus::Completed, vec![verify]));
        state.overlay = Some(Overlay::PublishConfirm);

        let text = render_text(&state, 120, 34);

        assert!(
            text.contains("Verification did not pass: stage verify_2 is failed."),
            "{text}"
        );
        assert!(text.contains("not in your checkout"), "{text}");

        // A run whose verification passed says nothing about it.
        let verify = stage(
            "verify_2",
            StageKind::Verify,
            Role::Verifier,
            StageStatus::Completed,
        );
        state.details = Some(details(RunStatus::Completed, vec![verify]));
        let text = render_text(&state, 120, 34);
        assert!(!text.contains("Verification did not pass"), "{text}");
    }

    /// The last stop before a run is gone: POD stands over the plunger, the
    /// overlay names what is about to be destroyed, and the key that goes
    /// through is the same `D` that opened it — never Enter, which every
    /// other confirmation takes.
    #[test]
    fn the_delete_overlay_names_the_damage_and_asks_for_its_own_key() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.replace_runs(vec![RunListItem {
            id: RunId::from_u128(7),
            workflow: WorkflowKind::Standard,
            status: RunStatus::Completed,
            task_summary: "Ship the widget".to_owned(),
            repository: None,
            updated_at: at(12, 0, 0),
            archived: true,
        }]);
        state.details = Some(details(RunStatus::Completed, Vec::new()));
        state.overlay = Some(Overlay::DeleteConfirm);
        let text = render_text(&state, 120, 34);
        assert!(text.contains("DELETE FOREVER"));
        assert!(text.contains("Ship the widget"), "names the run");
        assert!(
            text.contains("worktree") && text.contains("artifacts"),
            "says what is destroyed: {text}"
        );
        assert!(text.contains("D deletes"), "asks for its own key: {text}");
        assert!(text.contains(POD_SHELL), "POD works the plunger");
    }

    #[test]
    fn pipeline_shows_human_stages_with_durations_and_no_fake_zero() {
        let text = render_text(&running_state(), 160, 40);
        assert!(text.contains("PIPELINE"));
        assert!(text.contains("Architecture"), "human stage name");
        assert!(text.contains("Quality review"), "human stage name");
        assert!(
            text.contains("2m 14s"),
            "completed stage shows its duration"
        );
        assert!(
            !text.contains("0s"),
            "a pending stage never shows a fabricated duration"
        );
    }

    #[test]
    fn selection_and_execution_are_distinct_in_the_rail() {
        let mut state = running_state();
        // Reading a finished stage while another one runs.
        state.selected_stage_index = 0;
        state.selected_stage = Some(StageId::new("architecture").unwrap());
        let text = render_text(&state, 160, 40);
        let cursor = text.find("▸ ").expect("the cursor marks the selection");
        let architecture = text.find("Architecture").unwrap();
        let implementation = text.find("Implementation").unwrap();
        assert!(
            cursor < architecture && cursor < implementation,
            "the cursor sits on the selected row, not the running one"
        );
        assert_eq!(
            text.matches("▸ ").count(),
            1,
            "exactly one row is selected at a time"
        );
        // The running stage keeps its own glyph regardless of the cursor.
        assert!(text.contains("● "), "execution state stays on the live row");
        assert!(
            text.contains("COMPLETED"),
            "the hero follows the selection, not the running stage"
        );
    }

    #[test]
    fn running_hero_leads_with_state_clock_runtime_and_activity() {
        let text = render_text(&running_state(), 160, 40);
        assert!(text.contains("IMPLEMENTATION"), "stage names the panel");
        assert!(text.contains("RUNNING"));
        assert!(text.contains("Agent is working…"));
        assert!(text.contains("codex"), "runtime summary is present");
        assert!(text.contains("native default"));
        assert!(
            !text.contains("Kind         "),
            "operational view drops technical field rows"
        );
    }

    #[test]
    fn operational_view_hides_diagnostics_and_technical_view_groups_them() {
        let mut state = running_state();
        let operational = render_text(&state, 160, 40);
        assert!(!operational.contains("native-session-id"), "no native ids");
        assert!(!operational.contains("session-record"));
        assert!(
            !operational.contains("/Users/e/Code/wp-calypso-2"),
            "operational view keeps the full path out"
        );
        assert!(operational.contains(" i  Details"));

        state.technical = true;
        let technical = render_text(&state, 160, 40);
        assert!(technical.contains("TECHNICAL"), "mode is labelled");
        for group in [
            "EXECUTION",
            "RUNTIME",
            "RESOURCE EVIDENCE",
            "WORKSPACE",
            "ROUTING",
        ] {
            assert!(technical.contains(group), "{group} group is present");
        }
        assert!(technical.contains("native-session-id".get(..8).unwrap()));
        assert!(technical.contains("/Users/e/Code/wp-calypso-2"));
        assert!(technical.contains("recommended_v2"));
        assert!(technical.contains("recommended_role_assignment"));
        assert!(technical.contains("abc1234"), "base commit stays available");
        assert!(technical.contains(" i  operational view"));
        assert!(
            !technical.contains(POD_SHELL),
            "technical mode spends POD's rows on diagnostics"
        );
    }

    #[test]
    fn runtime_summary_reports_mismatch_only_when_targets_disagree() {
        let mut aligned = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Running,
        );
        aligned.actual_model = None;
        assert!(!runtime_summary(&aligned).contains("configured →"));

        // A confirmed model where the route asked for the native default is
        // confirmation, not drift.
        let mut confirmed = aligned.clone();
        confirmed.actual_model = Some("gpt-5.4-codex".to_owned());
        let summary = runtime_summary(&confirmed);
        assert!(
            !summary.contains("configured →"),
            "confirmation is not drift"
        );
        assert!(summary.contains("gpt-5.4-codex"), "the real model is named");

        let mut drifted = aligned.clone();
        drifted.configured_model = Some("sonnet".to_owned());
        drifted.actual_provider = Some("claude".to_owned());
        drifted.actual_model = Some("opus".to_owned());
        let summary = runtime_summary(&drifted);
        assert!(summary.contains("configured →"), "drift is surfaced");
        assert!(summary.contains("opus"));
    }

    /// The two runtimes do not report the same quantity under the name
    /// "input": Claude's excludes what its cache served, Codex's includes it.
    /// So there is one line per runtime, no total across them, and a cached
    /// token is never printed twice on the runtime that already folded it in.
    #[test]
    fn resources_never_sum_two_runtimes_or_count_a_cached_token_twice() {
        let lines = resource_lines(&details(RunStatus::Running, Vec::new()));
        assert_eq!(lines.len(), 2, "one line per reporting runtime");
        let claude = lines
            .iter()
            .find(|line| line.starts_with("claude"))
            .expect("claude line");
        let codex = lines
            .iter()
            .find(|line| line.starts_with("codex"))
            .expect("codex line");

        // Claude keeps input and cache read disjoint, so both are quantities
        // and both are listed.
        assert!(claude.contains("128 input"), "{claude}");
        assert!(claude.contains("4.3M cache read"), "{claude}");

        // Codex folds cache reads into its input total. Naming it again as a
        // separate dimension would show the same 8.9M tokens twice.
        assert!(codex.contains("9.2M input"), "{codex}");
        assert!(codex.contains("8.8M of it cached"), "{codex}");
        assert!(
            !codex.contains("cache read"),
            "cached input is already inside the input total: {codex}"
        );
        // 8.9M cached tokens must not also appear as their own dimension.
        assert_eq!(
            codex.matches("8.8M").count(),
            1,
            "a cached token is named once: {codex}"
        );

        // 128 + 9_246_322 is not a quantity of anything, so no line may
        // speak for both runtimes at once.
        for line in &lines {
            assert!(
                !(line.contains("claude") && line.contains("codex")),
                "runtimes are never merged into one figure: {line}"
            );
        }
        assert!(
            !lines.iter().any(|line| line.contains('$')),
            "usage never implies cost"
        );
    }

    /// The one input figure that means the same thing on both runtimes.
    #[test]
    fn uncached_input_is_derived_only_where_the_runtime_declared_how_it_counts() {
        let usage = details(RunStatus::Running, Vec::new()).usage;
        let by_provider = usage.providers().collect::<Vec<_>>();
        let claude = &by_provider[0];
        let codex = &by_provider[1];
        assert_eq!(claude.uncached_input_units(), Some(128));
        assert_eq!(codex.uncached_input_units(), Some(9_246_322 - 8_885_760));

        // An unrecognised runtime declared no convention, so nothing is
        // derived on its behalf.
        let unknown = crate::app::RunUsage::from_totals([(
            "gemini".to_owned(),
            UsageSummary {
                input_units: 4_000,
                output_units: 10,
                cache_read_units: Some(3_000),
                cache_write_units: None,
                reasoning_output_units: None,
            },
        )]);
        let entry = unknown.providers().next().expect("one entry");
        assert_eq!(entry.uncached_input_units(), None);
        assert!(!entry.input_contains_cache_reads());
    }

    #[test]
    fn needs_user_dominates_the_hero_and_precedes_everything_secondary() {
        let mut state = running_state();
        let details = state.details.as_mut().unwrap();
        details.status = RunStatus::NeedsUser;
        details.stages[1].status = StageStatus::NeedsUser;
        details.attention = vec![crate::app::AttentionSummary {
            id: crate::domain::AttentionRequestId::from_u128(1),
            stage_id: StageId::new("implementation").unwrap(),
            kind: AttentionKind::Permission,
            summary: "Claude requests permission to use Bash".to_owned(),
        }];
        let text = render_text(&state, 160, 40);
        assert!(text.contains("ACTION REQUIRED"));
        assert!(text.contains("Claude requests permission to use Bash"));
        assert!(text.contains(" u  Review and resolve"));
        let action = text.find("ACTION REQUIRED").unwrap();
        for secondary in ["RESOURCES", "RESULT", "codex · native default"] {
            assert!(
                action < text.find(secondary).unwrap(),
                "attention outranks {secondary}"
            );
        }
        assert!(
            actions_text(&state).starts_with(" u  Resolve attention"),
            "the footer leads with the attention shortcut"
        );
    }

    #[test]
    fn completed_run_pivots_to_review_and_drops_monitoring_language() {
        let text = render_text(&completed_state(), 160, 40);
        assert!(text.contains("RUN COMPLETE"));
        assert!(text.contains("12:48"), "run elapsed is prominent");
        assert!(text.contains("READY TO REVIEW"));
        assert!(text.contains(" a  Apply changes"));
        assert!(text.contains(" X  Discard"));
        assert!(
            !text.contains("Agent is working"),
            "monitoring language is gone after completion"
        );
        assert!(
            !text.contains("Stage finished"),
            "the run, not the stage, speaks after the pivot"
        );
    }

    #[test]
    fn failed_state_is_legible_and_offers_recovery_once() {
        let mut state = running_state();
        let details = state.details.as_mut().unwrap();
        details.status = RunStatus::Failed;
        details.stages[1].status = StageStatus::Failed;
        details.stages[1].finished_at = Some(at(12, 3, 0));
        let text = render_text(&state, 160, 40);
        assert!(text.contains("✗ FAILED"));
        assert!(text.contains("No completed result"));
        assert!(text.contains(" t  Retry"));
        assert!(text.contains(" l  Logs"));
        assert_eq!(
            text.matches(" t  Retry").count(),
            2,
            "hero and footer each offer retry exactly once"
        );
        assert!(
            !text.contains("provider exited"),
            "raw provider metadata stays out of the operational view"
        );
    }

    /// The failure reason folded onto a stage is the one place the operator
    /// learns *why* without ever leaving the panel: the strip's sentence and
    /// the hero's activity line both show it in place of the generic text.
    #[test]
    fn a_failed_stage_shows_its_reason_instead_of_generic_text() {
        let mut state = running_state();
        let details = state.details.as_mut().unwrap();
        details.status = RunStatus::Failed;
        details.stages[1].status = StageStatus::Failed;
        details.stages[1].finished_at = Some(at(12, 3, 0));
        details.stages[1].failure_reason = Some("compile failed: missing semicolon".to_owned());
        details.stages[1].blocking = true;

        let sentences = status_sentences(state.details.as_ref().unwrap(), at(12, 13, 0), 160);
        assert_eq!(
            sentences[0],
            "Implementation failed: compile failed: missing semicolon"
        );

        let text = render_text(&state, 160, 40);
        assert!(text.contains("compile failed: missing semicolon"));
        assert!(
            !text.contains("The provider ended this stage before it completed"),
            "the reason replaces the generic activity line, not adds to it"
        );
    }

    /// A stage that failed without the runtime reporting why keeps the
    /// generic activity text — the reason is a bonus, never a requirement.
    #[test]
    fn a_failed_stage_without_a_reason_keeps_the_generic_text() {
        let mut state = running_state();
        let details = state.details.as_mut().unwrap();
        details.status = RunStatus::Failed;
        details.stages[1].status = StageStatus::Failed;
        details.stages[1].finished_at = Some(at(12, 3, 0));
        let text = render_text(&state, 160, 40);
        assert!(text.contains("The provider ended this stage before it completed"));
    }

    /// The strip is a fixed [`STATUS_HEIGHT`] rows. A reason near the 200
    /// character cap must be cut to the strip's own width rather than wrap
    /// and push the stage-count sentence out of the box.
    #[test]
    fn status_strip_truncates_a_long_reason_to_fit_the_available_width() {
        let mut failing = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Failed,
        );
        failing.failure_reason = Some("x".repeat(200));
        failing.blocking = true;
        let run = details(RunStatus::Failed, vec![failing]);

        let width = 40u16;
        let sentences = status_sentences(&run, at(12, 13, 0), width);
        assert!(
            sentences[0].chars().count() <= width as usize,
            "the reason line must fit the strip's fixed width: {:?}",
            sentences[0]
        );
        assert!(sentences[0].ends_with('…'), "the cut is visible");
        assert_eq!(
            sentences[1], "0 of 1 stages complete, 1 failed.",
            "the stage-count sentence survives a long reason instead of being pushed out"
        );
    }

    /// At the 50-column supported minimum the rail's inner width is only
    /// about 16 columns — narrower than truncating just the reason ever
    /// accounted for, since the untruncated `"<Stage> failed: "` prefix could
    /// still push the composed line past `width`. With a short stage title
    /// the prefix itself still fits, so the fix (bounding the whole composed
    /// sentence) must truncate successfully rather than fall back, and the
    /// result must never exceed the strip's width.
    #[test]
    fn status_strip_bounds_the_whole_composed_sentence_at_a_16_column_width() {
        let mut failing = stage(
            "fix",
            StageKind::Fix,
            Role::Implementer,
            StageStatus::Failed,
        );
        failing.failure_reason = Some("x".repeat(200));
        failing.blocking = true;
        let run = details(RunStatus::Failed, vec![failing]);

        let width = 16u16;
        let sentences = status_sentences(&run, at(12, 13, 0), width);
        assert!(
            sentences[0].chars().count() <= width as usize,
            "the composed sentence must fit the strip's width: {:?}",
            sentences[0]
        );
        assert!(
            sentences[0].starts_with("Fix failed: "),
            "the prefix must survive intact, not be cut mid-word: {:?}",
            sentences[0]
        );
        assert!(sentences[0].ends_with('…'), "the cut is visible");
        assert_eq!(
            sentences[1], "0 of 1 stages complete, 1 failed.",
            "the stage-count sentence survives a bounded composed sentence"
        );
    }

    /// The real bug this guards: `"Implementation failed: "` alone is 23
    /// characters, already wider than the ~16-column rail at the smallest
    /// supported terminal. No truncation of that prefix can produce anything
    /// legible, so rather than land mid-word the strip drops the reason
    /// entirely and falls back to the reasonless generic sentence — the same
    /// text shown when a failed stage carries no reason at all. This
    /// necessarily still exceeds 16 columns (an unavoidable consequence of
    /// naming a 14-character stage at that width, present before failure
    /// reasons existed), but the stage-count sentence must still be exactly
    /// right — proving the fallback never corrupts anything past it.
    #[test]
    fn status_strip_falls_back_to_the_generic_sentence_when_even_the_prefix_cannot_fit() {
        let mut failing = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Failed,
        );
        failing.failure_reason = Some("x".repeat(200));
        failing.blocking = true;
        let run = details(RunStatus::Failed, vec![failing]);

        let sentences = status_sentences(&run, at(12, 13, 0), 16);
        assert_eq!(
            sentences[0], "Implementation failed — its logs say why.",
            "an unfittable prefix falls back cleanly instead of a garbled truncation"
        );
        assert_eq!(
            sentences[1], "0 of 1 stages complete, 1 failed.",
            "the stage-count sentence survives even the fallback path"
        );
    }

    /// The hero shows the whole reason, not a one-line cut of it: reading
    /// the message is why anyone looks at a failed stage. A reason at the
    /// 200-character cap is longer than the panel, so it must wrap onto
    /// further rows and still be fully present in the rendered text.
    #[test]
    fn hero_shows_the_whole_failure_reason_wrapped_under_its_own_heading() {
        let mut state = running_state();
        let details = state.details.as_mut().unwrap();
        details.status = RunStatus::Failed;
        let reason = "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 4:05 PM.";
        details.stages[1].status = StageStatus::Failed;
        details.stages[1].failure_reason = Some(reason.to_owned());
        details.stages[1].blocking = true;
        state.selected_stage_index = 1;
        let text = render_text(&state, 120, 40);
        assert!(text.contains("WHY IT FAILED"), "{text}");
        let joined: String = text
            .lines()
            .map(|line| line.trim_start_matches(['│', ' ']).trim_end())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("try again at 4:05 PM."),
            "the tail of the reason survives instead of an ellipsis:\n{text}"
        );
        assert!(!text.contains("usage limit…"), "no one-line cut:\n{text}");
    }

    /// A failed stage with no reason, or any non-failed status, falls back
    /// to the generic typed activity text rather than an empty heading.
    #[test]
    fn hero_falls_back_to_the_generic_message_without_a_reason() {
        let failing = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Failed,
        );
        assert_eq!(failed_stage_reason(&failing), None);
        assert_eq!(
            activity_message(&failing),
            Some("The provider ended this stage before it completed".to_owned())
        );
        let running = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Running,
        );
        assert_eq!(failed_stage_reason(&running), None);
        assert_eq!(
            activity_message(&running),
            Some("Agent is working…".to_owned())
        );
    }

    /// A failed stage's reason outranks stale waiting info on the same
    /// stage; a Pending stage with no failure keeps its waiting text.
    #[test]
    fn failure_reason_outranks_waiting_info_by_status() {
        let mut failing = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Failed,
        );
        failing.failure_reason = Some("compile failed".to_owned());
        failing.waiting = Some(StageWaitingSummary {
            waiting_on: vec![StageDependencyRef {
                id: StageId::new("architecture").unwrap(),
                kind: StageKind::Architecture,
            }],
            blocked_by: Vec::new(),
            degraded: Vec::new(),
        });
        assert_eq!(failed_stage_reason(&failing), Some("compile failed"));

        let mut pending = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Pending,
        );
        pending.failure_reason = Some("stale".to_owned());
        pending.waiting = Some(StageWaitingSummary {
            waiting_on: vec![StageDependencyRef {
                id: StageId::new("architecture").unwrap(),
                kind: StageKind::Architecture,
            }],
            blocked_by: Vec::new(),
            degraded: Vec::new(),
        });
        assert_eq!(failed_stage_reason(&pending), None);
        assert_eq!(
            activity_message(&pending),
            Some("Waiting on: Architecture".to_owned())
        );
    }

    /// The Runs screen's overview names the reason before the run is even
    /// opened, and only for a failed run with a blocking reason.
    #[test]
    fn runs_overview_shows_the_blocking_failure_reason() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.replace_runs(vec![RunListItem {
            id: RunId::from_u128(1),
            workflow: WorkflowKind::Standard,
            status: RunStatus::Failed,
            task_summary: "OAuth provider".to_owned(),
            repository: Some(std::path::PathBuf::from("/repo")),
            updated_at: at(12, 0, 0),
            archived: false,
        }]);
        let mut failed = details(RunStatus::Failed, Vec::new());
        failed.failure_reason = Some("You've hit your usage limit.".to_owned());
        state.details = Some(failed);
        let text = render_text(&state, 160, 40);
        assert!(text.contains("WHY IT FAILED"), "{text}");
        assert!(text.contains("You've hit your usage limit."), "{text}");

        state.details = Some(details(RunStatus::Failed, Vec::new()));
        let text = render_text(&state, 160, 40);
        assert!(
            !text.contains("WHY IT FAILED"),
            "no heading without a reason:\n{text}"
        );
    }

    /// Opening a failed run lands on the stage that failed it, so the hero
    /// opens on the reason; a run that is not failed keeps its selection,
    /// and a failed stage that did not block the run is not chosen.
    #[test]
    fn focus_blocking_failure_selects_the_blocking_failed_stage_only() {
        let mut state = running_state();
        state.selected_stage_index = 0;
        state.selected_stage = state
            .details
            .as_ref()
            .map(|details| details.stages[0].id.clone());
        state.focus_blocking_failure();
        assert_eq!(state.selected_stage_index, 0, "a running run is untouched");

        let details = state.details.as_mut().unwrap();
        details.status = RunStatus::Failed;
        details.stages[1].status = StageStatus::Failed;
        details.stages[1].blocking = false;
        state.focus_blocking_failure();
        assert_eq!(
            state.selected_stage_index, 0,
            "a non-blocking failure is not the reason the run failed"
        );

        state.details.as_mut().unwrap().stages[1].blocking = true;
        state.focus_blocking_failure();
        assert_eq!(state.selected_stage_index, 1);
        assert_eq!(
            state.selected_stage.as_ref().map(ToString::to_string),
            Some(state.details.as_ref().unwrap().stages[1].id.to_string())
        );
    }

    #[test]
    fn footer_advertises_apply_only_when_the_run_is_applyable() {
        let running = running_state();
        let actions = actions_text(&running);
        assert!(!actions.contains(" a  Apply"), "no apply while running");
        assert!(actions.contains(" o  Result"));

        // A finished run's apply lives on the panel's own row; the footer
        // keeps only what that row leaves out.
        let completed = completed_state();
        let actions = actions_text(&completed);
        assert!(!actions.contains(" a  Apply"));
        assert!(actions.contains(" X  Discard"));
        assert!(render_text(&completed, 160, 40).contains(" a  Apply changes"));

        // Narrow terminals drop navigation, never the contextual actions.
        let wide: String = footer_line(Screen::RunDetail, &running, 200)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(wide.contains("Esc runs"));
        let narrow: String = footer_line(Screen::RunDetail, &running, 46)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(narrow.contains(" o  Result"), "actions survive");
        assert!(!narrow.contains("? help"), "navigation yields first");
    }

    /// Fix, continue and Follow-ups share one key: a run that can take any
    /// of them offers `[f]` Next cycle in both the footer and the hero panel,
    /// and never lists `[c]` or `[w]` beside it.
    #[test]
    fn every_cycle_is_offered_behind_the_one_next_cycle_key() {
        let mut continuable = completed_with_failure_details();
        continuable.routes.push(RouteSummary {
            role: Role::EngineeringLead,
            configured_provider: "codex".to_owned(),
            configured_model: None,
            reason: "test".to_owned(),
            requested_effort: Some(EffortSetting::NativeDefault),
        });
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.screen = Screen::RunDetail;
        state.selected_run = Some(RunId::from_u128(3));
        state.replace_details(continuable);

        let actions = actions_text(&state);
        let text = render_text(&state, 160, 40);
        assert!(text.contains(" f  Next cycle…"));
        for folded in [" c  Continue", " w  Follow-ups", " b  Rebase"] {
            assert!(!actions.contains(folded), "footer lists {folded}");
            assert!(!text.contains(folded), "panel lists {folded}");
        }
        // Details stays in the footer only; the panel's row is for decisions.
        let hero: String = hero_actions(
            true,
            cycle_label(&state),
            &publish_action(state.details.as_ref().unwrap()),
            StageStatus::Completed,
        )
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
        .collect();
        assert!(!hero.contains("Details"));
    }

    /// `[P]` says whether the run is already on its pull request.
    #[test]
    fn push_to_pr_says_whether_the_run_is_already_pushed() {
        let mut details = completed_with_failure_details();
        assert_eq!(publish_action(&details).0, "Push to PR");
        details.publication = Some(crate::app::Publication {
            branch: "senate/run-3".to_owned(),
            pull_request_url: Some("https://github.com/o/r/pull/123".to_owned()),
            outdated: false,
        });
        assert_eq!(publish_action(&details).0, "Pushed ✓ #123");
        details.publication.as_mut().unwrap().outdated = true;
        assert_eq!(publish_action(&details).0, "Update #123 · behind");
        details.publication.as_mut().unwrap().pull_request_url = None;
        assert_eq!(publish_action(&details).0, "Update senate/run-3 · behind");
    }

    #[test]
    fn footer_offers_resume_for_suspended_and_orphaned_runs() {
        // Running with nobody driving it: left behind by a dead instance.
        let orphaned = running_state();
        assert!(actions_text(&orphaned).contains(" r  Resume"));

        // Running with its driver in flight needs no second one.
        let mut driven = running_state();
        driven.begin_action(crate::tui::worker::ActionKind::Resume, driven.selected_run);
        assert!(!actions_text(&driven).contains(" r  Resume"));

        let mut paused = running_state();
        paused.details.as_mut().unwrap().status = RunStatus::Paused;
        assert!(actions_text(&paused).contains(" r  Resume"));
    }

    #[test]
    fn result_section_is_state_aware() {
        let running = running_state();
        let text = render_text(&running, 160, 40);
        assert!(
            text.contains("Not available yet"),
            "informational, not error"
        );
        assert!(!text.contains("✓ Verified artifact available"));

        let mut completed = completed_state();
        completed
            .stages_with_artifacts
            .insert(StageId::new("implementation").unwrap());
        let text = render_text(&completed, 160, 40);
        assert!(text.contains("✓ Implementation ready"));
        assert!(text.contains(" Enter/o  Open result"));
    }

    /// A store the panel cannot read is not a stage that produced nothing.
    #[test]
    fn an_unreadable_artifact_listing_says_so_instead_of_claiming_absence() {
        let mut state = completed_state();
        state.artifacts_unavailable =
            Some("database schema version 10 is newer than this Senate build supports".to_owned());
        let text = render_text(&state, 160, 40);
        assert!(text.contains("Results unavailable — database schema version 10"));
        assert!(!text.contains("No verified artifact"));
    }

    /// The panel quotes the artifact of the stage the operator is looking at,
    /// and of no other. A headline outlives a keypress; the selection it was
    /// read for does not.
    #[test]
    fn the_result_section_quotes_the_selected_stage_and_only_that_one() {
        let mut state = completed_state();
        let implementation = StageId::new("implementation").unwrap();
        state.stages_with_artifacts.insert(implementation.clone());
        state.headline = Some(StageHeadline {
            stage_id: implementation,
            attempt: 1,
            content_size: 512,
            text: Some("It does what the task asked, but the retry path is untested.".to_owned()),
            contracted: true,
        });

        let text = render_text(&state, 160, 40);
        assert!(text.contains("It does what the task asked, but the retry path is untested."));
        assert!(
            text.contains(" Enter/o  Open result"),
            "the quote is a lead-in, not a replacement"
        );

        state.headline = state.headline.take().map(|headline| StageHeadline {
            stage_id: StageId::new("research").unwrap(),
            ..headline
        });
        let text = render_text(&state, 160, 40);
        assert!(!text.contains("It does what the task asked"));
        assert!(
            text.contains("✓ Implementation ready"),
            "a stale quote hides itself without hiding the artifact"
        );
    }

    /// A glance is two rows. An agent that ignored the word limit is cut off
    /// with an ellipsis rather than pushing Resources off the panel.
    #[test]
    fn a_long_opening_line_is_cut_to_two_rows() {
        let mut state = completed_state();
        let implementation = StageId::new("implementation").unwrap();
        state.stages_with_artifacts.insert(implementation.clone());
        state.headline = Some(StageHeadline {
            stage_id: implementation.clone(),
            attempt: 1,
            content_size: 4096,
            text: Some("word ".repeat(400)),
            contracted: false,
        });
        let selected = state.details.as_ref().unwrap().stages[0].clone();

        let lines = headline_lines(&state, &selected, 40);
        let quoted: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert!(quoted.ends_with('…'), "the cut is visible");
        assert!(
            quoted.chars().count() <= 40 * HEADLINE_ROWS + 1,
            "two rows at most: {}",
            quoted.chars().count()
        );
    }

    #[test]
    fn notification_never_hides_run_detail_navigation_hints() {
        let mut state = running_state();
        state.set_error("resuming run failed for run-1: boom");
        let text = render_text(&state, 160, 40);
        assert!(text.contains("boom"), "message is visible");
        assert!(text.contains("Esc runs"), "leave-screen hint stays visible");
        assert!(text.contains("x dismiss"), "dismiss affordance advertised");
    }

    /// A refusal that names the command it refused is longer than one row.
    /// Clipping it at the terminal edge hides exactly the part the operator
    /// needs, so the footer grows instead.
    #[test]
    fn a_long_notification_wraps_instead_of_being_cut_off() {
        let mut state = running_state();
        state.set_error(
            "resolving attention failed for run-1: Claude Code permission cannot be resumed \
             safely: Bash command cannot be granted as an exact rule (shell this parser will \
             not read: command substitution, subshell, or heredoc); type a response to \
             continue without granting it, or stop the run: yarn install \"$(cat .yarnrc)\"",
        );
        let text = render_text(&state, 100, 40);
        assert!(
            text.contains("yarn install \"$(cat .yarnrc)\""),
            "the refused command survives to the screen: {text}"
        );
        assert!(text.contains("x dismiss"), "dismiss affordance advertised");
        assert!(text.contains("Esc runs"), "key hints keep their row");
    }

    /// The footer is a footer: a runaway provider string may not eat the run.
    #[test]
    fn a_runaway_notification_is_capped_and_marked_as_cut() {
        let rows = message_rows(&"word ".repeat(500), 100);
        assert_eq!(rows.len(), 4);
        assert!(rows.last().unwrap().ends_with('\u{2026}'));
        for row in &rows {
            assert!(row.chars().count() <= 100, "{row}");
        }
    }

    /// Standing permission is visible from the run's own screen: the chip
    /// says the run is armed, and the footer offers the way back.
    #[test]
    fn an_armed_run_says_so_in_the_header_and_offers_the_way_back() {
        let mut state = running_state();
        let asking = state.details.as_mut().unwrap();
        asking.auto_approve = true;
        asking.attention = vec![crate::app::AttentionSummary {
            id: crate::domain::AttentionRequestId::from_u128(1),
            stage_id: crate::domain::StageId::new("implementation").unwrap(),
            kind: AttentionKind::Permission,
            summary: "Claude Code requests permission for: Bash cargo test".to_owned(),
        }];
        let armed = render_text(&state, 160, 40);
        assert!(
            armed.contains("AUTO-APPROVE"),
            "the chip is visible: {armed}"
        );
        assert!(
            armed.contains("Ask me again"),
            "disarming is offered: {armed}"
        );

        state.details.as_mut().unwrap().auto_approve = false;
        let unarmed = render_text(&state, 160, 40);
        assert!(!unarmed.contains("AUTO-APPROVE"), "{unarmed}");
        assert!(
            unarmed.contains("Auto-approve"),
            "arming is offered: {unarmed}"
        );
    }

    #[test]
    fn message_kinds_render_distinct_styles() {
        let styles: Vec<_> = [
            UiMessageKind::Info,
            UiMessageKind::Success,
            UiMessageKind::Warning,
            UiMessageKind::Error,
        ]
        .into_iter()
        .map(message_presentation)
        .collect();
        for (index, (glyph, style)) in styles.iter().enumerate() {
            for (other_glyph, other_style) in styles.iter().skip(index + 1) {
                assert!(glyph != other_glyph || style != other_style);
            }
        }
    }

    /// Viewer content is external: a diff carries repository source, logs
    /// carry provider stdout. A control character written into a cell measures
    /// zero columns for Ratatui but still moves the terminal cursor, so the
    /// two disagree and the diff-based repaint can no longer erase what it
    /// drew — content survives on screen after the viewer is closed. The
    /// invariant is therefore checked at the buffer, where it is deterministic:
    /// nothing a viewer renders may be a control character.
    #[test]
    fn viewers_never_write_control_characters_into_the_buffer() {
        const HOSTILE: &str =
            "fn main() {\n\tlet x = 1;\r\n\u{1b}[31mred\u{1b}[0m\n\u{0}nul\u{7}bell\n";

        fn control_cells(text: &str) -> Vec<char> {
            text.chars()
                .filter(|character| {
                    character.is_control()
                        || matches!(character, '\u{7f}'..='\u{9f}' | '\u{200b}'..='\u{200f}')
                })
                .collect()
        }

        let mut state = completed_state();

        state.screen = Screen::Diff;
        state.diff = Some(crate::app::RunDiffPreview {
            text: format!("diff --git a/x b/x\n@@ -1 +1 @@\n+{HOSTILE}"),
            changed_files: vec![crate::app::ChangedFileSummary {
                path: "x".to_owned(),
                binary: false,
            }],
            total_bytes: 64,
            truncated: false,
        });
        let diff = render_text(&state, 120, 30);
        assert!(
            control_cells(&diff).is_empty(),
            "diff viewer leaked control characters: {:?}",
            control_cells(&diff)
        );
        assert!(diff.contains("let x = 1;"), "content still renders: {diff}");

        state.screen = Screen::Logs;
        state.logs = Some(crate::app::ProcessLogView {
            process_id: crate::process::ManagedProcessId::new(),
            process_status: "running".to_owned(),
            stdout: crate::app::ProcessLogStream {
                text: HOSTILE.to_owned(),
                total_bytes: 32,
                truncated: false,
            },
            stderr: crate::app::ProcessLogStream {
                text: String::new(),
                total_bytes: 0,
                truncated: false,
            },
        });
        let logs = render_text(&state, 120, 30);
        assert!(
            control_cells(&logs).is_empty(),
            "log viewer leaked control characters: {:?}",
            control_cells(&logs)
        );

        state.screen = Screen::Artifact;
        state.artifact = Some(crate::app::ArtifactView {
            summary: crate::app::ArtifactSummary {
                stage_id: StageId::new("implementation").unwrap(),
                kind: crate::domain::ArtifactKind::Implementation,
                status: crate::domain::ArtifactStatus::Complete,
                attempt: 1,
                provider: None,
                model: None,
                content_size: 10,
                created_at: at(12, 0, 0),
            },
            text: HOSTILE.to_owned(),
        });
        let rendered = render_text(&state, 120, 30);
        assert!(
            control_cells(&rendered).is_empty(),
            "artifact viewer leaked control characters: {:?}",
            control_cells(&rendered)
        );
        state.artifact_raw = true;
        let raw = render_text(&state, 120, 30);
        assert!(
            control_cells(&raw).is_empty(),
            "raw artifact viewer leaked control characters: {:?}",
            control_cells(&raw)
        );

        // The `[w]` overlay copies lines extracted from an agent-authored
        // decision artifact, not a viewer reading a file — the same
        // untrusted-content risk under a different entry point.
        state.artifact_raw = false;
        state.screen = Screen::RunDetail;
        state.overlay = Some(Overlay::FollowUps);
        state.follow_ups_text = Some(HOSTILE.to_owned());
        let follow_ups = render_text(&state, 120, 30);
        assert!(
            control_cells(&follow_ups).is_empty(),
            "follow-ups overlay leaked control characters: {:?}",
            control_cells(&follow_ups)
        );
        assert!(
            follow_ups.contains("let x = 1;"),
            "content still renders: {follow_ups}"
        );
    }

    #[test]
    fn artifact_viewer_renders_markdown_and_advertises_back() {
        let mut state = completed_state();
        state.screen = Screen::Artifact;
        state.artifact = Some(crate::app::ArtifactView {
            summary: crate::app::ArtifactSummary {
                stage_id: StageId::new("implementation").unwrap(),
                kind: crate::domain::ArtifactKind::Implementation,
                status: crate::domain::ArtifactStatus::Complete,
                attempt: 1,
                provider: None,
                model: None,
                content_size: 10,
                created_at: at(12, 0, 0),
            },
            text: "## Result\n\n**done** with `code`".to_owned(),
        });
        let text = render_text(&state, 120, 30);
        assert!(text.contains("Esc run detail"), "viewer advertises back");
        assert!(text.contains("IMPLEMENTATION"), "viewer names its stage");
        assert!(text.contains("Result"), "heading text renders");
        assert!(!text.contains("## Result"), "no literal markdown heading");
        assert!(!text.contains("**done**"), "no literal bold markers");

        state.artifact_raw = true;
        let raw = render_text(&state, 120, 30);
        assert!(
            raw.contains("## Result"),
            "raw mode shows markdown verbatim"
        );
        assert!(raw.contains("**done**"));
    }

    #[test]
    fn new_run_reads_as_a_briefing_with_a_focused_field() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.screen = Screen::NewRun;
        let text = render_text(&state, 120, 30);
        assert!(text.contains("START A RUN"));
        for label in ["TASK", "WORKFLOW", "REPOSITORY", "EXECUTION", "EFFORT"] {
            assert!(text.contains(label), "{label} field is labelled");
        }
        assert!(text.contains(" Enter  Start run"));
        assert!(text.contains("▸ "), "the focused field carries a cursor");
    }

    #[test]
    fn mascot_appears_with_space_and_disappears_when_constrained() {
        let empty = TuiState::new(std::path::Path::new("/repo"));
        assert!(render_text(&empty, 90, 24).contains(POD_SHELL));
        assert!(render_text(&empty, 90, 24).contains("READY"));
        assert!(!render_text(&empty, 55, 12).contains(POD_SHELL));

        let running = running_state();
        let text = render_text(&running, 160, 40);
        assert!(text.contains(POD_SHELL));
        assert!(
            text.contains("BUILDING"),
            "implementer running reads BUILDING"
        );
        assert!(
            text.contains("█ ▄▄▄▄▄▄"),
            "POD's hand is raised beside the builder's laptop"
        );
        assert!(!render_text(&running, 70, 24).contains(POD_SHELL));
    }

    /// POD's seat has room for what POD is working on: the pull request's
    /// title once `gh` has answered, its repository and number until then.
    #[test]
    fn pod_names_the_pull_request_it_works_on() {
        let mut state = running_state();
        let url = "https://github.a8c.com/Automattic/wpcom/pull/241491";
        state.details.as_mut().unwrap().task = Some(format!("Review {url}"));

        let text = render_text(&state, 160, 40);
        assert!(text.contains(POD_SHELL), "{text}");
        assert!(text.contains("wpcom #241491"), "{text}");

        state
            .pull_request_titles
            .insert(url.to_owned(), Some("Fix the checkout race".to_owned()));
        let text = render_text(&state, 160, 40);
        assert!(text.contains("#241491 Fix the checkout race"), "{text}");

        state.details.as_mut().unwrap().task = Some("Add OAuth provider support".to_owned());
        assert!(
            !render_text(&state, 160, 40).contains("#241491"),
            "a task naming no pull request gets no caption"
        );
    }

    /// The palette has to arrive through the accessors, not merely resolve
    /// correctly in isolation. Rendering with Vivid installed paints
    /// specified colours; the default paints none, so the assertion is about
    /// the theme rather than about colour existing at all.
    #[test]
    fn the_vivid_palette_reaches_the_screen_and_the_native_one_leaves_it_to_the_terminal() {
        let state = running_state();
        let specified = |palette| {
            theme::with_palette(palette, || {
                painted_colours(&state, 160, 40)
                    .iter()
                    .copied()
                    .any(theme::is_specified)
            })
        };
        assert!(
            specified(theme::Palette::resolve(
                theme::ColorCapability::TrueColor,
                theme::ThemeChoice::Vivid
            )),
            "vivid resolved but never reached a cell"
        );
        assert!(
            !specified(theme::Palette::resolve(
                theme::ColorCapability::TrueColor,
                theme::ThemeChoice::Native
            )),
            "native painted a colour the terminal theme cannot override"
        );
    }

    /// A reaction says something happened. So it fires when the world moves
    /// under POD — and not when the user simply looks somewhere else, which
    /// also changes the face POD is wearing.
    #[test]
    fn pod_reacts_to_the_world_moving_and_not_to_being_looked_away_from() {
        let stages = |implementation| {
            vec![
                // Deliberately not the same face as the stage below it, so
                // moving the selection genuinely changes what POD shows.
                stage(
                    "architecture",
                    StageKind::Architecture,
                    Role::Architect,
                    StageStatus::Failed,
                ),
                stage(
                    "implementation",
                    StageKind::Implementation,
                    Role::Implementer,
                    implementation,
                ),
            ]
        };

        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.screen = Screen::RunDetail;
        state.selected_stage_index = 1;
        state.replace_details(details(RunStatus::Running, stages(StageStatus::Running)));
        state.settle_reaction(std::time::Instant::now());
        assert!(
            !state.reacting,
            "the first sighting of a run is not something that happened"
        );

        state.replace_details(details(RunStatus::Running, stages(StageStatus::Completed)));
        state.settle_reaction(std::time::Instant::now());
        assert!(state.reacting, "the stage finished and POD did not notice");

        // It ends on its own, without anything else happening.
        state.settle_reaction(std::time::Instant::now() + std::time::Duration::from_secs(1));
        assert!(!state.reacting, "the reaction outlived its window");

        // Looking at a different stage shows a different face, which is not
        // an event. Moved through the real selection path: replace_details
        // restores the index from the selected stage id, so setting the index
        // alone would silently move nothing and prove nothing.
        state.move_stage(false);
        assert_eq!(state.selected_stage_index, 0, "the selection has to move");
        state.replace_details(details(RunStatus::Running, stages(StageStatus::Completed)));
        state.settle_reaction(std::time::Instant::now());
        assert!(
            !state.reacting,
            "POD reacted to the user pressing an arrow key"
        );

        // ... and the face it now wears really is a different one, so the
        // assertion above is about identity rather than about nothing having
        // changed.
        assert_ne!(
            mascot::mascot_state(Some(RunStatus::Running), Some(StageStatus::Failed)),
            mascot::mascot_state(Some(RunStatus::Running), Some(StageStatus::Completed)),
            "the stage moved to has to look different for this to mean anything"
        );

        // Nor to a different run: that is a different thing to watch, not
        // this one changing. Same selected stage name, different run, and a
        // face that genuinely differs from the one POD was wearing.
        let mut elsewhere = details(
            RunStatus::Running,
            vec![stage(
                "architecture",
                StageKind::Architecture,
                Role::Architect,
                StageStatus::Running,
            )],
        );
        elsewhere.id = RunId::from_u128(4);
        state.replace_details(elsewhere);
        state.settle_reaction(std::time::Instant::now());
        assert!(!state.reacting, "POD reacted to a different run entirely");
    }

    /// And the reaction has to reach the screen, inside POD's footprint.
    #[test]
    fn a_reaction_reaches_the_screen_without_moving_anything() {
        const WIDTH: u16 = 160;
        let mut state = running_state();
        let resting = render_symbols(&state, WIDTH, 40);
        state.reacting = true;
        let reacting = render_symbols(&state, WIDTH, 40);

        assert_ne!(resting, reacting, "the reaction never reached a cell");
        let changed: Vec<usize> = resting
            .iter()
            .zip(&reacting)
            .enumerate()
            .filter(|(_, (old, new))| old != new)
            .map(|(index, _)| index)
            .collect();
        assert!(
            changed.len() <= 4,
            "a reaction is four eye cells, not {} cells of the screen",
            changed.len()
        );
        let rows: std::collections::HashSet<usize> =
            changed.iter().map(|index| index / WIDTH as usize).collect();
        assert_eq!(rows.len(), 1, "the whole change lives on POD's eye row");
    }

    /// The property that makes the repeating motion safe to have at all: it
    /// is redundant. A frame with every kind of movement switched off still
    /// names the state in words, so `SENATE_MOTION=off` costs the user
    /// nothing, and nobody has to read a blink as evidence of anything.
    #[test]
    fn a_frame_that_never_moves_still_names_the_state() {
        let mut state = running_state();
        state.motion_phase = 0;
        state.reacting = false;
        let text = render_text(&state, 160, 40);
        assert!(
            text.contains("RUNNING"),
            "the run state is not written down"
        );
        assert!(
            text.contains("BUILDING"),
            "the stage's work is not written down"
        );
    }

    /// Motion may repaint a cell; it may never move one. A frame drawn mid
    /// blink and a frame drawn between blinks differ only where POD's own
    /// eyes are, so nothing reflows, no width changes, and no line the user
    /// is reading shifts underneath them.
    #[test]
    fn a_blink_repaints_cells_and_never_moves_them() {
        const WIDTH: u16 = 160;
        let mut resting = running_state();
        resting.motion_phase = 0;
        let mut blinking = running_state();
        // The blink tick of the loop in `motion`, which the prop cycle rests
        // on — so the only cells this frame may repaint are POD's eyes.
        blinking.motion_phase = 5;

        let before = render_symbols(&resting, WIDTH, 40);
        let after = render_symbols(&blinking, WIDTH, 40);
        assert_eq!(before.len(), after.len(), "the grid itself must not move");

        let changed: Vec<usize> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter(|(_, (old, new))| old != new)
            .map(|(index, _)| index)
            .collect();
        assert!(
            !changed.is_empty(),
            "running work has to look alive on an operating surface"
        );
        assert!(
            changed.len() <= 4,
            "a blink is four eye cells, not {} cells of the screen",
            changed.len()
        );
        let rows: std::collections::HashSet<usize> =
            changed.iter().map(|index| index / WIDTH as usize).collect();
        assert_eq!(rows.len(), 1, "the whole change lives on POD's eye row");
    }

    #[test]
    fn pod_is_seated_under_the_hero_rather_than_floating() {
        let text = render_text(&running_state(), 160, 40);
        let resources = text.find("RESOURCES").unwrap();
        let tail = &text[resources..];
        let rule = tail.find("──────────").expect("POD sits under a rule");
        let pod = tail.find(POD_SHELL).unwrap();
        assert!(rule < pod, "the rule separates the hero from its operator");
    }

    /// The rail's bottom strip narrates the run in plain sentences, and the
    /// sentence follows the run's canonical status rather than the selection.
    #[test]
    fn status_strip_speaks_the_runs_actual_state_in_sentences() {
        let running = render_text(&running_state(), 160, 40);
        assert!(running.contains("STATUS"));
        assert!(
            running.contains("Implementation has been running for"),
            "the live stage is named with its elapsed"
        );
        assert!(running.contains("1 of 3 stages complete."));

        let completed = render_text(&completed_state(), 160, 40);
        assert!(completed.contains("Run complete — the result is ready to review."));
        assert!(completed.contains("1 of 1 stages complete."));

        // A rail too short for the strip spends its rows on the stages.
        assert!(!render_text(&running_state(), 70, 15).contains("STATUS"));
    }

    /// A workflow may complete over an optional stage's failure — the
    /// decision ruled with one review missing. "Run complete" alone would
    /// hide that, so every completed surface names the failure: the strip's
    /// sentence, its stage count, and the hero's tally. What the decision
    /// ruled stays a quote from its own artifact, never this strip's claim.
    #[test]
    fn a_run_completed_over_a_failure_says_so_everywhere_it_counts() {
        // The sentences are asserted at the seam — the rendered rail wraps
        // long lines, so `contains` on the screen text cannot see them whole.
        let sentences = status_sentences(&completed_with_failure_details(), at(12, 13, 0), 160);
        assert_eq!(
            sentences[0],
            "Run complete — Spec review failed, but the decision was still reached. \
             Its verdict is ready to review."
        );
        assert_eq!(sentences[1], "4 of 5 stages complete, 1 failed.");

        let text = render_text(&completed_with_failure_state(), 160, 40);
        assert!(text.contains("✓ 4 of 5 stages completed · Spec review failed"));
        assert!(
            !text.contains("the result is ready to review."),
            "the plain sentence would hide the failure"
        );
    }

    /// A completed stage's badge already reads COMPLETED; the hero does not
    /// repeat it as prose while the run works on.
    #[test]
    fn a_completed_stage_is_not_narrated_as_finished() {
        let mut state = running_state();
        state.selected_stage_index = 0;
        state.selected_stage = Some(StageId::new("architecture").unwrap());
        let text = render_text(&state, 160, 40);
        assert!(text.contains("COMPLETED"), "the badge carries the state");
        assert!(
            !text.contains("Stage finished"),
            "the badge is not repeated as prose"
        );
    }

    #[test]
    fn selection_uses_no_background_so_pod_keeps_its_contrast() {
        // M13d.1 noted that POD's solid eyes lose contrast on a
        // background-highlighted row. The rail marks selection with a cursor
        // and modifiers only, so nothing in the left column paints a surface.
        for status in [
            StageStatus::Running,
            StageStatus::Completed,
            StageStatus::Pending,
            StageStatus::NeedsUser,
            StageStatus::Failed,
        ] {
            for selected in [true, false] {
                assert!(
                    stage_name_style(status, selected).bg.is_none(),
                    "{status:?} selected={selected} paints a background"
                );
            }
        }
        let state = running_state();
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rail = 160_u16 * 38 / 100;
        for y in 2..38 {
            for x in 0..rail {
                // POD's sprite is the one legitimate surface: a `▀` cell
                // carries its bottom pixel in the background. Everything
                // else in the rail stays unpainted.
                if buffer[(x, y)].symbol() == "▀" {
                    continue;
                }
                assert_eq!(
                    buffer[(x, y)].bg,
                    ratatui::style::Color::Reset,
                    "the pipeline rail stays unpainted at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn state_is_never_carried_by_color_alone() {
        // Each state pairs its color with a glyph and a word, so a monochrome
        // terminal reads the same interface.
        for (status, word) in [
            (StageStatus::Running, "RUNNING"),
            (StageStatus::NeedsUser, "NEEDS YOU"),
            (StageStatus::Failed, "FAILED"),
            (StageStatus::Completed, "COMPLETED"),
        ] {
            let visual = stage_visual(status);
            assert_eq!(visual.label, word);
            assert!(!visual.glyph.is_empty());
        }
        let mut state = running_state();
        state.details.as_mut().unwrap().status = RunStatus::Failed;
        state.details.as_mut().unwrap().stages[1].status = StageStatus::Failed;
        let text = render_text(&state, 160, 40);
        assert!(text.contains("✗ FAILED"), "glyph and word travel together");
    }

    #[test]
    fn footer_shapes_follow_canonical_state() {
        let running = actions_text(&running_state());
        assert!(running.starts_with(" o  Result"));
        assert!(!running.contains("Apply") && !running.contains("Resolve"));

        let completed = actions_text(&completed_state());
        assert!(completed.starts_with(" X  Discard"));
        assert!(!completed.contains(" o  Result"), "review, not monitoring");

        let mut technical = running_state();
        technical.technical = true;
        assert!(
            actions_text(&technical).contains(" i  Operational"),
            "the toggle names the mode it leads to"
        );
    }

    #[test]
    fn operational_information_survives_every_supported_size() {
        for (width, height) in [(160, 40), (120, 35), (100, 30), (80, 26), (70, 24)] {
            let text = render_text(&running_state(), width, height);
            assert!(
                text.contains("RUNNING"),
                "status survives at {width}x{height}"
            );
            assert!(
                text.contains("Implementation") || text.contains("IMPLEMENTATION"),
                "current stage survives at {width}x{height}"
            );
            assert!(
                text.contains(" o  Result"),
                "the primary action survives at {width}x{height}"
            );
            assert!(
                text.contains("2m 14s") || text.contains("14s"),
                "elapsed survives at {width}x{height}"
            );
        }
        // POD is the first thing to go, and only after operational content fits.
        assert!(render_text(&running_state(), 160, 40).contains(POD_SHELL));
        assert!(!render_text(&running_state(), 70, 24).contains(POD_SHELL));
    }

    #[test]
    fn narrow_attention_keeps_the_state_action() {
        let mut state = running_state();
        let details = state.details.as_mut().unwrap();
        details.status = RunStatus::NeedsUser;
        details.stages[1].status = StageStatus::NeedsUser;
        details.attention = vec![crate::app::AttentionSummary {
            id: crate::domain::AttentionRequestId::from_u128(1),
            stage_id: StageId::new("implementation").unwrap(),
            kind: AttentionKind::Permission,
            summary: "Claude requests permission to use Bash".to_owned(),
        }];
        let text = render_text(&state, 70, 24);
        assert!(
            text.contains("ACTION REQUIRED"),
            "attention cannot be missed"
        );
        assert!(text.contains(" u  Resolve attention"));
    }

    #[test]
    fn attention_overlay_distinguishes_permission_from_question() {
        let mut state = running_state();
        let details = state.details.as_mut().unwrap();
        details.attention = vec![crate::app::AttentionSummary {
            id: crate::domain::AttentionRequestId::from_u128(1),
            stage_id: StageId::new("implementation").unwrap(),
            kind: AttentionKind::Permission,
            summary: "allow network access".to_owned(),
        }];
        state.overlay = Some(Overlay::Attention);
        let text = render_text(&state, 120, 30);
        assert!(text.contains("Permission request"));
        assert!(text.contains("Enter approve"));
        assert!(text.contains("Ctrl-S skip"), "skip must be discoverable");
        assert!(
            text.contains("Response (optional):"),
            "permission offers an answer that continues without granting"
        );

        state.details.as_mut().unwrap().attention[0].kind = AttentionKind::Question;
        let text = render_text(&state, 120, 30);
        assert!(
            text.contains("Response:"),
            "question keeps editable response"
        );
        assert!(text.contains("Enter submit"));
    }

    fn update_info(current: &str, available: &str) -> crate::update::UpdateInfo {
        crate::update::UpdateInfo {
            current_version: semver::Version::parse(current).unwrap(),
            available_version: semver::Version::parse(available).unwrap(),
            tag: format!("v{available}"),
            release_url: "https://example.invalid/r".to_owned(),
            published_at: None,
        }
    }

    #[test]
    fn no_update_overlay_exists_without_an_available_release() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        assert!(
            !state.update_prompt_is_due(),
            "nothing is offered before a check concludes"
        );
        // A concluded check that found nothing leaves the field empty.
        state.update = None;
        state.overlay = Some(Overlay::Update);
        let text = render_text(&state, 120, 30);
        assert!(
            !text.contains("UPDATE AVAILABLE"),
            "the overlay renders nothing without an update"
        );
    }

    #[test]
    fn an_available_update_prompts_with_both_versions() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.update = Some(update_info("0.1.0", "0.2.0"));
        state.update_install = Some(crate::update::InstallSource::OfficialBinary);
        assert!(state.update_prompt_is_due());
        state.overlay = Some(Overlay::Update);
        let text = render_text(&state, 120, 30);
        assert!(text.contains("UPDATE AVAILABLE"));
        assert!(text.contains("0.1.0"));
        assert!(text.contains("0.2.0"));
        assert!(text.contains("Install now?"));
        assert!(text.contains("It applies when The Senate restarts."));
        assert!(text.contains("→ Yes"), "the default answer is highlighted");
        assert!(text.contains("No"));
    }

    #[test]
    fn an_unsupported_installation_is_told_the_truth() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.update = Some(update_info("0.1.0", "0.2.0"));
        state.update_install = Some(crate::update::InstallSource::Source);
        state.overlay = Some(Overlay::Update);
        let text = render_text(&state, 120, 30);
        assert!(text.contains("UPDATE AVAILABLE"));
        assert!(text.contains("managed from source"));
        assert!(
            !text.contains("Install now?"),
            "an install that cannot happen is never offered"
        );
    }

    #[test]
    fn the_update_prompt_yields_to_run_attention_and_stays_on_the_runs_screen() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.update = Some(update_info("0.1.0", "0.2.0"));
        assert!(state.update_prompt_is_due(), "quiet Runs screen");

        state.screen = Screen::RunDetail;
        assert!(!state.update_prompt_is_due(), "never on the mission deck");
        state.screen = Screen::Runs;

        state.overlay = Some(Overlay::Attention);
        assert!(!state.update_prompt_is_due(), "never over another overlay");
        state.overlay = None;

        state.begin_action(
            crate::tui::worker::ActionKind::Apply,
            Some(RunId::from_u128(1)),
        );
        assert!(!state.update_prompt_is_due(), "never during an action");
        state.settle_action(
            crate::tui::worker::ActionKind::Apply,
            Some(RunId::from_u128(1)),
        );

        state.replace_runs(vec![RunListItem {
            id: RunId::from_u128(1),
            workflow: WorkflowKind::Standard,
            status: RunStatus::NeedsUser,
            task_summary: "OAuth".to_owned(),
            repository: None,
            updated_at: at(12, 0, 0),
            archived: false,
        }]);
        assert!(
            !state.update_prompt_is_due(),
            "a run that needs the user outranks a software update"
        );
    }

    #[test]
    fn a_dismissed_update_never_reopens_in_this_process() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.update = Some(update_info("0.1.0", "0.2.0"));
        state.update_dismissed = true;
        assert!(!state.update_prompt_is_due());
    }

    #[test]
    fn the_update_prompt_uses_mission_deck_theme_and_fits_narrow_terminals() {
        let mut state = TuiState::new(std::path::Path::new("/repo"));
        state.update = Some(update_info("0.1.0", "0.2.0"));
        state.update_install = Some(crate::update::InstallSource::OfficialBinary);
        state.overlay = Some(Overlay::Update);
        for (width, height) in [(160, 40), (100, 30), (70, 24), (50, 12)] {
            let text = render_text(&state, width, height);
            assert!(
                text.contains("UPDATE AVAILABLE"),
                "prompt survives {width}x{height}"
            );
            assert!(
                text.contains("0.2.0"),
                "the new version survives {width}x{height}"
            );
        }
        // The band is an aside, never a takeover.
        let band = update_rect(
            Rect::new(0, 0, 160, 40),
            &[Line::from("one"), Line::from("two")],
        );
        assert!(band.width <= 62 && band.height <= 12);
        assert!(band.x > 0 && band.y > 0);

        // A long guidance line wraps, and the band grows so the action row
        // stays visible instead of being clipped.
        let tall = update_rect(
            Rect::new(0, 0, 160, 40),
            &[Line::from("x".repeat(200)), Line::from("[Enter] Continue")],
        );
        assert!(
            tall.height > band.height,
            "wrapped content is accounted for"
        );
    }

    #[test]
    fn confirmation_overlays_keep_existing_semantics() {
        let mut state = completed_state();
        state.overlay = Some(Overlay::ApplyConfirm);
        assert!(render_text(&state, 120, 30).contains("Enter confirms apply"));
        state.overlay = Some(Overlay::DiscardConfirm);
        assert!(render_text(&state, 120, 30).contains("Enter confirms discard"));
    }

    /// The publishing card says the wait is a wait: what is happening, for
    /// how long, and that hiding it stops nothing.
    #[test]
    fn the_publishing_card_names_the_work_and_its_clock() {
        let mut state = completed_state();
        state.overlay = Some(Overlay::Publishing);
        state.publishing = Some(super::super::state::PublishInFlight {
            run_id: RunId::from_u128(7),
            started: std::time::Instant::now(),
        });
        let text = render_text(&state, 120, 30);
        assert!(text.contains("PUSHING TO PR"), "{text}");
        assert!(text.contains("pushing the branch to origin"), "{text}");
        assert!(text.contains("0s"), "the clock is on screen: {text}");
        assert!(text.contains("Esc hides this card"), "{text}");
    }

    /// The starting card names the step the start is on, and once a pull
    /// request check has gone quiet for a while, the usual reason why.
    #[test]
    fn the_starting_card_names_its_step_and_explains_a_silent_host() {
        let mut state = completed_state();
        state.overlay = Some(Overlay::Starting);
        state.starting = Some(super::super::state::StartInFlight {
            ticket: 0,
            task: "Review the wpcom change".to_owned(),
            started: std::time::Instant::now(),
            progress: Some(StartProgress::CheckingPullRequest {
                url: "https://github.a8c.com/Automattic/wpcom/pull/241491".to_owned(),
                host: "github.a8c.com".to_owned(),
            }),
        });
        let text = render_text(&state, 120, 30);
        assert!(text.contains("STARTING RUN"), "{text}");
        assert!(text.contains("Review the wpcom change"), "{text}");
        assert!(text.contains("Checking that gh can read"), "{text}");
        assert!(!text.contains("No answer from"), "not yet: {text}");
        assert!(text.contains("Esc hides this card"), "{text}");

        state.starting.as_mut().unwrap().started = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(SLOW_PULL_REQUEST_CHECK))
            .unwrap();
        let text = render_text(&state, 120, 30);
        assert!(text.contains("No answer from github.a8c.com yet"), "{text}");
    }

    /// The refusal card keeps every line of the reason, because the fix is
    /// never on the first one.
    #[test]
    fn the_start_failed_card_keeps_the_whole_reason() {
        let mut state = completed_state();
        state.overlay = Some(Overlay::StartFailed);
        state.start_failure = Some(super::super::state::StartFailure {
            task: "Review the wpcom change".to_owned(),
            error: "Pull request cannot be read.\n  i/o timeout\n  Fix: restore access".to_owned(),
            draft_restored: true,
        });
        let text = render_text(&state, 120, 40);
        assert!(text.contains("RUN NOT STARTED"), "{text}");
        assert!(text.contains("Pull request cannot be read."), "{text}");
        assert!(text.contains("i/o timeout"), "{text}");
        assert!(text.contains("Fix: restore access"), "{text}");
        assert!(text.contains("Enter"), "{text}");
    }

    /// The published card is built around the URL, and offers its two uses.
    /// Without a URL, the offers go away and the reason takes their place.
    #[test]
    fn the_published_card_headlines_the_url_and_offers_open_and_copy() {
        let mut state = completed_state();
        state.overlay = Some(Overlay::Published);
        state.published = Some(PublishOutcome::Pushed {
            run_id: RunId::from_u128(7),
            branch: "senate/run-7".to_owned(),
            commit: "abcdef1234567890".to_owned(),
            pull_request: PullRequestStatus::Created("https://example.invalid/pull/7".to_owned()),
            note: None,
        });
        let text = render_text(&state, 120, 30);
        assert!(text.contains("PULL REQUEST CREATED"), "{text}");
        assert!(text.contains("https://example.invalid/pull/7"), "{text}");
        assert!(text.contains("senate/run-7"), "{text}");
        assert!(text.contains("abcdef123456"), "commit is shortened: {text}");
        assert!(!text.contains("abcdef1234567890"), "{text}");
        assert!(
            text.contains("open in browser") && text.contains("copy URL"),
            "{text}"
        );

        state.published = Some(PublishOutcome::Pushed {
            run_id: RunId::from_u128(7),
            branch: "senate/run-7".to_owned(),
            commit: "abcdef1234567890".to_owned(),
            pull_request: PullRequestStatus::Unavailable("gh is not installed".to_owned()),
            note: None,
        });
        let text = render_text(&state, 120, 30);
        assert!(text.contains("BRANCH PUSHED"), "{text}");
        assert!(text.contains("gh is not installed"), "{text}");
        assert!(
            !text.contains("open in browser") && !text.contains("copy URL"),
            "{text}"
        );

        state.published = Some(PublishOutcome::Failed {
            run_id: RunId::from_u128(7),
            error: "remote rejected the push".to_owned(),
        });
        let text = render_text(&state, 120, 30);
        assert!(text.contains("PUBLISH FAILED"), "{text}");
        assert!(text.contains("remote rejected the push"), "{text}");
    }

    /// Verbatim from a real Codex `turn.failed` on a subscription account whose
    /// `~/.codex/config.toml` named a model the account cannot use.
    const REFUSED_MODEL_REASON: &str = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"The 'gpt-6-sol' model is not supported when using Codex with a ChatGPT account."}}"#;

    #[test]
    fn model_fallback_is_offered_only_for_a_codex_stage_whose_model_was_refused() {
        let mut refused = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Failed,
        );
        refused.failure_reason = Some(REFUSED_MODEL_REASON.to_owned());
        assert_eq!(
            refused.model_fallback(),
            Some(crate::app::CODEX_FALLBACK_MODEL)
        );
        assert!(
            crate::domain::ModelId::new(crate::app::CODEX_FALLBACK_MODEL).is_ok(),
            "the fallback must survive ModelId, or the row retries on the native default"
        );

        let mut other = refused.clone();
        other.failure_reason = Some("You've hit your usage limit.".to_owned());
        assert_eq!(other.model_fallback(), None, "any other failure");

        let mut claude = refused.clone();
        claude.actual_provider = Some("claude".to_owned());
        assert_eq!(claude.model_fallback(), None, "not a Codex stage");

        let mut on_fallback = refused.clone();
        on_fallback.configured_model = Some(crate::app::CODEX_FALLBACK_MODEL.to_owned());
        assert_eq!(
            on_fallback.model_fallback(),
            None,
            "the fallback itself was refused; offering it again only fails again"
        );

        let mut pending = refused;
        pending.status = StageStatus::Pending;
        assert_eq!(pending.model_fallback(), None, "nothing to retry");
    }

    #[test]
    fn retry_route_chooser_offers_the_fallback_model_when_codex_refused_it() {
        let mut state = completed_state();
        let details = state.details.as_mut().unwrap();
        details.stages[state.selected_stage_index].status = StageStatus::Failed;
        details.stages[state.selected_stage_index].failure_reason =
            Some(REFUSED_MODEL_REASON.to_owned());
        state.retry_route_choice = RetryRouteChoice::CodexModel(crate::app::CODEX_FALLBACK_MODEL);
        state.overlay = Some(Overlay::RetryRoute);
        let text = render_text(&state, 120, 30);
        assert!(
            text.contains(&format!("→ Codex on {}", crate::app::CODEX_FALLBACK_MODEL)),
            "{text}"
        );
        assert!(text.contains("Configured provider ("), "{text}");
        assert!(text.contains("Claude (native default model)"), "{text}");
    }

    #[test]
    fn retry_route_chooser_lists_the_configured_provider_first() {
        let mut state = completed_state();
        state.retry_route_choice = RetryRouteChoice::Claude;
        state.overlay = Some(Overlay::RetryRoute);
        let text = render_text(&state, 120, 30);
        assert!(text.contains("Retry stage"), "{text}");
        assert!(text.contains("Configured provider ("), "{text}");
        assert!(text.contains("→ Claude (native default model)"), "{text}");
        assert!(text.contains("Codex (native default model)"), "{text}");
        assert!(text.contains("Only this stage moves"), "{text}");
    }

    #[test]
    fn runtime_summary_marks_an_operator_override() {
        let mut overridden = stage(
            "implementation",
            StageKind::Implementation,
            Role::Implementer,
            StageStatus::Pending,
        );
        overridden.configured_provider = "claude".to_owned();
        overridden.route_overridden = true;
        assert!(runtime_summary(&overridden).starts_with("claude · native default (override)"));
    }

    fn mission_fixture(core_status: WorkPackageStatus) -> crate::app::MissionDetails {
        let at = Utc.with_ymd_and_hms(2026, 9, 22, 12, 0, 0).unwrap();
        let core = crate::domain::WorkPackageId::new("core").unwrap();
        let package = |id: &str, title: &str, status: WorkPackageStatus, deps: Vec<_>| {
            crate::app::WorkPackageSummary {
                id: crate::domain::WorkPackageId::new(id).unwrap(),
                title: title.to_owned(),
                goal: format!("deliver {title}"),
                rationale: String::new(),
                scope: String::new(),
                acceptance_criteria: vec!["it works".to_owned()],
                verification: String::new(),
                workflow: WorkflowKind::Fast,
                status,
                dependencies: deps,
                runs: vec![],
                current_run: (status != WorkPackageStatus::Planned
                    && status != WorkPackageStatus::Ready)
                    .then(|| RunId::from_u128(9)),
                run_status: None,
                reason: None,
                result: None,
                handoff: None,
                created_at: at,
                updated_at: at,
            }
        };
        let mut attention = crate::domain::MissionAttention::default();
        if core_status == WorkPackageStatus::Delivered {
            attention.awaiting_integration.push(core.clone());
        }
        crate::app::MissionDetails {
            id: crate::domain::MissionId::from_u128(3),
            title: "Greeting tool".to_owned(),
            goal: "greet by name".to_owned(),
            repository: std::path::PathBuf::from("/tmp/greet"),
            base_commit: "abc".to_owned(),
            status: crate::domain::MissionStatus::Active,
            packages: vec![
                package("core", "Core", core_status, vec![]),
                package("docs", "Docs", WorkPackageStatus::Planned, vec![core]),
            ],
            decisions: vec![],
            attention,
            lead: None,
            revision: crate::store::MissionRevision::initial(),
            created_at: at,
            updated_at: at,
        }
    }

    fn mission_state(core_status: WorkPackageStatus) -> TuiState {
        let mut state = TuiState::new(std::path::Path::new("/tmp"));
        let mission = mission_fixture(core_status);
        state.replace_missions(vec![crate::app::MissionListItem {
            id: mission.id,
            title: mission.title.clone(),
            status: mission.status,
            repository: mission.repository.clone(),
            packages: 2,
            integrated: 0,
            active: 0,
            attention: mission.attention.len(),
            updated_at: mission.updated_at,
        }]);
        state.replace_mission(mission);
        state
    }

    /// The campaigns screen lists the campaign and shows its plan beside it,
    /// with the calm line when nothing is waiting on the operator.
    #[test]
    fn the_missions_screen_lists_missions_and_shows_the_selected_plan() {
        let mut state = mission_state(WorkPackageStatus::Ready);
        state.screen = Screen::Missions;
        let text = render_text(&state, 110, 30);
        assert!(text.contains("CAMPAIGNS"), "{text}");
        assert!(text.contains("Greeting tool"), "{text}");
        assert!(text.contains("ORDERS"), "{text}");
        assert!(text.contains("core"), "{text}");
        assert!(text.contains("docs"), "{text}");
        assert!(text.contains("Nothing needs you."), "{text}");
    }

    /// The open campaign is a Consul pane and a Campaign pane: the lead's
    /// word (or the fallback when it has none yet) on the left, the
    /// selected Order's state and what it can take right now on the right.
    #[test]
    fn the_mission_detail_offers_start_or_integrate_by_package_state() {
        let mut state = mission_state(WorkPackageStatus::Ready);
        state.screen = Screen::MissionDetail;
        let text = render_text(&state, 110, 32);
        assert!(text.contains("CAMPAIGN"), "{text}");
        assert!(text.contains("CONSUL"), "{text}");
        assert!(text.contains("Engineering lead"), "{text}");
        assert!(text.contains("No conversation yet."), "{text}");
        assert!(text.contains("CURRENT"), "{text}");
        assert!(text.contains("ready to start"), "{text}");
        assert!(text.contains("1 acceptance criterion"), "{text}");
        assert!(text.contains("Start"), "{text}");
        assert!(!text.contains("Bring in"), "{text}");

        let mut state = mission_state(WorkPackageStatus::Delivered);
        state.screen = Screen::MissionDetail;
        let text = render_text(&state, 110, 32);
        assert!(text.contains("done, bring it in"), "{text}");
        assert!(text.contains("Bring in"), "{text}");
        assert!(text.contains("Open run"), "{text}");
        let actions = footer_line(Screen::MissionDetail, &state, 110)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect::<String>();
        assert!(actions.contains("Bring in"), "{actions}");
        assert!(!actions.contains("Start"), "{actions}");

        let mut state = mission_state(WorkPackageStatus::Blocked);
        state.mission.as_mut().unwrap().attention.blocked.push((
            crate::domain::WorkPackageId::new("core").unwrap(),
            "Permission: Claude Code requests permission for: Bash ./greet.sh Bob — this cannot be granted as a permission rule"
                .to_owned(),
        ));
        state.screen = Screen::MissionDetail;
        let text = render_text(&state, 110, 32);
        assert!(text.contains("needs you"), "{text}");
        assert!(text.contains("1 need your judgment"), "{text}");
        assert!(text.contains("Answer"), "{text}");
    }

    /// Package states, like run states, must survive losing every colour.
    #[test]
    fn every_package_state_stays_distinguishable_with_all_colour_removed() {
        theme::with_palette(
            theme::Palette::resolve(theme::ColorCapability::Mono, theme::ThemeChoice::Native),
            || {
                let states = [
                    WorkPackageStatus::Planned,
                    WorkPackageStatus::Ready,
                    WorkPackageStatus::Running,
                    WorkPackageStatus::Blocked,
                    WorkPackageStatus::Delivered,
                    WorkPackageStatus::Integrated,
                    WorkPackageStatus::Failed,
                    WorkPackageStatus::Cancelled,
                ];
                for (index, status) in states.iter().enumerate() {
                    for other in states.iter().skip(index + 1) {
                        assert_ne!(
                            package_visual(*status).glyph,
                            package_visual(*other).glyph,
                            "{status:?} and {other:?} are told apart only by colour"
                        );
                    }
                    assert_eq!(
                        package_visual(*status).color,
                        package_visual(states[0]).color
                    );
                }
            },
        );
    }
}
