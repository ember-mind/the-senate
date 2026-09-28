//! The world-state projection: the one small, semantic view of a campaign
//! that the 3D Senate reads. It is computed from the canonical mission read
//! model (the same `MissionDetails` the terminal renders) on every request
//! and never stored, so the browser has no state of its own to diverge.
//!
//! Everything here is derived from committed facts: package status, the
//! current run's stages, the lead session's status. Nothing is inferred
//! from prose, and nothing is summarised by a model: the Consul's lines are
//! fixed sentences filled with titles and counts.

use std::collections::HashMap;
use std::hash::BuildHasher;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::app::{
    AppError, MissionDetails, MissionService, RunDetails, RunService, RuntimeProviderFactory,
    WorkPackageSummary,
};
use crate::domain::{
    MissionId, MissionStatus, RunId, RunStatus, StageKind, StageStatus, WorkPackageId,
    WorkPackageStatus,
};

/// Bumped whenever a field changes meaning; the browser refuses a schema it
/// does not know rather than guessing.
pub const WORLD_SCHEMA: u32 = 1;

/// Lines the Consul says at most; the rest is one panel away.
const CONSUL_LINES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorldState {
    pub schema: u32,
    pub source: &'static str,
    pub at: DateTime<Utc>,
    pub campaign: Option<Campaign>,
    pub consul: Consul,
    pub orders: Vec<Order>,
    pub censor: Censor,
    pub attention: Vec<Attention>,
    pub curia: Curia,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Campaign {
    pub id: String,
    pub title: String,
    pub goal: String,
    /// `planning`, `active`, `completed` or `cancelled`.
    pub state: &'static str,
    pub settled: usize,
    pub total: usize,
    /// Orders a Cohort or the Censor is working on right now.
    pub active: usize,
    /// Orders waiting on the user: stopped, failed, or delivered and not yet
    /// integrated.
    pub needs_you: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Consul {
    /// `quiet`, `conferring` (the lead is answering) or `awaiting_you`.
    pub state: &'static str,
    pub summary: Vec<String>,
    pub lead_turns: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Order {
    pub id: String,
    /// Stable number for the Cohort that serves this Order: its position in
    /// creation order, so adding a package never renumbers the others.
    pub cohort: usize,
    pub title: String,
    /// `planned`, `ready`, `working`, `in_review`, `blocked`, `delivered`,
    /// `settled`, `failed` or `cancelled`.
    pub state: &'static str,
    /// What the current run's active stage is doing: `reading`,
    /// `designing`, `coding`, `testing`, `reviewing` or `deciding`. `None`
    /// between stages; the world then shows the Cohort waiting.
    pub activity: Option<&'static str>,
    pub worker: Option<Worker>,
    /// When the active stage (or, failing that, the package) last changed.
    pub since: Option<DateTime<Utc>>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Worker {
    pub provider: String,
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Censor {
    /// `idle` or `reviewing`.
    pub state: &'static str,
    pub order: Option<String>,
    pub reviewer: Option<Worker>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attention {
    /// `blocked`, `failed` or `awaiting_integration`.
    pub kind: &'static str,
    pub order: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Curia {
    /// Always `closed` until decisions get a chamber of their own.
    pub state: &'static str,
}

/// The slice of one stage the projection reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageView {
    pub kind: StageKind,
    pub status: StageStatus,
    pub provider: String,
    pub model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
}

impl StageView {
    fn from_summary(stage: &crate::app::StageSummary) -> Self {
        Self {
            kind: stage.kind,
            status: stage.status,
            provider: stage
                .actual_provider
                .clone()
                .unwrap_or_else(|| stage.configured_provider.clone()),
            model: stage
                .actual_model
                .clone()
                .or_else(|| stage.configured_model.clone()),
            started_at: stage.started_at,
        }
    }

    fn worker(&self) -> Worker {
        Worker {
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }
}

const fn is_review(kind: StageKind) -> bool {
    matches!(
        kind,
        StageKind::CodeQualityReview
            | StageKind::SpecReview
            | StageKind::Review
            | StageKind::IndependentReview
    )
}

const fn activity(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Research => "reading",
        StageKind::Architecture | StageKind::DeepAnalysis | StageKind::Synthesis => "designing",
        StageKind::Implementation
        | StageKind::Simplification
        | StageKind::Fix
        | StageKind::FollowUp
        | StageKind::Lead => "coding",
        StageKind::Verify => "testing",
        StageKind::CodeQualityReview
        | StageKind::SpecReview
        | StageKind::Review
        | StageKind::IndependentReview => "reviewing",
        StageKind::Decision => "deciding",
    }
}

/// The stage that says what the run is doing now: one waiting for the user
/// first, then one running.
fn active_stage(stages: &[StageView]) -> Option<&StageView> {
    stages
        .iter()
        .find(|stage| stage.status == StageStatus::NeedsUser)
        .or_else(|| {
            stages
                .iter()
                .find(|stage| stage.status == StageStatus::Running)
        })
}

/// The stage whose provider best names who is doing the Order's work.
fn implementer(stages: &[StageView]) -> Option<&StageView> {
    stages.iter().rev().find(|stage| {
        matches!(
            stage.kind,
            StageKind::Implementation | StageKind::Fix | StageKind::FollowUp
        ) && stage.status != StageStatus::Pending
    })
}

fn order_state(status: WorkPackageStatus, active: Option<&StageView>) -> &'static str {
    match status {
        WorkPackageStatus::Planned => "planned",
        WorkPackageStatus::Ready => "ready",
        WorkPackageStatus::Running => {
            if active.is_some_and(|stage| is_review(stage.kind)) {
                "in_review"
            } else {
                "working"
            }
        }
        WorkPackageStatus::Blocked => "blocked",
        WorkPackageStatus::Delivered => "delivered",
        WorkPackageStatus::Integrated => "settled",
        WorkPackageStatus::Failed => "failed",
        WorkPackageStatus::Cancelled => "cancelled",
    }
}

const fn mission_state(status: MissionStatus) -> &'static str {
    match status {
        MissionStatus::Planning => "planning",
        MissionStatus::Active => "active",
        MissionStatus::Completed => "completed",
        MissionStatus::Cancelled => "cancelled",
    }
}

/// Roman numeral for a Cohort number, as the terminal and the world write it.
#[must_use]
pub fn numeral(n: usize) -> String {
    const TABLE: [(usize, &str); 9] = [
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut n = n;
    let mut out = String::new();
    for (value, text) in TABLE {
        while n >= value {
            out.push_str(text);
            n -= value;
        }
    }
    out
}

fn project_order<S: BuildHasher>(
    package: &WorkPackageSummary,
    cohort: usize,
    runs: &HashMap<RunId, Vec<StageView>, S>,
) -> Order {
    let stages = package
        .current_run
        .and_then(|run| runs.get(&run))
        .map_or(&[][..], Vec::as_slice);
    let active = active_stage(stages);
    let state = order_state(package.status, active);
    let live = matches!(
        package.status,
        WorkPackageStatus::Running | WorkPackageStatus::Blocked
    );
    let worker = implementer(stages)
        .or(active.filter(|stage| !is_review(stage.kind)))
        .map(StageView::worker);
    Order {
        id: package.id.to_string(),
        cohort,
        title: package.title.clone(),
        state,
        activity: active.filter(|_| live).map(|stage| activity(stage.kind)),
        worker: if package.current_run.is_some() {
            worker
        } else {
            None
        },
        since: active
            .filter(|_| live)
            .and_then(|stage| stage.started_at)
            .or(Some(package.updated_at)),
        reason: package.reason.clone(),
    }
}

/// Projects one mission. `runs` holds the stages of each package's current
/// run; `lead_busy` says whether the lead session is answering right now.
#[must_use]
pub fn project<S: BuildHasher>(
    details: &MissionDetails,
    runs: &HashMap<RunId, Vec<StageView>, S>,
    lead_busy: bool,
    at: DateTime<Utc>,
) -> WorldState {
    // Cohort numbers follow creation order, not the plan's dependency order.
    let mut created: Vec<&WorkPackageSummary> = details.packages.iter().collect();
    created.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    let cohort_of = |id: &WorkPackageId| {
        created
            .iter()
            .position(|p| &p.id == id)
            .map_or(0, |i| i + 1)
    };

    let orders: Vec<Order> = details
        .packages
        .iter()
        .map(|package| project_order(package, cohort_of(&package.id), runs))
        .collect();

    let censor = orders
        .iter()
        .zip(&details.packages)
        .find(|(order, _)| order.state == "in_review")
        .map_or(
            Censor {
                state: "idle",
                order: None,
                reviewer: None,
            },
            |(order, package)| Censor {
                state: "reviewing",
                order: Some(order.id.clone()),
                reviewer: package
                    .current_run
                    .and_then(|run| runs.get(&run))
                    .and_then(|stages| active_stage(stages))
                    .map(StageView::worker),
            },
        );

    let title_of = |id: &WorkPackageId| {
        details
            .package(id)
            .map_or_else(|| id.to_string(), |package| package.title.clone())
    };
    let attention = attention_items(details);

    let settled = orders.iter().filter(|o| o.state == "settled").count();
    let active = orders
        .iter()
        .filter(|o| matches!(o.state, "working" | "in_review" | "blocked"))
        .count();
    let needs_you = orders
        .iter()
        .filter(|o| matches!(o.state, "blocked" | "failed" | "delivered"))
        .count();
    let consul_state = if lead_busy {
        "conferring"
    } else if needs_you > 0 {
        "awaiting_you"
    } else {
        "quiet"
    };
    let summary = consul_lines(details, &orders, settled, needs_you, &title_of);

    WorldState {
        schema: WORLD_SCHEMA,
        source: "live",
        at,
        campaign: Some(Campaign {
            id: details.id.to_string(),
            title: details.title.clone(),
            goal: details.goal.clone(),
            state: mission_state(details.status),
            settled,
            total: orders.iter().filter(|o| o.state != "cancelled").count(),
            active,
            needs_you,
        }),
        consul: Consul {
            state: consul_state,
            summary,
            lead_turns: details.lead.as_ref().map_or(0, |lead| lead.turns),
        },
        orders,
        censor,
        attention,
        curia: Curia { state: "closed" },
    }
}

fn attention_items(details: &MissionDetails) -> Vec<Attention> {
    let mut attention = Vec::new();
    for (id, reason) in &details.attention.blocked {
        attention.push(Attention {
            kind: "blocked",
            order: Some(id.to_string()),
            text: reason.clone(),
        });
    }
    for (id, reason) in &details.attention.failed {
        attention.push(Attention {
            kind: "failed",
            order: Some(id.to_string()),
            text: reason.clone(),
        });
    }
    for id in &details.attention.awaiting_integration {
        attention.push(Attention {
            kind: "awaiting_integration",
            order: Some(id.to_string()),
            text: "Delivered; integrate it to settle it.".to_owned(),
        });
    }
    attention
}

/// The Consul's standing report: fixed sentences over committed facts,
/// most urgent first.
fn consul_lines(
    details: &MissionDetails,
    orders: &[Order],
    settled: usize,
    needs_you: usize,
    title_of: &dyn Fn(&WorkPackageId) -> String,
) -> Vec<String> {
    let mut lines = Vec::new();
    if details.status == MissionStatus::Completed {
        lines.push("The campaign is complete.".to_owned());
    }
    for (id, reason) in &details.attention.blocked {
        lines.push(format!(
            "{} has stopped: {}",
            title_of(id),
            first_line(reason)
        ));
    }
    for (id, reason) in &details.attention.failed {
        lines.push(format!("{} failed: {}", title_of(id), first_line(reason)));
    }
    for id in &details.attention.awaiting_integration {
        lines.push(format!(
            "{} is delivered and waits to be integrated.",
            title_of(id)
        ));
    }
    for order in orders {
        match (order.state, order.activity) {
            ("in_review", _) => lines.push(format!("The Censor is reviewing {}.", order.title)),
            ("working", activity) => {
                let verb = match activity {
                    Some("reading") => "is reading the repository for",
                    Some("designing") => "is planning",
                    Some("coding") => "is writing",
                    Some("testing") => "is running the checks for",
                    Some("deciding") => "is weighing",
                    _ => "is working on",
                };
                lines.push(format!(
                    "Cohort {} {verb} {}.",
                    numeral(order.cohort),
                    order.title
                ));
            }
            _ => {}
        }
    }
    let running = orders
        .iter()
        .any(|o| matches!(o.state, "working" | "in_review"));
    if !running && needs_you == 0 && details.status != MissionStatus::Completed {
        lines.push("Nothing is running.".to_owned());
        let total = orders.iter().filter(|o| o.state != "cancelled").count();
        lines.push(format!("{settled} of {total} Orders are settled."));
    } else if needs_you == 0 && running {
        lines.push("Nothing needs your judgment.".to_owned());
    }
    lines.truncate(CONSUL_LINES);
    lines
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut out: String = line.chars().take(140).collect();
    if line.chars().count() > 140 {
        out.push('…');
    }
    out
}

fn empty(at: DateTime<Utc>) -> WorldState {
    WorldState {
        schema: WORLD_SCHEMA,
        source: "live",
        at,
        campaign: None,
        consul: Consul {
            state: "quiet",
            summary: vec![
                "There is no campaign yet.".to_owned(),
                "Start one from the terminal with `senate mission create`.".to_owned(),
            ],
            lead_turns: 0,
        },
        orders: Vec::new(),
        censor: Censor {
            state: "idle",
            order: None,
            reviewer: None,
        },
        attention: Vec::new(),
        curia: Curia { state: "closed" },
    }
}

fn now() -> DateTime<Utc> {
    // `chrono`'s clock feature is off in this crate; read the same clock
    // through `SystemTime`.
    std::time::SystemTime::now().into()
}

/// Which mission the world shows: the one asked for, else the most recently
/// updated open one, else the most recently updated of all.
fn resolve(
    missions: &MissionService,
    requested: Option<MissionId>,
) -> Result<Option<MissionId>, AppError> {
    if requested.is_some() {
        return Ok(requested);
    }
    let mut list = missions.list_missions()?;
    list.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    Ok(list
        .iter()
        .find(|item| !item.status.is_closed())
        .or_else(|| list.first())
        .map(|item| item.id))
}

const fn lead_busy(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Created | RunStatus::Preparing | RunStatus::Ready | RunStatus::Running
    )
}

/// Whether `MissionService::ask_lead` takes a new message for a lead in
/// this state: it continues a completed session and opens a fresh one after
/// a failed or discarded one; anything else is still the earlier turn.
const fn lead_accepts(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Completed | RunStatus::Failed | RunStatus::Discarded
    )
}

/// A message from the browser on its way to the lead. Held from the moment
/// a POST is accepted until `ask_lead` returns, so two quick POSTs can never
/// both be told yes.
static ASK_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct InFlight;

impl InFlight {
    fn take() -> Option<Self> {
        ASK_IN_FLIGHT
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        ASK_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Release);
    }
}

struct Loaded {
    details: MissionDetails,
    runs: HashMap<RunId, RunDetails>,
}

fn load(mission: Option<MissionId>) -> Result<Option<Loaded>, AppError> {
    let missions = MissionService::from_environment()?;
    let Some(id) = resolve(&missions, mission)? else {
        return Ok(None);
    };
    let details = missions.inspect_mission(id)?;
    let service = RunService::from_environment(RuntimeProviderFactory)?;
    let mut runs = HashMap::new();
    for package in &details.packages {
        if let Some(run) = package.current_run {
            runs.insert(run, service.inspect_run(run)?);
        }
    }
    Ok(Some(Loaded { details, runs }))
}

/// The snapshot the server hands the browser for `GET /api/world`.
///
/// # Errors
/// Returns persistence errors from the mission and run read models.
pub fn snapshot(mission: Option<MissionId>) -> Result<WorldState, AppError> {
    let Some(loaded) = load(mission)? else {
        return Ok(empty(now()));
    };
    let stages: HashMap<RunId, Vec<StageView>> = loaded
        .runs
        .iter()
        .map(|(id, run)| {
            (
                *id,
                run.stages.iter().map(StageView::from_summary).collect(),
            )
        })
        .collect();
    let busy = loaded
        .details
        .lead
        .as_ref()
        .is_some_and(|lead| lead_busy(lead.run_status));
    Ok(project(&loaded.details, &stages, busy, now()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OrderDetail {
    pub id: String,
    pub title: String,
    pub state: &'static str,
    pub goal: String,
    pub acceptance: Vec<String>,
    pub stages: Vec<StageLine>,
    pub verification: Option<Outcome>,
    pub review: Option<Outcome>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StageLine {
    pub kind: String,
    pub label: String,
    pub status: String,
    pub provider: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub status: String,
    /// The artifact's own bottom line, verbatim.
    pub bottom_line: Option<String>,
}

fn snake(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

fn label(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Research => "Research",
        StageKind::Architecture => "Architecture",
        StageKind::Implementation => "Implementation",
        StageKind::Simplification => "Simplification",
        StageKind::CodeQualityReview => "Code quality review",
        StageKind::SpecReview => "Spec review",
        StageKind::Review => "Review",
        StageKind::IndependentReview => "Independent review",
        StageKind::DeepAnalysis => "Deep analysis",
        StageKind::Synthesis => "Synthesis",
        StageKind::Decision => "Decision",
        StageKind::Fix => "Fix",
        StageKind::Lead => "Lead",
        StageKind::FollowUp => "Follow-up",
        StageKind::Verify => "Verification",
    }
}

/// One Order as its panel shows it (`GET /api/order/<id>`). `None` when the
/// mission has no such package.
///
/// # Errors
/// Returns persistence errors from the mission and run read models.
pub fn order_detail(
    mission: Option<MissionId>,
    order: &str,
) -> Result<Option<OrderDetail>, AppError> {
    let Some(loaded) = load(mission)? else {
        return Ok(None);
    };
    let Some(package) = loaded
        .details
        .packages
        .iter()
        .find(|package| package.id.to_string() == order)
    else {
        return Ok(None);
    };
    let run = package.current_run.and_then(|id| loaded.runs.get(&id));
    let views: Vec<StageView> = run
        .map(|run| run.stages.iter().map(StageView::from_summary).collect())
        .unwrap_or_default();
    let outcome = |o: &crate::domain::StageOutcome| Outcome {
        status: snake(o.status),
        bottom_line: o.bottom_line.clone(),
    };
    Ok(Some(OrderDetail {
        id: package.id.to_string(),
        title: package.title.clone(),
        state: order_state(package.status, active_stage(&views)),
        goal: package.goal.clone(),
        acceptance: package.acceptance_criteria.clone(),
        stages: run
            .map(|run| {
                run.stages
                    .iter()
                    .map(|stage| StageLine {
                        kind: snake(stage.kind),
                        label: label(stage.kind).to_owned(),
                        status: snake(stage.status),
                        provider: (stage.kind != StageKind::Verify)
                            .then(|| StageView::from_summary(stage).provider),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        verification: package
            .result
            .as_ref()
            .and_then(|result| result.verification.as_ref())
            .map(outcome),
        review: package
            .result
            .as_ref()
            .and_then(|result| result.reviews.last())
            .map(outcome),
        reason: package.reason.clone(),
    }))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConsulLog {
    pub turns: Vec<Turn>,
    pub busy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Turn {
    pub role: &'static str,
    pub text: String,
}

/// The lead's latest answer, verbatim without its plan-changes section
/// (`GET /api/consul`). The full history stays in the terminal.
///
/// # Errors
/// Returns persistence or artifact integrity errors.
pub fn consul_log(mission: Option<MissionId>) -> Result<ConsulLog, AppError> {
    let missions = MissionService::from_environment()?;
    let Some(id) = resolve(&missions, mission)? else {
        return Ok(ConsulLog {
            turns: Vec::new(),
            busy: false,
        });
    };
    let details = missions.inspect_mission(id)?;
    let busy = details
        .lead
        .as_ref()
        .is_some_and(|lead| lead_busy(lead.run_status));
    let turns = missions
        .latest_lead_answer(id)?
        .map(|answer| {
            vec![Turn {
                role: "consul",
                text: answer.prose().trim().to_owned(),
            }]
        })
        .unwrap_or_default();
    Ok(ConsulLog { turns, busy })
}

/// Why a message to the Consul was not sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskRefused {
    NoCampaign,
    Busy,
    /// The lead's session is stopped or waiting on the operator; the
    /// terminal is where it resumes.
    Stopped,
    Empty,
}

/// Sends a message to the mission's lead through the same service
/// `senate mission ask` uses. The turn runs on a background thread and
/// can take minutes; the answer shows up in [`consul_log`] when it is
/// written. Plan changes it proposes are never applied from here.
///
/// # Errors
/// Returns persistence errors while checking the lead's state.
pub fn ask_consul(
    mission: Option<MissionId>,
    message: &str,
) -> Result<Result<(), AskRefused>, AppError> {
    let message = message.trim().to_owned();
    if message.is_empty() {
        return Ok(Err(AskRefused::Empty));
    }
    let missions = MissionService::from_environment()?;
    let Some(id) = resolve(&missions, mission)? else {
        return Ok(Err(AskRefused::NoCampaign));
    };
    let Some(in_flight) = InFlight::take() else {
        return Ok(Err(AskRefused::Busy));
    };
    let details = missions.inspect_mission(id)?;
    if let Some(lead) = details
        .lead
        .as_ref()
        .filter(|lead| !lead_accepts(lead.run_status))
    {
        return Ok(Err(if lead_busy(lead.run_status) {
            AskRefused::Busy
        } else {
            AskRefused::Stopped
        }));
    }
    std::thread::spawn(move || {
        let _in_flight = in_flight;
        let outcome = RunService::from_environment(RuntimeProviderFactory).and_then(|runs| {
            missions.ask_lead(
                &runs,
                id,
                &message,
                None,
                crate::app::EffortRequest::ProfileDefault,
            )
        });
        if let Err(error) = outcome {
            tracing::warn!(%error, "the Consul could not take the message");
        }
    });
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::domain::MissionAttention;
    use crate::store::MissionRevision;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-26T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn package(
        n: u128,
        title: &str,
        status: WorkPackageStatus,
        run: Option<u128>,
    ) -> WorkPackageSummary {
        WorkPackageSummary {
            id: WorkPackageId::new(format!("wp{n}")).unwrap(),
            title: title.to_owned(),
            goal: String::new(),
            rationale: String::new(),
            scope: String::new(),
            acceptance_criteria: Vec::new(),
            verification: String::new(),
            workflow: crate::domain::WorkflowKind::Standard,
            status,
            dependencies: Vec::new(),
            runs: run.map(RunId::from_u128).into_iter().collect(),
            current_run: run.map(RunId::from_u128),
            run_status: None,
            reason: None,
            result: None,
            handoff: None,
            created_at: at() + chrono::Duration::seconds(i64::try_from(n).unwrap()),
            updated_at: at(),
        }
    }

    fn mission(packages: Vec<WorkPackageSummary>, attention: MissionAttention) -> MissionDetails {
        MissionDetails {
            id: MissionId::from_u128(7),
            title: "Living Characters".to_owned(),
            goal: "Remember things.".to_owned(),
            repository: PathBuf::from("/tmp/repo"),
            base_commit: "abc".to_owned(),
            status: MissionStatus::Active,
            packages,
            decisions: Vec::new(),
            attention,
            lead: None,
            revision: MissionRevision::initial(),
            created_at: at(),
            updated_at: at(),
        }
    }

    fn stage(kind: StageKind, status: StageStatus, provider: &str) -> StageView {
        StageView {
            kind,
            status,
            provider: provider.to_owned(),
            model: None,
            started_at: Some(at()),
        }
    }

    fn standard_run(running: StageKind) -> Vec<StageView> {
        let kinds = [
            StageKind::Implementation,
            StageKind::Verify,
            StageKind::IndependentReview,
        ];
        let at_running = kinds.iter().position(|k| *k == running).unwrap();
        kinds
            .into_iter()
            .enumerate()
            .map(|(i, kind)| {
                let status = match i.cmp(&at_running) {
                    std::cmp::Ordering::Less => StageStatus::Completed,
                    std::cmp::Ordering::Equal => StageStatus::Running,
                    std::cmp::Ordering::Greater => StageStatus::Pending,
                };
                let provider = match kind {
                    StageKind::IndependentReview => "claude",
                    StageKind::Verify => "verify",
                    _ => "deepseek",
                };
                stage(kind, status, provider)
            })
            .collect()
    }

    #[test]
    fn working_order_names_its_cohort_worker_and_activity() {
        let details = mission(
            vec![package(
                1,
                "Persistent memory",
                WorkPackageStatus::Running,
                Some(10),
            )],
            MissionAttention::default(),
        );
        let runs = HashMap::from([(
            RunId::from_u128(10),
            standard_run(StageKind::Implementation),
        )]);
        let world = project(&details, &runs, false, at());
        let order = &world.orders[0];
        assert_eq!(order.state, "working");
        assert_eq!(order.activity, Some("coding"));
        assert_eq!(order.cohort, 1);
        assert_eq!(order.worker.as_ref().unwrap().provider, "deepseek");
        assert_eq!(world.censor.state, "idle");
        assert_eq!(world.consul.state, "quiet");
        assert_eq!(
            world.consul.summary,
            vec![
                "Cohort I is writing Persistent memory.".to_owned(),
                "Nothing needs your judgment.".to_owned()
            ]
        );
    }

    #[test]
    fn a_running_review_stage_puts_the_order_before_the_censor() {
        let details = mission(
            vec![package(
                2,
                "Event queue",
                WorkPackageStatus::Running,
                Some(20),
            )],
            MissionAttention::default(),
        );
        let runs = HashMap::from([(
            RunId::from_u128(20),
            standard_run(StageKind::IndependentReview),
        )]);
        let world = project(&details, &runs, false, at());
        assert_eq!(world.orders[0].state, "in_review");
        // The Cohort that did the work stays the worker; the reviewer is the Censor's.
        assert_eq!(
            world.orders[0].worker.as_ref().unwrap().provider,
            "deepseek"
        );
        assert_eq!(world.censor.state, "reviewing");
        assert_eq!(
            world.censor.order.as_deref(),
            Some(world.orders[0].id.as_str())
        );
        assert_eq!(world.censor.reviewer.as_ref().unwrap().provider, "claude");
        assert!(world.consul.summary[0].contains("The Censor is reviewing Event queue."));
    }

    #[test]
    fn verification_reads_as_testing() {
        let details = mission(
            vec![package(
                1,
                "Persistent memory",
                WorkPackageStatus::Running,
                Some(10),
            )],
            MissionAttention::default(),
        );
        let runs = HashMap::from([(RunId::from_u128(10), standard_run(StageKind::Verify))]);
        let world = project(&details, &runs, false, at());
        assert_eq!(world.orders[0].activity, Some("testing"));
        assert_eq!(
            world.orders[0].worker.as_ref().unwrap().provider,
            "deepseek"
        );
    }

    #[test]
    fn blocked_and_delivered_orders_need_the_user_and_say_why() {
        let mut blocked = package(1, "Event queue", WorkPackageStatus::Blocked, Some(10));
        blocked.reason = Some("Asks permission to run `cargo add tokio`.".to_owned());
        let delivered = package(2, "Save format", WorkPackageStatus::Delivered, Some(20));
        let attention = MissionAttention {
            blocked: vec![(blocked.id.clone(), blocked.reason.clone().unwrap())],
            failed: Vec::new(),
            awaiting_integration: vec![delivered.id.clone()],
        };
        let details = mission(vec![blocked, delivered], attention);
        let mut stages = standard_run(StageKind::Implementation);
        stages[0].status = StageStatus::NeedsUser;
        let runs = HashMap::from([(RunId::from_u128(10), stages)]);
        let world = project(&details, &runs, false, at());
        assert_eq!(world.orders[0].state, "blocked");
        assert_eq!(world.orders[1].state, "delivered");
        assert_eq!(world.campaign.as_ref().unwrap().needs_you, 2);
        assert_eq!(world.consul.state, "awaiting_you");
        assert_eq!(world.attention.len(), 2);
        assert_eq!(
            world.consul.summary[0],
            "Event queue has stopped: Asks permission to run `cargo add tokio`."
        );
        assert_eq!(
            world.consul.summary[1],
            "Save format is delivered and waits to be integrated."
        );
    }

    #[test]
    fn a_quiet_campaign_says_nothing_is_running() {
        let details = mission(
            vec![
                package(1, "Save format", WorkPackageStatus::Integrated, Some(10)),
                package(2, "Decision policy", WorkPackageStatus::Planned, None),
            ],
            MissionAttention::default(),
        );
        let world = project(&details, &HashMap::new(), false, at());
        let campaign = world.campaign.unwrap();
        assert_eq!(
            (campaign.settled, campaign.total, campaign.active),
            (1, 2, 0)
        );
        assert_eq!(
            world.consul.summary,
            vec![
                "Nothing is running.".to_owned(),
                "1 of 2 Orders are settled.".to_owned()
            ]
        );
        assert!(world.orders[1].worker.is_none());
    }

    #[test]
    fn cohort_numbers_follow_creation_not_plan_order() {
        // Dependency order puts the newer package first; numbers stay put.
        let details = mission(
            vec![
                package(9, "Later", WorkPackageStatus::Planned, None),
                package(3, "Earlier", WorkPackageStatus::Planned, None),
            ],
            MissionAttention::default(),
        );
        let world = project(&details, &HashMap::new(), false, at());
        assert_eq!(world.orders[0].cohort, 2);
        assert_eq!(world.orders[1].cohort, 1);
    }

    #[test]
    fn a_busy_lead_is_conferring() {
        let details = mission(Vec::new(), MissionAttention::default());
        let world = project(&details, &HashMap::new(), true, at());
        assert_eq!(world.consul.state, "conferring");
    }

    #[test]
    fn numerals() {
        assert_eq!(numeral(1), "I");
        assert_eq!(numeral(4), "IV");
        assert_eq!(numeral(9), "IX");
        assert_eq!(numeral(14), "XIV");
    }

    #[test]
    fn schema_field_names_match_the_browser() {
        let details = mission(
            vec![package(
                1,
                "Persistent memory",
                WorkPackageStatus::Running,
                Some(10),
            )],
            MissionAttention::default(),
        );
        let runs = HashMap::from([(
            RunId::from_u128(10),
            standard_run(StageKind::Implementation),
        )]);
        let json = serde_json::to_value(project(&details, &runs, false, at())).unwrap();
        for key in [
            "schema",
            "source",
            "at",
            "campaign",
            "consul",
            "orders",
            "censor",
            "attention",
            "curia",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        let order = &json["orders"][0];
        for key in [
            "id", "cohort", "title", "state", "activity", "worker", "since", "reason",
        ] {
            assert!(order.get(key).is_some(), "order missing {key}");
        }
        assert_eq!(json["campaign"]["needs_you"], 0);
    }

    #[test]
    fn the_browser_accepts_a_message_exactly_when_ask_lead_would() {
        use RunStatus::{
            Applied, Completed, Created, Discarded, Failed, Interrupted, NeedsUser, Paused,
            Preparing, Ready, Running,
        };
        for status in [Completed, Failed, Discarded] {
            assert!(lead_accepts(status), "{status:?}");
        }
        for status in [
            Created,
            Preparing,
            Ready,
            Running,
            NeedsUser,
            Paused,
            Interrupted,
            Applied,
        ] {
            assert!(!lead_accepts(status), "{status:?}");
        }
    }

    #[test]
    fn only_one_message_is_in_flight_at_a_time() {
        let first = InFlight::take().expect("free");
        assert!(InFlight::take().is_none());
        drop(first);
        assert!(InFlight::take().is_some());
    }
}
