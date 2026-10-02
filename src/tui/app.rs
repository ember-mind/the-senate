use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};

use crate::app::{
    AppError, ArtifactSummary, LeadAnswer, MissionService, RunListItem, RunService,
    RuntimeProviderFactory, StartProgress,
};
use crate::domain::{MissionId, RunId, RunStatus, StageId, StageStatus, WorkPackageStatus};
use crate::update::{InstallSource, UpdateInfo};

use super::bottom_line;
use super::desktop;
use super::format::short_commit;
use super::input::{self, Intent, map_key, map_text_key};
use super::motion;
use super::render;
use super::state::{
    ConsulAskInFlight, CycleChoice, Overlay, PublishInFlight, PublishOutcome, RetryRouteChoice,
    Screen, StageHeadline, StartFailure, StartInFlight, TuiState, UiMessageKind,
};
use super::terminal::TerminalSession;
use super::worker::ActionKind;
use super::worker::{StartUpdate, Worker, WorkerCommand, WorkerResult, WorkerSuccess};

const EVENT_POLL: Duration = Duration::from_millis(100);
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// One completed background update check: the newer release when there is
/// one, and how this executable was installed.
type UpdateOutcome = (Option<UpdateInfo>, Option<InstallSource>);

/// One completed installation attempt.
type InstallOutcome = Result<String, String>;

pub(crate) struct TuiApp {
    state: TuiState,
    reader: RunService<RuntimeProviderFactory>,
    /// Mission reads: the list and the open mission, observed on refresh.
    missions: MissionService,
    worker: Worker,
    /// Delivers the background update check exactly once. Startup never waits
    /// on it, and a dropped sender simply means no update will be offered.
    update: Receiver<UpdateOutcome>,
    /// Present only while an installation this session started is running.
    installing: Option<Receiver<InstallOutcome>>,
    last_refresh: Instant,
    /// When this session started drawing. Only the blink phase reads it,
    /// so POD's breathing is tied to the session rather than to whichever
    /// redraw happened to come first.
    started: Instant,
    /// Runs the store said were `Running` on this session's first refresh.
    /// Their drivers died with the instance that started them, so this one
    /// resumes each of them; `None` until that first look is taken.
    orphaned: Option<Vec<RunId>>,
    /// Pull request title lookups running in the background.
    titles: TitleLookups,
}

/// Where background pull request title lookups report back: the URL they
/// looked up and the title, if one came back.
struct TitleLookups {
    sender: mpsc::Sender<(String, Option<String>)>,
    receiver: Receiver<(String, Option<String>)>,
}

impl Default for TitleLookups {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self { sender, receiver }
    }
}

impl TuiApp {
    pub(crate) fn new(repository: &Path) -> Result<Self, crate::app::AppError> {
        let reader = RunService::from_environment(RuntimeProviderFactory)?;
        let actions = RunService::from_environment(RuntimeProviderFactory)?;
        Ok(Self {
            state: TuiState::new(repository),
            reader,
            missions: MissionService::from_environment()?,
            worker: Worker::spawn(actions, MissionService::from_environment()?),
            update: spawn_update_check(),
            installing: None,
            last_refresh: Instant::now()
                .checked_sub(REFRESH_INTERVAL)
                .unwrap_or_else(Instant::now),
            started: Instant::now(),
            orphaned: None,
            titles: TitleLookups::default(),
        })
    }

    pub(crate) fn run(mut self, terminal: &mut TerminalSession) -> anyhow::Result<()> {
        spawn_workspace_reclaim();
        self.refresh();
        while !self.state.quit {
            // Progress first: a start's last step must land before the
            // outcome that ends it, or a refusal would be followed by a step.
            self.receive_start_progress();
            self.receive_worker_results();
            self.receive_update();
            self.receive_pull_request_titles();
            self.receive_install();
            self.state.clear_expired_message(Instant::now());
            if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
                self.refresh();
            }
            self.state.motion_phase = motion::active_phase(self.started.elapsed());
            self.state.settle_reaction(Instant::now());
            terminal
                .terminal_mut()
                .draw(|frame| render::render(frame, &self.state))?;
            if event::poll(self.idle_for())? {
                self.handle_event(event::read()?);
            }
        }
        self.shut_down(terminal);
        Ok(())
    }

    /// How long to wait for input before drawing again.
    ///
    /// Every redraw used to cost a full frame at ten a second, whether or not
    /// anything on screen could have changed. Nothing here changes on its own
    /// except on a schedule this loop owns — the half-second data refresh and,
    /// while something is animating, the quarter-second animation tick — so the
    /// wait runs to whichever of those comes first. Input is never delayed by
    /// it: a keypress ends the wait early.
    ///
    /// A still surface therefore idles at the refresh rate rather than at the
    /// input-poll rate, and an animated one draws exactly the frames the
    /// animation has.
    fn idle_for(&self) -> Duration {
        let until_refresh = REFRESH_INTERVAL.saturating_sub(self.last_refresh.elapsed());
        let wait = if self.state.motion_frame().is_animating() {
            until_refresh.min(motion::TICK)
        } else {
            until_refresh
        };
        // Never busy-spin, and never wait so long that a resize or keypress
        // feels laggy on a surface with no clock of its own.
        wait.clamp(EVENT_POLL, REFRESH_INTERVAL)
    }

    /// Stops what this session started before the session goes away.
    ///
    /// Quitting used to leave every running agent behind: the interface closed,
    /// the run stayed `Running` in the store, and the process group kept the
    /// machine's memory until someone found it. Nothing else ever came back for
    /// them, because the next session resumes runs from the store and has no
    /// way to know the old operating-system processes are still there.
    ///
    /// The wait is real — a stop climbs the termination ladder, and several
    /// runs climb it one after another — so the interface says what it is doing
    /// rather than appearing to hang. Failures are shown and then dropped:
    /// quitting must not be the one action a user cannot complete.
    fn shut_down(&mut self, terminal: &mut TerminalSession) {
        let running: Vec<RunId> = self
            .state
            .runs
            .iter()
            .filter(|run| run.status == RunStatus::Running)
            .map(|run| run.id)
            .collect();
        if running.is_empty() {
            return;
        }
        self.state.notify(
            UiMessageKind::Info,
            format!(
                "Stopping {} agent{}…",
                running.len(),
                if running.len() == 1 { "" } else { "s" }
            ),
        );
        let _ = terminal
            .terminal_mut()
            .draw(|frame| render::render(frame, &self.state));
        for run_id in running {
            if let Err(error) = self.reader.stop_run(run_id) {
                tracing::warn!(%run_id, %error, "could not stop run during shutdown");
            }
        }
    }

    /// Absorbs the background check without ever blocking, then opens the
    /// prompt only when the interface has nothing more important to say.
    fn receive_update(&mut self) {
        match self.update.try_recv() {
            Ok((info, source)) => {
                self.state.update = info;
                self.state.update_install = source;
            }
            // Empty means still checking; disconnected means the check
            // finished or failed and will never speak again. Neither is worth
            // telling the user about.
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        }
        if self.state.update_prompt_is_due() {
            self.state.overlay = Some(Overlay::Update);
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                let text_mode = self.state.screen == Screen::NewRun
                    && matches!(self.state.new_run.focus, 0 | 2)
                    || matches!(
                        self.state.overlay,
                        Some(Overlay::Attention | Overlay::Continue | Overlay::ConsulChat)
                    );
                let intent = if text_mode {
                    map_text_key(key)
                } else {
                    map_key(key)
                };
                if key.kind == KeyEventKind::Repeat && !input::repeats(intent) {
                    return;
                }
                self.handle_intent(intent);
            }
            Event::Paste(text) => self.handle_paste(&text),
            Event::Key(_)
            | Event::Resize(_, _)
            | Event::FocusGained
            | Event::FocusLost
            | Event::Mouse(_) => {}
        }
    }

    fn handle_intent(&mut self, intent: Intent) {
        if intent == Intent::Quit {
            self.state.quit = true;
            return;
        }
        if let Some(overlay) = self.state.overlay {
            self.handle_overlay_intent(overlay, intent);
            return;
        }
        if self.state.screen == Screen::NewRun {
            self.handle_new_run_intent(intent);
            return;
        }
        if matches!(self.state.screen, Screen::Missions | Screen::MissionDetail) {
            self.handle_mission_intent(intent);
            return;
        }
        match intent {
            Intent::Up => self.move_selection(false),
            Intent::Down => self.move_selection(true),
            Intent::PageUp => self.state.scroll = self.state.scroll.saturating_sub(10),
            Intent::PageDown => self.state.scroll = self.state.scroll.saturating_add(10),
            Intent::Home if is_viewer(self.state.screen) => self.state.scroll = 0,
            Intent::End if is_viewer(self.state.screen) => self.state.scroll = u16::MAX,
            Intent::Enter | Intent::Right if self.state.screen == Screen::Runs => {
                self.open_selected_run();
            }
            Intent::Missions => self.show_missions(),
            // Enter on a stage opens its primary result; open_artifact keeps
            // expected absence informational, so this never mutates anything.
            Intent::Enter | Intent::Right if self.state.screen == Screen::RunDetail => {
                self.open_artifact();
            }
            // The arrows walk the same ladder Enter and Esc already walk, one
            // rung per press: → goes into whatever the cursor is on, ← comes
            // back out. Nothing on these screens edits text, so the arrows are
            // free; the composer and the overlays take theirs first, above.
            Intent::Escape | Intent::Left => self.back(),
            Intent::NewRun => {
                self.state.screen = Screen::NewRun;
                self.state.overlay = None;
                self.state.message = None;
                self.state.run_opened_from_mission = false;
            }
            Intent::Runs => {
                self.state.screen = Screen::Runs;
                self.state.overlay = None;
                self.state.scroll = 0;
                self.state.run_opened_from_mission = false;
            }
            Intent::Resume => self.resume(),
            Intent::Stop => self.stop(),
            Intent::Retry => self.retry(),
            Intent::Attention => self.open_attention(),
            Intent::Artifact => self.open_artifact(),
            Intent::Logs => self.open_logs(),
            Intent::Diff => self.open_diff(),
            Intent::Apply => self.open_apply_confirmation(),
            Intent::Rebase => self.open_rebase_confirmation(),
            Intent::Publish => self.open_publish_confirmation(),
            Intent::Fix => self.open_next_cycle(),
            Intent::Continue => self.open_continue(),
            Intent::FollowUps => self.open_follow_ups(),
            Intent::Discard => self.state.overlay = Some(Overlay::DiscardConfirm),
            Intent::Archive if self.state.screen == Screen::Runs => {
                self.toggle_selected_archived();
            }
            Intent::AutoApprove if self.state.screen == Screen::RunDetail => {
                self.toggle_auto_approve();
            }
            Intent::ShowArchived if self.state.screen == Screen::Runs => {
                self.state.show_archived = !self.state.show_archived;
                self.refresh();
            }
            Intent::DeleteForever if self.state.screen == Screen::Runs => {
                self.open_delete_confirmation();
            }
            Intent::DismissMessage => self.state.dismiss_message(),
            Intent::ToggleRaw if self.state.screen == Screen::Artifact => {
                self.state.artifact_raw = !self.state.artifact_raw;
            }
            Intent::ExpandTask if self.state.screen == Screen::RunDetail => {
                self.state.task_expanded = !self.state.task_expanded;
            }
            Intent::TechnicalDetails if self.state.screen == Screen::RunDetail => {
                self.state.technical = !self.state.technical;
                self.refresh_evidence();
            }
            Intent::Help => self.state.overlay = Some(Overlay::Help),
            _ => {}
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one dispatch table for overlay intents"
    )]
    fn handle_overlay_intent(&mut self, overlay: Overlay, intent: Intent) {
        if overlay == Overlay::Update {
            self.handle_update_intent(intent);
            return;
        }
        if overlay == Overlay::StartFailed {
            if matches!(intent, Intent::Enter | Intent::Escape) {
                self.close_start_failure();
            }
            return;
        }
        if overlay == Overlay::Published && intent == Intent::Escape {
            // Esc closes the card the way Enter does: the summary still
            // lands in the footer, so leaving is never losing the URL.
            self.handle_published_intent(Intent::Enter);
            return;
        }
        if matches!(intent, Intent::Escape | Intent::Help) {
            match overlay {
                Overlay::ConsulChat => self.close_consul_chat(),
                Overlay::PlanConfirm if self.state.lead_plan_pending_ticket.is_some() => {
                    self.state.overlay = None;
                }
                Overlay::PlanConfirm => {
                    self.state.overlay = None;
                    self.state.lead_plan_preview = None;
                    self.state.lead_plan_scroll = 0;
                }
                _ => self.state.overlay = None,
            }
            return;
        }
        match overlay {
            Overlay::Attention => self.handle_attention_intent(intent),
            Overlay::ConsulChat => self.handle_consul_chat_intent(intent),
            Overlay::PlanConfirm => self.handle_plan_confirm_intent(intent),
            Overlay::Continue => self.handle_continue_intent(intent),
            Overlay::FollowUps => self.handle_follow_ups_intent(intent),
            Overlay::NextCycle => self.handle_next_cycle_intent(intent),
            Overlay::RetryRoute => self.handle_retry_route_intent(intent),
            // The confirmation lists the files; `d` opens the whole diff.
            Overlay::ApplyConfirm | Overlay::PublishConfirm if intent == Intent::Diff => {
                self.state.overlay = None;
                self.open_diff();
            }
            Overlay::ApplyConfirm if intent == Intent::Enter => {
                if let Some(run_id) = self.state.selected_run {
                    self.dispatch(WorkerCommand::ApplyRun { run_id });
                    self.state.overlay = None;
                }
            }
            Overlay::IntegrateConfirm if intent == Intent::Enter => {
                if let (Some(mission_id), Some(package_id)) = (
                    self.state.selected_mission,
                    self.state.selected_package.clone(),
                ) {
                    self.dispatch(WorkerCommand::IntegratePackage {
                        mission_id,
                        package_id,
                    });
                    self.state.overlay = None;
                }
            }
            Overlay::RebaseConfirm if intent == Intent::Enter => {
                if let Some(run_id) = self.state.selected_run {
                    self.dispatch(WorkerCommand::RebaseRun { run_id });
                    self.state.overlay = None;
                }
            }
            Overlay::PublishConfirm if intent == Intent::Enter => {
                if let Some(run_id) = self.state.selected_run {
                    self.state.overlay = None;
                    if self.dispatch(WorkerCommand::PublishRun { run_id }) {
                        self.state.publishing = Some(PublishInFlight {
                            run_id,
                            started: Instant::now(),
                        });
                        self.state.published = None;
                        self.state.overlay = Some(Overlay::Publishing);
                    }
                }
            }
            Overlay::Published => self.handle_published_intent(intent),
            Overlay::DiscardConfirm if intent == Intent::Enter => {
                if let Some(run_id) = self.state.selected_run {
                    self.dispatch(WorkerCommand::DiscardRun { run_id });
                    self.state.overlay = None;
                }
            }
            Overlay::DeleteConfirm if intent == Intent::DeleteForever => {
                if let Some(run_id) = self.state.selected_run {
                    self.dispatch(WorkerCommand::PurgeRun { run_id });
                    self.state.overlay = None;
                }
            }
            Overlay::Help
            | Overlay::ApplyConfirm
            | Overlay::IntegrateConfirm
            | Overlay::RebaseConfirm
            | Overlay::PublishConfirm
            | Overlay::DiscardConfirm
            | Overlay::DeleteConfirm
            | Overlay::Publishing
            | Overlay::Starting
            | Overlay::StartFailed
            | Overlay::Update => {}
        }
    }

    /// The published card: `o` opens the pull request, `y` copies its URL,
    /// Enter closes. Closing leaves the summary in the footer so the URL is
    /// still on screen for a moment after the card is gone.
    fn handle_published_intent(&mut self, intent: Intent) {
        let Some(outcome) = self.state.published.clone() else {
            self.state.overlay = None;
            return;
        };
        match intent {
            Intent::Enter => {
                self.state.overlay = None;
                if outcome.is_failure() {
                    self.state.set_error(outcome.summary());
                } else {
                    self.state.set_message(outcome.summary());
                }
            }
            Intent::Artifact => {
                let Some(url) = outcome.url() else {
                    return;
                };
                match desktop::open_in_browser(url) {
                    Ok(()) => self.state.set_message(format!("Opened {url}")),
                    Err(reason) => self
                        .state
                        .set_error(format!("Could not open the browser: {reason}")),
                }
            }
            Intent::CopyUrl => {
                let Some(url) = outcome.url() else {
                    return;
                };
                match desktop::copy_to_clipboard(url) {
                    Ok(()) => self.state.set_message("Pull request URL copied."),
                    Err(reason) => self
                        .state
                        .set_error(format!("Could not copy the URL: {reason}")),
                }
            }
            _ => {}
        }
    }

    /// Hands a finished publish to the card when the operator is free to
    /// look at it, or to the footer when something else has the screen.
    ///
    /// A card over another overlay would tear the operator away from an
    /// answer they are typing; a card over the new-run form would cover the
    /// task they are writing. In both cases the footer carries the result and
    /// the card is skipped rather than queued.
    fn present_publish(&mut self, outcome: PublishOutcome) {
        let run_id = match &outcome {
            PublishOutcome::Pushed { run_id, .. } | PublishOutcome::Failed { run_id, .. } => {
                *run_id
            }
        };
        if self.state.publishing.is_some_and(|p| p.run_id == run_id) {
            self.state.publishing = None;
        }
        let free = matches!(self.state.overlay, None | Some(Overlay::Publishing))
            && self.state.screen != Screen::NewRun;
        if free {
            self.state.published = Some(outcome);
            self.state.overlay = Some(Overlay::Published);
            self.state.message = None;
        } else if outcome.is_failure() {
            self.state.set_error(outcome.summary());
        } else {
            self.state.set_message(outcome.summary());
        }
    }

    /// Any answer closes the prompt for the lifetime of this process; the
    /// 24-hour cache decides whether the next process asks again. Only an
    /// explicit Yes on an installation The Senate owns starts an install.
    fn handle_update_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Up | Intent::Down => {
                if self.state.update_is_installable() {
                    self.state.update_install_selected = !self.state.update_install_selected;
                }
            }
            Intent::Enter => {
                let install =
                    self.state.update_is_installable() && self.state.update_install_selected;
                self.state.update_dismissed = true;
                self.state.overlay = None;
                if install {
                    self.begin_update_install();
                }
            }
            Intent::Escape | Intent::Help => {
                self.state.update_dismissed = true;
                self.state.overlay = None;
            }
            _ => {}
        }
    }

    /// Runs the installation off the render loop so the interface stays
    /// responsive, and reports its outcome as an ordinary notification.
    fn begin_update_install(&mut self) {
        let Some(info) = self.state.update.clone() else {
            return;
        };
        self.state
            .notify(UiMessageKind::Info, "Installing update…".to_owned());
        self.installing = Some(spawn_update_install(info));
    }

    /// Absorbs an installation result without ever blocking the loop.
    fn receive_install(&mut self) {
        let Some(receiver) = self.installing.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(notice)) => {
                self.state.set_message(notice);
                self.installing = None;
            }
            Ok(Err(error)) => {
                // A failed install is recoverable: the existing executable is
                // untouched and the interface carries on normally.
                self.state
                    .set_error(format!("Update not installed: {error}"));
                self.installing = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.installing = None,
        }
    }

    fn handle_attention_intent(&mut self, intent: Intent) {
        let length = self
            .state
            .details
            .as_ref()
            .map_or(0, |details| details.attention.len());
        match intent {
            Intent::Up => self.state.attention_index = self.state.attention_index.saturating_sub(1),
            Intent::Down if length > 0 => {
                self.state.attention_index = (self.state.attention_index + 1).min(length - 1);
            }
            Intent::Left => self.state.attention_response.left(),
            Intent::Right => self.state.attention_response.right(),
            Intent::Home => self.state.attention_response.home(),
            Intent::End => self.state.attention_response.end(),
            Intent::Backspace => self.state.attention_response.backspace(),
            Intent::DeleteToStart => self.state.attention_response.delete_to_start(),
            Intent::DeleteToEnd => self.state.attention_response.delete_to_end(),
            Intent::DeleteWordBefore => self.state.attention_response.delete_word_before(),
            Intent::Delete => self.state.attention_response.delete(),
            Intent::Character(character) => self.state.attention_response.insert(character),
            Intent::Enter => {
                let response = (!self.state.attention_response.text().is_empty())
                    .then(|| self.state.attention_response.text().to_owned());
                self.submit_attention(response);
            }
            Intent::Skip => {
                self.submit_attention(Some(crate::app::SKIP_ATTENTION_RESPONSE.to_owned()));
            }
            _ => {}
        }
    }

    /// Opens the Consul composer for the selected Mission without taking the
    /// Mission identity from a later selection or worker response.
    fn open_consul_chat(&mut self) {
        let Some(mission_id) = self.state.selected_mission else {
            return;
        };
        if self
            .state
            .mission
            .as_ref()
            .is_none_or(|mission| mission.id != mission_id)
        {
            self.state.notify(
                UiMessageKind::Info,
                "Wait for the selected Mission to load before messaging its Consul.".to_owned(),
            );
            return;
        }
        self.state.consul_drafts.entry(mission_id).or_default();
        self.state.consul_chat_mission = Some(mission_id);
        self.state.overlay = Some(Overlay::ConsulChat);
    }

    /// Leaves the draft in its Mission-scoped slot so opening another
    /// campaign never inherits these words.
    fn close_consul_chat(&mut self) {
        self.state.overlay = None;
        self.state.consul_chat_mission = None;
    }

    fn handle_consul_chat_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Left => self.edit_consul_draft(super::state::TextField::left),
            Intent::Right => self.edit_consul_draft(super::state::TextField::right),
            Intent::Home => self.edit_consul_draft(super::state::TextField::home),
            Intent::End => self.edit_consul_draft(super::state::TextField::end),
            Intent::Backspace => self.edit_consul_draft(super::state::TextField::backspace),
            Intent::Delete => self.edit_consul_draft(super::state::TextField::delete),
            Intent::DeleteToStart => {
                self.edit_consul_draft(super::state::TextField::delete_to_start);
            }
            Intent::DeleteToEnd => self.edit_consul_draft(super::state::TextField::delete_to_end),
            Intent::DeleteWordBefore => {
                self.edit_consul_draft(super::state::TextField::delete_word_before);
            }
            Intent::Character(character) => {
                self.edit_consul_draft(|field| field.insert(character));
            }
            Intent::Enter => self.submit_consul_message(),
            _ => {}
        }
    }

    fn consul_draft_mut(&mut self) -> Option<&mut super::state::TextField> {
        let mission_id = self.state.consul_chat_mission?;
        Some(self.state.consul_drafts.entry(mission_id).or_default())
    }

    fn edit_consul_draft(&mut self, edit: impl FnOnce(&mut super::state::TextField)) {
        if let Some(field) = self.consul_draft_mut() {
            edit(field);
        }
    }

    /// Sends one Mission-scoped Consul turn to the worker. A durable lead
    /// awaiting the operator must be resumed through its run attention first.
    fn submit_consul_message(&mut self) {
        let Some(mission_id) = self.state.consul_chat_mission else {
            return;
        };
        if self.state.consul_pending.contains_key(&mission_id) {
            self.state.notify(
                UiMessageKind::Info,
                format!("The Consul for {mission_id} is already conferring."),
            );
            return;
        }
        let Some(draft) = self.state.consul_drafts.get(&mission_id) else {
            return;
        };
        let raw_text = draft.text().to_owned();
        let message = raw_text.trim().to_owned();
        if message.is_empty() {
            self.state.set_error("Message cannot be empty");
            return;
        }
        if let Some(lead) = self
            .state
            .mission
            .as_ref()
            .filter(|mission| mission.id == mission_id)
            .and_then(|mission| mission.lead.as_ref())
            .filter(|lead| !lead.run_status.is_execution_finished())
        {
            let reason = if lead.run_status == RunStatus::NeedsUser {
                format!(
                    "The Consul is waiting on you in {} — use `u` to open its attention first.",
                    lead.run_id
                )
            } else if matches!(lead.run_status, RunStatus::Paused | RunStatus::Interrupted) {
                format!(
                    "The Consul is {:?} in {} — open that run and resume it before sending another message.",
                    lead.run_status, lead.run_id
                )
            } else {
                format!(
                    "The Consul is {:?} in {} — wait for that turn to finish.",
                    lead.run_status, lead.run_id
                )
            };
            self.state.set_error(reason);
            return;
        }

        let command = WorkerCommand::AskLead {
            mission_id,
            message,
            selection: self.state.new_run.execution.selection(),
            effort: self.state.new_run.effort.into(),
        };
        if let Some(ticket) = self.dispatch_ticketed(command) {
            self.state.consul_pending.insert(
                mission_id,
                ConsulAskInFlight {
                    ticket,
                    sent_text: raw_text,
                },
            );
        }
    }

    /// Reads a proposal preview now, then leaves it pinned in state for the
    /// confirmation card and the eventual freshness check in the service.
    fn open_lead_plan_preview(&mut self) {
        if self.state.lead_plan_pending_ticket.is_some() {
            self.state.notify(
                UiMessageKind::Info,
                "Plan approval is in progress; wait for its result.".to_owned(),
            );
            return;
        }
        let Some(mission_id) = self.state.selected_mission else {
            return;
        };
        if self.state.consul_pending.contains_key(&mission_id) {
            self.state.notify(
                UiMessageKind::Info,
                "Wait for the current Consul turn before previewing Plan changes.".to_owned(),
            );
            return;
        }
        self.state.lead_plan_preview = None;
        self.state.lead_plan_scroll = 0;
        match self.missions.preview_lead_proposals(mission_id) {
            Ok(preview) if preview.mission_id == mission_id => {
                self.state.lead_plan_preview = Some(preview);
                self.state.lead_plan_scroll = 0;
                self.state.lead_plan_pending_ticket = None;
                self.state.overlay = Some(Overlay::PlanConfirm);
            }
            Ok(_) => self.state.set_error(
                "The Consul preview belongs to another Mission; reload before approving."
                    .to_owned(),
            ),
            Err(error) => self
                .state
                .set_error(format!("Could not preview Plan changes: {error}")),
        }
    }

    fn handle_plan_confirm_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Up => {
                self.state.lead_plan_scroll = self.state.lead_plan_scroll.saturating_sub(1);
            }
            Intent::Down => {
                self.state.lead_plan_scroll = self.state.lead_plan_scroll.saturating_add(1);
            }
            Intent::PageUp => {
                self.state.lead_plan_scroll = self.state.lead_plan_scroll.saturating_sub(10);
            }
            Intent::PageDown => {
                self.state.lead_plan_scroll = self.state.lead_plan_scroll.saturating_add(10);
            }
            Intent::Home => self.state.lead_plan_scroll = 0,
            Intent::Enter => self.approve_lead_plan_preview(),
            _ => {}
        }
    }

    fn approve_lead_plan_preview(&mut self) {
        if self.state.lead_plan_pending_ticket.is_some() {
            return;
        }
        let Some(preview) = self.state.lead_plan_preview.clone() else {
            self.state
                .set_error("There is no pinned Plan preview to approve");
            self.state.overlay = None;
            return;
        };
        if let Some(ticket) = self.dispatch_ticketed(WorkerCommand::ApplyLeadPlan {
            preview: Box::new(preview),
        }) {
            self.state.lead_plan_pending_ticket = Some(ticket);
        }
    }

    /// Resolves the selected attention with `response` (`None` approves a
    /// permission as asked) and closes the overlay.
    fn submit_attention(&mut self, response: Option<String>) {
        let Some(run_id) = self.state.selected_run else {
            return;
        };
        let Some(attention_id) = self.state.selected_attention_id() else {
            self.state
                .set_error("No pending attention request selected");
            return;
        };
        self.dispatch(WorkerCommand::ResolveAttention {
            run_id,
            attention_id,
            response,
        });
        self.state.overlay = None;
    }

    fn handle_new_run_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Escape => self.back(),
            Intent::Tab => self.state.new_run.next_field(),
            Intent::BackTab => self.state.new_run.previous_field(),
            Intent::Left if matches!(self.state.new_run.focus, 1 | 3 | 4) => {
                self.state.new_run.cycle_value(false);
            }
            Intent::Right if matches!(self.state.new_run.focus, 1 | 3 | 4) => {
                self.state.new_run.cycle_value(true);
            }
            Intent::Left => self.edit_text(super::state::TextField::left),
            Intent::Right => self.edit_text(super::state::TextField::right),
            Intent::Home => self.edit_text(super::state::TextField::home),
            Intent::End => self.edit_text(super::state::TextField::end),
            Intent::Backspace => self.edit_text(super::state::TextField::backspace),
            Intent::DeleteToStart => self.edit_text(super::state::TextField::delete_to_start),
            Intent::DeleteToEnd => self.edit_text(super::state::TextField::delete_to_end),
            Intent::DeleteWordBefore => {
                self.edit_text(super::state::TextField::delete_word_before);
            }
            Intent::Delete => self.edit_text(super::state::TextField::delete),
            Intent::Character(character) => self.edit_text(|field| field.insert(character)),
            Intent::Enter => self.submit_new_run(),
            _ => {}
        }
    }

    fn edit_text(&mut self, edit: impl FnOnce(&mut super::state::TextField)) {
        if let Some(field) = self.state.new_run.active_text_mut() {
            edit(field);
        }
    }

    /// Pastes into whatever field has the operator's attention. An open
    /// overlay has it even when it takes no text: the form behind a card is
    /// not where a paste belongs.
    fn handle_paste(&mut self, text: &str) {
        match self.state.overlay {
            Some(Overlay::Attention) => self.state.attention_response.paste(text),
            Some(Overlay::Continue) => self.state.continue_instruction.paste(text),
            Some(Overlay::ConsulChat) => {
                if let Some(mission_id) = self.state.consul_chat_mission {
                    self.state
                        .consul_drafts
                        .entry(mission_id)
                        .or_default()
                        .paste(text);
                }
            }
            None if self.state.screen == Screen::NewRun => {
                self.edit_text(|field| field.paste(text));
            }
            Some(_) | None => {}
        }
    }

    fn submit_new_run(&mut self) {
        let task = self.state.new_run.task.text().trim().to_owned();
        let repository = self.state.new_run.repository.text().trim().to_owned();
        if task.is_empty() {
            self.state.set_error("Task cannot be empty");
            return;
        }
        if repository.is_empty() {
            self.state.set_error("Repository cannot be empty");
            return;
        }
        if let Some(ticket) = self.dispatch_ticketed(WorkerCommand::StartRun {
            workflow: self.state.new_run.workflow,
            task: task.clone(),
            repository: PathBuf::from(repository),
            selection: self.state.new_run.execution.selection(),
            effort: self.state.new_run.effort.into(),
        }) {
            let draft = task.clone();
            self.state.starting = Some(StartInFlight {
                ticket,
                task,
                started: Instant::now(),
                progress: None,
            });
            self.state.start_failure = None;
            self.state.overlay = Some(Overlay::Starting);
            // Held until the run exists, so a start refused before then can
            // give the words back.
            self.state.start_drafts.push((ticket, draft));
            // The task belongs to the run that was just dispatched; leaving it
            // in the composer makes the next run open on another run's words.
            // Repository, workflow, execution and effort stay: those are the
            // operator's standing preferences, not this run's content.
            self.state.new_run.task = super::state::TextField::default();
            self.state.new_run.focus = 0;
            self.state.screen = Screen::Runs;
        }
    }

    /// Opens the selected run's detail screen, landing the cursor on whatever
    /// is blocking it.
    fn open_selected_run(&mut self) {
        if self.state.selected_run.is_some() {
            self.state.run_opened_from_mission = false;
            self.state.screen = Screen::RunDetail;
            self.refresh_selected();
            self.state.focus_blocking_failure();
        }
    }

    fn move_selection(&mut self, forward: bool) {
        match self.state.screen {
            Screen::Runs => {
                self.state.move_run(forward);
                self.refresh_selected();
            }
            Screen::RunDetail => self.state.move_stage(forward),
            Screen::Artifact | Screen::Logs | Screen::Diff => {
                self.state.scroll = if forward {
                    self.state.scroll.saturating_add(1)
                } else {
                    self.state.scroll.saturating_sub(1)
                };
            }
            Screen::NewRun => {}
            Screen::Missions => {
                self.state.move_mission(forward);
                self.refresh_selected_mission();
            }
            Screen::MissionDetail => self.state.move_package(forward),
        }
    }

    fn back(&mut self) {
        self.state.overlay = None;
        self.state.scroll = 0;
        self.state.screen = match self.state.screen {
            Screen::RunDetail if self.state.run_opened_from_mission => {
                self.state.run_opened_from_mission = false;
                self.refresh_selected_mission();
                Screen::MissionDetail
            }
            Screen::Runs | Screen::RunDetail | Screen::NewRun | Screen::Missions => {
                self.state.run_opened_from_mission = false;
                Screen::Runs
            }
            Screen::Artifact | Screen::Logs | Screen::Diff => Screen::RunDetail,
            Screen::MissionDetail => Screen::Missions,
        };
    }

    /// The mission screens take only the keys that mean something there.
    /// Run actions (`s`, `a`, `X`, …) are not offered: the selected run is
    /// whatever was last opened, which is not necessarily the package's.
    fn handle_mission_intent(&mut self, intent: Intent) {
        let detail = self.state.screen == Screen::MissionDetail;
        match intent {
            Intent::Up => self.move_selection(false),
            Intent::Down => self.move_selection(true),
            Intent::Enter | Intent::Right if detail => self.open_selected_package_run(),
            Intent::Enter | Intent::Right => self.open_selected_mission(),
            Intent::Escape | Intent::Left => self.back(),
            Intent::NewRun => {
                self.state.screen = Screen::NewRun;
                self.state.overlay = None;
                self.state.message = None;
            }
            Intent::Runs => {
                self.state.screen = Screen::Runs;
                self.state.overlay = None;
                self.state.scroll = 0;
            }
            Intent::Missions => self.show_missions(),
            Intent::StartPackage if detail => self.start_selected_package(),
            Intent::Integrate if detail => self.open_integrate_confirmation(),
            Intent::Attention if detail => self.open_package_attention(),
            Intent::Consul if detail => self.open_consul_chat(),
            Intent::PreviewPlan if detail => self.open_lead_plan_preview(),
            Intent::AutoApprove if detail => self.toggle_mission_auto_approve(),
            Intent::EnterSenate => self.enter_senate(),
            Intent::DismissMessage => self.state.dismiss_message(),
            Intent::Help => self.state.overlay = Some(Overlay::Help),
            _ => {}
        }
    }

    /// `W`: open the selected campaign in the Senate, the optional 3D view.
    /// It reads the same mission state this screen shows and changes none.
    fn enter_senate(&mut self) {
        match super::desktop::enter_senate(self.state.selected_mission) {
            Ok(()) => self.state.notify(
                UiMessageKind::Info,
                "Opening the Senate in your browser. Agents keep working if you close it."
                    .to_owned(),
            ),
            Err(reason) => self.state.notify(
                UiMessageKind::Error,
                format!("Could not open the Senate: {reason}"),
            ),
        }
    }

    fn show_missions(&mut self) {
        self.state.screen = Screen::Missions;
        self.state.overlay = None;
        self.state.scroll = 0;
        self.state.run_opened_from_mission = false;
        self.refresh_missions();
    }

    /// `u` on a package: answer its run's pending request without leaving
    /// the mission. The overlay reads the selected run's details, so the
    /// package's run becomes the selected run first.
    fn open_package_attention(&mut self) {
        if let Some(package) = self.state.selected_package_summary()
            && package.status == WorkPackageStatus::Blocked
            && let Some(run_id) = package.current_run
        {
            let title = package.title.clone();
            self.state.selected_run = Some(run_id);
            self.refresh_selected();
            if self
                .state
                .details
                .as_ref()
                .is_some_and(|details| !details.attention.is_empty())
            {
                self.open_attention();
                return;
            }
            // A blocked package can also mean its dependency is waiting; in
            // that case the selected run has no question to answer. Give the
            // lead's own attention a chance before reporting no work.
            if self.lead_needs_user() {
                self.open_lead_attention();
                return;
            }
            self.state.notify(
                UiMessageKind::Info,
                format!("{title} has no pending attention request."),
            );
            return;
        }
        if self.lead_needs_user() {
            self.open_lead_attention();
            return;
        }
        if let Some(package) = self.state.selected_package_summary() {
            if package.current_run.is_some() {
                self.state.notify(
                    UiMessageKind::Info,
                    format!("{} is not waiting on you right now.", package.title),
                );
            } else {
                self.state.notify(
                    UiMessageKind::Info,
                    format!("{} has no run yet, so nothing is asking.", package.title),
                );
            }
        }
    }

    fn lead_needs_user(&self) -> bool {
        self.state
            .mission
            .as_ref()
            .and_then(|mission| mission.lead.as_ref())
            .is_some_and(|lead| lead.run_status == RunStatus::NeedsUser)
    }

    fn open_lead_attention(&mut self) {
        let Some(lead_run_id) = self
            .state
            .mission
            .as_ref()
            .and_then(|mission| mission.lead.as_ref())
            .map(|lead| lead.run_id)
        else {
            return;
        };
        self.state.selected_run = Some(lead_run_id);
        self.refresh_selected();
        self.open_attention();
    }

    /// `A` on a mission: approve grantable permission requests for every
    /// run serving its packages, now and for runs this session starts.
    fn toggle_mission_auto_approve(&mut self) {
        let Some(mission) = self.state.mission.as_ref() else {
            return;
        };
        let mission_id = mission.id;
        let arm = !self.state.auto_approve_missions.contains(&mission_id);
        let live: Vec<RunId> = mission
            .packages
            .iter()
            .filter(|package| package.status.has_active_run())
            .filter_map(|package| package.current_run)
            .collect();
        for run_id in &live {
            if let Err(error) = self.reader.set_run_auto_approve(*run_id, arm) {
                self.state.set_error(error.to_string());
                return;
            }
        }
        if arm {
            self.state.auto_approve_missions.insert(mission_id);
            self.state.set_message(
                "Auto-approve on — grantable permission requests are approved for this mission's runs. Questions still stop them.".to_owned(),
            );
            // A run already waiting on a request it may now approve.
            for run_id in live {
                self.dispatch(WorkerCommand::ResumeRun { run_id });
            }
        } else {
            self.state.auto_approve_missions.remove(&mission_id);
            self.state
                .set_message("Auto-approve off — permission requests wait for you.".to_owned());
        }
        self.refresh_selected_mission();
    }

    fn open_selected_mission(&mut self) {
        if self.state.selected_mission.is_some() {
            self.state.screen = Screen::MissionDetail;
            self.refresh_selected_mission();
        }
    }

    /// Enter on a package opens its current run: the run is one level down
    /// from the package, exactly as a stage's artifact is from the run.
    fn open_selected_package_run(&mut self) {
        let Some(package) = self.state.selected_package_summary() else {
            return;
        };
        let Some(run_id) = package.current_run else {
            self.state.notify(
                UiMessageKind::Info,
                format!("No run has served {} yet — S starts one.", package.id),
            );
            return;
        };
        self.state.selected_run = Some(run_id);
        self.state.run_opened_from_mission = true;
        self.state.screen = Screen::RunDetail;
        self.refresh_selected();
        self.state.focus_blocking_failure();
    }

    /// Starts the selected package's child run with the composer's current
    /// execution and effort choices, so `n` and `S` mean the same provider.
    fn start_selected_package(&mut self) {
        let Some(mission_id) = self.state.selected_mission else {
            return;
        };
        let Some(package) = self.state.selected_package_summary() else {
            return;
        };
        if package.status != WorkPackageStatus::Ready {
            self.state.notify(
                UiMessageKind::Info,
                start_package_unavailable_reason(package.status, &package.id),
            );
            return;
        }
        let package_id = package.id.clone();
        let title = package.title.clone();
        if let Some(ticket) = self.dispatch_ticketed(WorkerCommand::StartPackage {
            mission_id,
            package_id,
            selection: self.state.new_run.execution.selection(),
            effort: self.state.new_run.effort.into(),
            auto_approve: self.state.auto_approve_missions.contains(&mission_id),
        }) {
            self.state.starting = Some(StartInFlight {
                ticket,
                task: title,
                started: Instant::now(),
                progress: None,
            });
            self.state.start_failure = None;
            self.state.overlay = Some(Overlay::Starting);
        }
    }

    fn open_integrate_confirmation(&mut self) {
        let Some(package) = self.state.selected_package_summary() else {
            return;
        };
        if package.status != WorkPackageStatus::Delivered {
            self.state.notify(
                UiMessageKind::Info,
                format!(
                    "{} is {}; only a finished package can be brought in.",
                    package.id,
                    package_status_word(package.status)
                ),
            );
            return;
        }
        self.state.overlay = Some(Overlay::IntegrateConfirm);
    }

    fn refresh_missions(&mut self) {
        match self.missions.list_missions() {
            Ok(missions) => self.state.replace_missions(missions),
            Err(error) => self.state.set_error(error.to_string()),
        }
        self.refresh_selected_mission();
    }

    fn refresh_selected_mission(&mut self) {
        let Some(mission_id) = self.state.selected_mission else {
            self.state.mission = None;
            self.state.lead_answer = None;
            return;
        };
        match self.missions.inspect_mission(mission_id) {
            Ok(details) => {
                self.state.replace_mission(details);
                self.refresh_lead_answer(mission_id);
            }
            Err(error) => {
                if self
                    .state
                    .mission
                    .as_ref()
                    .is_some_and(|mission| mission.id != mission_id)
                {
                    self.state.mission = None;
                    self.state.lead_answer = None;
                }
                self.state.set_error(error.to_string());
            }
        }
    }

    /// The Consul pane reads the lead's latest finished answer, current as
    /// of the last refresh. A read error is reported like any other; it
    /// never hides the mission that just refreshed successfully.
    fn refresh_lead_answer(&mut self, mission_id: MissionId) {
        match self.missions.latest_lead_answer(mission_id) {
            Ok(answer) => self.state.lead_answer = answer,
            Err(error) => {
                self.state.lead_answer = None;
                self.state.set_error(error.to_string());
            }
        }
    }

    fn resume(&mut self) {
        if let Some(run_id) = self.state.selected_run {
            self.dispatch(WorkerCommand::ResumeRun { run_id });
        }
    }

    /// Stop is non-destructive — it preserves the run, its workspace, and its
    /// results — so it dispatches directly rather than through a confirmation
    /// overlay, which this interface reserves for apply and discard. States the
    /// domain cannot interrupt are refused here rather than dispatched.
    fn stop(&mut self) {
        let Some(run_id) = self.state.selected_run else {
            return;
        };
        if !self.state.run_is_stoppable() {
            self.state
                .notify(UiMessageKind::Info, stop_unavailable_reason(&self.state));
            return;
        }
        self.dispatch(WorkerCommand::StopRun { run_id });
    }

    /// Opens the `[t]` chooser for the selected failed stage. Retrying is
    /// two keys rather than one because the question "where?" is the one
    /// worth asking when a provider just ran out of quota: the stage's
    /// configured provider is the first row, so Enter alone keeps today's
    /// behaviour.
    fn retry(&mut self) {
        if self.state.selected_run.is_none() {
            return;
        }
        let Some(details) = self.state.details.as_ref() else {
            return;
        };
        let Some(stage) = details.stages.get(self.state.selected_stage_index) else {
            return;
        };
        if stage.status != StageStatus::Failed {
            self.state.set_error("Selected stage is not failed");
            return;
        }
        // A plain retry of a stage whose model Codex refused fails the same
        // way, so the fallback row starts highlighted when there is one.
        self.state.retry_route_choice = stage
            .model_fallback()
            .map_or(RetryRouteChoice::Configured, RetryRouteChoice::CodexModel);
        self.state.overlay = Some(Overlay::RetryRoute);
    }

    fn handle_retry_route_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Down => {
                let choices = self.state.retry_route_choices();
                self.state.retry_route_choice = self.state.retry_route_choice.next(&choices);
            }
            Intent::Up => {
                let choices = self.state.retry_route_choices();
                self.state.retry_route_choice = self.state.retry_route_choice.previous(&choices);
            }
            Intent::Enter => {
                let Some(run_id) = self.state.selected_run else {
                    self.state.overlay = None;
                    return;
                };
                let Some(stage_id) = self
                    .state
                    .details
                    .as_ref()
                    .and_then(|details| details.stages.get(self.state.selected_stage_index))
                    .map(|stage| stage.id.clone())
                else {
                    self.state.overlay = None;
                    return;
                };
                self.dispatch(WorkerCommand::RetryStage {
                    run_id,
                    stage_id,
                    route: self.state.retry_route_choice.route(),
                });
                self.state.overlay = None;
            }
            _ => {}
        }
    }

    /// Sends a completed run back for one remediation cycle.
    ///
    /// Deliberately not gated on the decision's own words. The verdict is
    /// prose written for a person; the operator read it and pressed the key,
    /// and that is the whole signal. Refusing here for a run the domain would
    /// decline only avoids dispatching a doomed action; it does not weaken
    /// that guard.
    fn request_fix(&mut self) {
        let Some(run_id) = self.state.selected_run else {
            return;
        };
        if self.state.run_can_be_fixed() {
            self.state.fix_when_finished.remove(&run_id);
            self.dispatch(WorkerCommand::RequestFix { run_id });
            return;
        }
        if self.state.run_can_book_a_fix() {
            if self.state.fix_when_finished.remove(&run_id) {
                self.state.notify(
                    UiMessageKind::Info,
                    "Fix cancelled — this run will rest when it finishes.",
                );
            } else {
                self.state.fix_when_finished.insert(run_id);
                self.state.notify(
                    UiMessageKind::Info,
                    "Fix booked — it starts on its own once this run reaches its verdict.",
                );
            }
            return;
        }
        self.state
            .notify(UiMessageKind::Info, fix_unavailable_reason(&self.state));
    }

    /// Opens the free-text "how should the agent continue?" prompt.
    ///
    /// Same eligibility as `[f]` Fix, and the same reason for not gating on
    /// the decision's own words: the operator read the verdict and pressed
    /// the key, and that is the whole signal. Refusing here for a run the
    /// domain would decline only avoids dispatching a doomed action.
    fn open_continue(&mut self) {
        if !self.state.run_can_be_continued() {
            self.state.notify(
                UiMessageKind::Info,
                continue_unavailable_reason(&self.state),
            );
            return;
        }
        self.state.continue_instruction = super::state::TextField::default();
        self.state.overlay = Some(Overlay::Continue);
    }

    fn handle_continue_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Left => self.state.continue_instruction.left(),
            Intent::Right => self.state.continue_instruction.right(),
            Intent::Home => self.state.continue_instruction.home(),
            Intent::End => self.state.continue_instruction.end(),
            Intent::Backspace => self.state.continue_instruction.backspace(),
            Intent::DeleteToStart => self.state.continue_instruction.delete_to_start(),
            Intent::DeleteToEnd => self.state.continue_instruction.delete_to_end(),
            Intent::DeleteWordBefore => self.state.continue_instruction.delete_word_before(),
            Intent::Delete => self.state.continue_instruction.delete(),
            Intent::Character(character) => self.state.continue_instruction.insert(character),
            Intent::Enter => {
                let Some(run_id) = self.state.selected_run else {
                    return;
                };
                let instruction = self.state.continue_instruction.text().trim().to_owned();
                if instruction.is_empty() {
                    self.state.set_error("Instruction cannot be empty");
                    return;
                }
                self.dispatch(WorkerCommand::RequestContinue {
                    run_id,
                    instruction,
                });
                self.state.overlay = None;
            }
            _ => {}
        }
    }

    /// Opens the "work on follow-ups" chooser, pre-filled from the decision
    /// artifact's own `## Follow-ups` section.
    ///
    /// Eligibility is exactly `[c]` Continue's, plus one more thing a verdict
    /// may simply not have written: a Follow-ups section to work from.
    /// Absence is explained rather than silent, the same as every other
    /// refusal in this interface.
    fn open_follow_ups(&mut self) {
        if !self.state.run_can_be_continued() {
            self.state.notify(
                UiMessageKind::Info,
                continue_unavailable_reason(&self.state),
            );
            return;
        }
        let text = match self.decision_follow_ups() {
            Ok(text) => text,
            Err(error) => {
                self.state.set_error(error);
                return;
            }
        };
        let Some(text) = text else {
            self.state.notify(
                UiMessageKind::Info,
                "This decision has no Follow-ups section to work from.",
            );
            return;
        };
        self.state.follow_ups_text = Some(text);
        self.state.follow_ups_as_new_run = false;
        self.state.overlay = Some(Overlay::FollowUps);
    }

    /// The selected run's latest decision's `## Follow-ups` section, if it
    /// wrote one.
    fn decision_follow_ups(&self) -> Result<Option<String>, String> {
        let Some(run_id) = self.state.selected_run else {
            return Ok(None);
        };
        let Some(decision_id) = self.state.details.as_ref().and_then(|details| {
            details
                .stages
                .iter()
                .rev()
                .find(|stage| stage.kind == crate::domain::StageKind::Decision)
                .map(|stage| stage.id.clone())
        }) else {
            return Ok(None);
        };
        self.reader
            .read_artifact(run_id, &decision_id)
            .map(|artifact| super::follow_ups::extract(&artifact.text))
            .map_err(|error| error.to_string())
    }

    /// `[f]`: one key for every cycle a verdict can start.
    ///
    /// A run still working can only book a fix, so the key does that as it
    /// always has. A finished run is offered each cycle it can take — fix,
    /// continue, and Follow-ups only when the decision wrote them — and when
    /// only one applies the picker is skipped.
    fn open_next_cycle(&mut self) {
        let fixable = self.state.run_can_be_fixed();
        let continuable = self.state.run_can_be_continued();
        if !fixable && !continuable {
            self.request_fix();
            return;
        }
        let mut choices = Vec::new();
        if fixable {
            choices.push(CycleChoice::Fix);
        }
        if continuable {
            choices.push(CycleChoice::Continue);
            // An unreadable decision only costs the picker this row; `[w]`
            // still says why when pressed on its own.
            if matches!(self.decision_follow_ups(), Ok(Some(_))) {
                choices.push(CycleChoice::FollowUps);
            }
        }
        if let [only] = choices[..] {
            self.start_cycle(only);
            return;
        }
        self.state.next_cycle_choices = choices;
        self.state.next_cycle_selected = 0;
        self.state.overlay = Some(Overlay::NextCycle);
    }

    fn handle_next_cycle_intent(&mut self, intent: Intent) {
        let choices = self.state.next_cycle_choices.clone();
        let picked = match intent {
            Intent::Up => {
                self.state.next_cycle_selected = self.state.next_cycle_selected.saturating_sub(1);
                None
            }
            Intent::Down => {
                self.state.next_cycle_selected =
                    (self.state.next_cycle_selected + 1).min(choices.len().saturating_sub(1));
                None
            }
            Intent::Enter => choices.get(self.state.next_cycle_selected).copied(),
            Intent::Fix => Some(CycleChoice::Fix),
            Intent::Continue => Some(CycleChoice::Continue),
            Intent::FollowUps => Some(CycleChoice::FollowUps),
            _ => None,
        };
        if let Some(choice) = picked.filter(|choice| choices.contains(choice)) {
            self.state.overlay = None;
            self.start_cycle(choice);
        }
    }

    fn start_cycle(&mut self, choice: CycleChoice) {
        match choice {
            CycleChoice::Fix => self.request_fix(),
            CycleChoice::Continue => self.open_continue(),
            CycleChoice::FollowUps => self.open_follow_ups(),
        }
    }

    fn handle_follow_ups_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Up | Intent::Down => {
                self.state.follow_ups_as_new_run = !self.state.follow_ups_as_new_run;
            }
            Intent::Enter => {
                let Some(text) = self.state.follow_ups_text.clone() else {
                    self.state.overlay = None;
                    return;
                };
                if self.state.follow_ups_as_new_run {
                    let repository = self
                        .state
                        .details
                        .as_ref()
                        .and_then(|details| details.repository.clone());
                    self.state.new_run.task = super::state::TextField::new(text);
                    if let Some(repository) = repository {
                        self.state.new_run.repository =
                            super::state::TextField::new(repository.display().to_string());
                    }
                    self.state.new_run.focus = 0;
                    self.state.overlay = None;
                    self.state.screen = Screen::NewRun;
                } else {
                    let Some(run_id) = self.state.selected_run else {
                        self.state.overlay = None;
                        return;
                    };
                    self.dispatch(WorkerCommand::RequestContinue {
                        run_id,
                        instruction: text,
                    });
                    self.state.overlay = None;
                }
            }
            _ => {}
        }
    }

    /// Starts the fixes the operator booked while their runs were still working.
    ///
    /// Driven from the listing rather than the detail panel, because a booked
    /// run is usually not the one being watched and waiting for the operator to
    /// select it would defeat the point of booking it. A booking the interface
    /// cannot honour yet — the run is held by another action, or this many
    /// agents are already at work — stays booked and says nothing; the next
    /// refresh is half a second away, and an explanation twice a second is not
    /// worth reading.
    fn start_booked_fixes(&mut self) {
        if self.state.fix_when_finished.is_empty() {
            return;
        }
        let ready = self
            .state
            .runs
            .iter()
            .filter(|item| {
                item.status == crate::domain::RunStatus::Completed
                    && self.state.fix_when_finished.contains(&item.id)
            })
            .map(|item| item.id)
            .collect::<Vec<_>>();
        for run_id in ready {
            let command = WorkerCommand::RequestFix { run_id };
            if self.state.action_refusal(&command).is_some() {
                continue;
            }
            self.state.fix_when_finished.remove(&run_id);
            self.dispatch(command);
        }
    }

    /// Resumes runs a dead instance left behind.
    ///
    /// A running run normally has the action driving it in this process, and
    /// nothing else ever reads its provider processes. A row that is already
    /// `Running` on this session's first refresh therefore has no driver
    /// anywhere — its instance died mid-run — and would sit "running" forever,
    /// even after its processes finish. Resuming reconciles what those
    /// processes wrote and drives the run on. Only runs from that first look
    /// are ever swept: anything that turns `Running` later was started here.
    /// Later sweeps retry an orphan the agent ceiling deferred, and drop one
    /// that moved on without us — resolved from the CLI, or finished.
    fn recover_orphaned_runs(&mut self, runs: &[RunListItem]) {
        let pending = if let Some(pending) = self.orphaned.as_mut() {
            pending.retain(|id| {
                runs.iter()
                    .any(|run| run.id == *id && run.status == RunStatus::Running)
            });
            pending.clone()
        } else {
            let found: Vec<RunId> = runs
                .iter()
                .filter(|run| run.status == RunStatus::Running)
                .map(|run| run.id)
                .collect();
            self.orphaned = Some(found.clone());
            found
        };
        let mut reconnected = 0_usize;
        for run_id in pending {
            let command = WorkerCommand::ResumeRun { run_id };
            if self.state.action_refusal(&command).is_some() {
                continue;
            }
            if let Some(pending) = self.orphaned.as_mut() {
                pending.retain(|id| *id != run_id);
            }
            if self.dispatch(command) {
                reconnected += 1;
            }
        }
        // After the dispatches: starting an action clears the message line.
        if reconnected > 0 {
            let noun = if reconnected == 1 { "run" } else { "runs" };
            self.state.notify(
                UiMessageKind::Info,
                format!("Resuming {reconnected} {noun} left running by a previous session"),
            );
        }
    }

    fn open_attention(&mut self) {
        if self
            .state
            .details
            .as_ref()
            .is_none_or(|details| details.attention.is_empty())
        {
            self.state
                .set_error("Selected run has no pending attention");
            return;
        }
        self.state.attention_index = 0;
        self.state.attention_response = super::state::TextField::default();
        self.state.overlay = Some(Overlay::Attention);
    }

    fn open_artifact(&mut self) {
        let Some((run_id, stage_id)) = self.selected_stage_identity() else {
            return;
        };
        match self.reader.read_artifact(run_id, &stage_id) {
            Ok(artifact) => {
                self.state.artifact = Some(artifact);
                self.state.artifact_raw = false;
                self.state.scroll = 0;
                self.state.screen = Screen::Artifact;
            }
            Err(AppError::ArtifactNotFound { .. }) => {
                let status = self
                    .state
                    .details
                    .as_ref()
                    .and_then(|details| details.stages.get(self.state.selected_stage_index))
                    .map(|stage| stage.status);
                let (kind, text) = artifact_unavailable_feedback(status, &stage_id);
                self.state.notify(kind, text);
            }
            Err(error) => self.state.set_error(error.to_string()),
        }
    }

    fn open_logs(&mut self) {
        let Some((run_id, stage_id)) = self.selected_stage_identity() else {
            return;
        };
        match self.reader.read_process_log_tail(run_id, &stage_id) {
            Ok(logs) => {
                self.state.logs = Some(logs);
                self.state.scroll = 0;
                self.state.screen = Screen::Logs;
            }
            Err(error) => self.state.set_error(error.to_string()),
        }
    }

    fn open_diff(&mut self) {
        let Some(run_id) = self.state.selected_run else {
            return;
        };
        match self.reader.preview_run_diff(run_id) {
            Ok(diff) => {
                self.state.diff = Some(diff);
                self.state.scroll = 0;
                self.state.screen = Screen::Diff;
            }
            Err(error) => self.state.set_error(error.to_string()),
        }
    }

    fn open_apply_confirmation(&mut self) {
        let Some(run_id) = self.state.selected_run else {
            return;
        };
        // The workspace layer rejects apply outside the completed branch-run
        // state; refusing here keeps the TUI from dispatching an action the
        // domain will decline, without weakening that guard.
        if !self.state.run_is_applyable() {
            self.state
                .notify(UiMessageKind::Info, apply_unavailable_reason(&self.state));
            return;
        }
        match self.reader.preview_run_diff(run_id) {
            Ok(diff) => {
                self.state.diff = Some(diff);
                self.state.overlay = Some(Overlay::ApplyConfirm);
            }
            Err(error) => self.state.set_error(error.to_string()),
        }
    }

    /// Rebase shares apply's gate — a completed branch run — because it
    /// exists for the run apply just refused; the workspace layer decides
    /// whether there is in fact a newer `HEAD` to move onto, and says so by
    /// name when there is not.
    fn open_rebase_confirmation(&mut self) {
        if self.state.selected_run.is_none() {
            return;
        }
        self.state.rebase_reason = None;
        if self.state.run_is_applyable() {
            self.state.overlay = Some(Overlay::RebaseConfirm);
        } else {
            self.state
                .notify(UiMessageKind::Info, apply_unavailable_reason(&self.state));
        }
    }

    fn settle_purge(&mut self, run_id: crate::domain::RunId) {
        // The run this was looking at no longer exists; the list it came
        // from is where the operator belongs.
        if self.state.selected_run == Some(run_id) {
            self.state.selected_run = None;
            self.state.details = None;
            self.state.screen = Screen::Runs;
        }
        self.state
            .set_message(format!("Run {run_id} deleted for good."));
        self.refresh();
    }

    /// An apply the moved checkout refused turns into the rebase that answers
    /// it, offered in place, so the operator never has to know `[b]` exists.
    /// Only when they are still looking at that run with nothing else open;
    /// otherwise the reason goes to the footer like any other refusal.
    fn offer_rebase(&mut self, run_id: crate::domain::RunId, reason: String) {
        let free = self.state.overlay.is_none()
            && self.state.screen == Screen::RunDetail
            && self.state.selected_run == Some(run_id);
        if free {
            self.state.rebase_reason = Some(reason);
            self.state.overlay = Some(Overlay::RebaseConfirm);
        } else {
            self.state
                .set_error(format!("Patch cannot be applied cleanly: {reason}"));
        }
    }

    /// Publish shares apply's gate — a completed branch run — because both
    /// transport the same delta; they differ only in where it lands.
    fn open_publish_confirmation(&mut self) {
        let Some(run_id) = self.state.selected_run else {
            return;
        };
        if !self.state.run_is_applyable() {
            self.state
                .notify(UiMessageKind::Info, apply_unavailable_reason(&self.state));
            return;
        }
        // Already pushed with nothing newer: there is nothing to send, so the
        // key goes to where the work already is.
        let current_url = self
            .state
            .details
            .as_ref()
            .and_then(|details| details.publication.as_ref())
            .filter(|publication| !publication.outdated)
            .and_then(|publication| publication.pull_request_url.clone());
        if let Some(url) = current_url {
            match desktop::open_in_browser(&url) {
                Ok(()) => self
                    .state
                    .set_message(format!("Already pushed — opened {url}")),
                Err(reason) => self
                    .state
                    .set_error(format!("Could not open the browser: {reason}")),
            }
            return;
        }
        match self.reader.preview_run_diff(run_id) {
            Ok(diff) => {
                self.state.diff = Some(diff);
                self.state.overlay = Some(Overlay::PublishConfirm);
            }
            Err(error) => self.state.set_error(error.to_string()),
        }
    }

    fn selected_stage_identity(&self) -> Option<(crate::domain::RunId, crate::domain::StageId)> {
        Some((self.state.selected_run?, self.state.selected_stage.clone()?))
    }

    /// Starts one action, unless another action already holds its run.
    ///
    /// Returns whether the command was dispatched, so a caller that navigates
    /// on success can tell a started action from a refused one.
    fn dispatch(&mut self, command: WorkerCommand) -> bool {
        self.dispatch_ticketed(command).is_some()
    }

    /// [`Self::dispatch`], returning the ticket the command's progress and
    /// outcome will carry.
    fn dispatch_ticketed(&mut self, command: WorkerCommand) -> Option<u64> {
        if let Some(refusal) = self.state.action_refusal(&command) {
            self.state.set_error(refusal.reason());
            return None;
        }
        self.state.begin_action(command.kind(), command.run_id());
        let ticket = self.worker.send(command);
        self.state.message = None;
        Some(ticket)
    }

    fn receive_start_progress(&mut self) {
        while let Some(update) = self.worker.try_recv_progress() {
            self.handle_start_progress(update);
        }
    }

    /// Moves the starting card on by one step.
    ///
    /// Once the run is running the wait the card was showing is over: a card
    /// still on screen hands over to the new run's detail, which is what the
    /// operator was waiting to see; a hidden one only says so in the footer,
    /// leaving the screen where the operator put it.
    fn handle_start_progress(&mut self, update: StartUpdate) {
        if matches!(
            update.progress,
            StartProgress::PreparingWorkspace(_) | StartProgress::Running(_)
        ) {
            // The run holds the task now; there is no draft left to give back.
            self.state
                .start_drafts
                .retain(|(ticket, _)| *ticket != update.ticket);
        }
        let Some(starting) = self
            .state
            .starting
            .as_mut()
            .filter(|starting| starting.ticket == update.ticket)
        else {
            return;
        };
        match update.progress {
            StartProgress::Running(run_id) => {
                self.state.starting = None;
                if self.state.overlay == Some(Overlay::Starting)
                    && self.state.screen == Screen::MissionDetail
                {
                    // A package's run is one level down from where the
                    // operator is standing; the package row says it started.
                    self.state.overlay = None;
                } else if self.state.overlay == Some(Overlay::Starting) {
                    self.state.overlay = None;
                    self.state.selected_run = Some(run_id);
                    self.state.screen = Screen::RunDetail;
                } else {
                    self.state.set_message(format!("Run {run_id} started."));
                }
                self.refresh();
            }
            StartProgress::PreparingWorkspace(_) => {
                starting.progress = Some(update.progress);
                // The run exists now; let the list show it.
                self.refresh();
            }
            progress => starting.progress = Some(progress),
        }
    }

    /// Hands a refused start to the card when the operator is free to read
    /// it, or to the footer when something else has the screen — the same
    /// rule [`Self::present_publish`] follows, for the same reasons.
    fn present_start_failure(&mut self, task: String, error: String, draft_restored: bool) {
        let free = matches!(self.state.overlay, None | Some(Overlay::Starting))
            && self.state.screen != Screen::NewRun;
        if free {
            self.state.start_failure = Some(StartFailure {
                task,
                error,
                draft_restored,
            });
            self.state.overlay = Some(Overlay::StartFailed);
            self.state.message = None;
        } else if draft_restored {
            self.state.set_error(format!(
                "Run not started: {error} Your task is back in the new run form."
            ));
        } else {
            self.state.set_error(format!("Run not started: {error}"));
        }
    }

    /// Gives a refused start's task back to the composer, unless the operator
    /// has already begun writing another one there: several starts can be
    /// refused in any order, and none of them may overwrite a newer draft.
    fn restore_start_draft(&mut self, ticket: u64) -> bool {
        let Some(index) = self
            .state
            .start_drafts
            .iter()
            .position(|(draft, _)| *draft == ticket)
        else {
            return false;
        };
        let (_, task) = self.state.start_drafts.remove(index);
        if !self.state.new_run.task.text().trim().is_empty() {
            return false;
        }
        self.state.new_run.task = super::state::TextField::new(task);
        true
    }

    /// Closing the refusal leaves its first line in the footer, so the reason
    /// is still on screen for a moment after the card is gone.
    fn close_start_failure(&mut self) {
        self.state.overlay = None;
        if let Some(failure) = self.state.start_failure.take() {
            let headline = failure.error.lines().next().unwrap_or_default().to_owned();
            self.state.set_error(format!("Run not started: {headline}"));
        }
    }

    fn receive_worker_results(&mut self) {
        while let Some(result) = self.worker.try_recv() {
            self.handle_worker_result(result);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one ticket-scoped outcome dispatch table"
    )]
    fn handle_worker_result(&mut self, result: WorkerResult) {
        self.state.settle_action(result.action, result.run_id);
        let draft_restored = result.action == ActionKind::Start
            && result.result.is_err()
            && self.restore_start_draft(result.ticket);
        let start = self
            .state
            .starting
            .take_if(|starting| starting.ticket == result.ticket);
        if let (Some(start), Err(error)) = (start, &result.result) {
            self.present_start_failure(start.task, error.clone(), draft_restored);
            self.refresh();
            return;
        }
        if let (true, Err(error)) = (draft_restored, &result.result) {
            // A start whose card was already handed to a newer one.
            self.state.set_error(format!(
                "Run not started: {error} Your task is back in the new run form."
            ));
            self.refresh();
            return;
        }
        match result.result {
            Ok(success) => {
                if let WorkerSuccess::Purged(receipt) = success {
                    self.settle_purge(receipt.run_id);
                    return;
                }
                if let WorkerSuccess::LeadAsked(turn, details) = success {
                    self.settle_consul_ask(result.ticket, turn.answer, *details);
                    self.refresh();
                    return;
                }
                if let WorkerSuccess::PlanApplied(details) = success {
                    self.settle_lead_plan(result.ticket, *details);
                    self.refresh();
                    return;
                }
                if let WorkerSuccess::Integrated(_) | WorkerSuccess::PackageStarted(..) = &success {
                    self.settle_mission_outcome(success);
                    self.refresh();
                    return;
                }
                if let WorkerSuccess::ApplyNeedsRebase { run_id, reason } = success {
                    self.offer_rebase(run_id, reason);
                    self.refresh();
                    return;
                }
                let Some(report) = success.report() else {
                    return;
                };
                let run_id = report.details.id;
                // An action finishing is not a reason to move the user. Only
                // the run they are already looking at — or the first run this
                // session has, when they are looking at nothing — opens on its
                // own, and only from the runs list or that run's detail with
                // nothing open over them; a task being written, an artifact
                // being read or a question being answered stays where it is.
                // Anything else reports through the message line and the
                // refreshed list, leaving the screen where the user put it.
                let operational = matches!(self.state.screen, Screen::Runs | Screen::RunDetail)
                    && matches!(
                        self.state.overlay,
                        None | Some(Overlay::Publishing | Overlay::Starting)
                    );
                let selected = self.state.selected_run;
                if selected == Some(run_id) || (selected.is_none() && operational) {
                    self.state.selected_run = Some(run_id);
                    self.state.replace_details(report.details.clone());
                    self.state.quiescent = Some(report.outcome.clone());
                    if operational {
                        self.state.screen = Screen::RunDetail;
                    }
                }
                let message = match success {
                    WorkerSuccess::Applied(outcome, _) => format!("Apply finished: {outcome:?}"),
                    WorkerSuccess::Published(receipt, _) => {
                        self.present_publish(PublishOutcome::Pushed {
                            run_id,
                            branch: receipt.branch,
                            commit: receipt.commit,
                            pull_request: receipt.pull_request,
                            note: receipt.note,
                        });
                        self.refresh();
                        return;
                    }
                    // The rebase itself is half the news: the other half is
                    // that apply is now waiting on a check that has not run
                    // yet, which the operator would otherwise meet as a
                    // refusal the next time they press `a`.
                    WorkerSuccess::Rebased(receipt, _) => format!(
                        "Change moved onto {}, {} commit(s) ahead — verify again before \
                         applying: [f] starts a cycle that does.",
                        short_commit(&receipt.to_base),
                        receipt.commits_gained
                    ),
                    WorkerSuccess::Execution(_)
                    | WorkerSuccess::ApplyNeedsRebase { .. }
                    | WorkerSuccess::LeadAsked(..)
                    | WorkerSuccess::PlanApplied(..)
                    | WorkerSuccess::Purged(_)
                    | WorkerSuccess::PackageStarted(..)
                    | WorkerSuccess::Integrated(_) => {
                        format!("{} finished for {run_id}", result.action.label())
                    }
                };
                self.state.set_message(message);
            }
            Err(error) => {
                if result.action == ActionKind::AskLead {
                    let Some(mission_id) = self.consul_mission_for_ticket(result.ticket) else {
                        return;
                    };
                    self.state.consul_pending.remove(&mission_id);
                    self.state.set_error(format!(
                        "Consul turn failed for {mission_id}: {error}. Your draft is preserved."
                    ));
                    self.refresh();
                    return;
                }
                if result.action == ActionKind::ApplyLeadPlan
                    && self.state.lead_plan_pending_ticket == Some(result.ticket)
                {
                    self.state.lead_plan_pending_ticket = None;
                    self.state.lead_plan_preview = None;
                    self.state.lead_plan_scroll = 0;
                    if self.state.overlay == Some(Overlay::PlanConfirm) {
                        self.state.overlay = None;
                    }
                    self.state.set_error(format!(
                        "Plan approval failed. Reload the Mission and preview again before approving: {error}"
                    ));
                    self.refresh();
                    return;
                }
                if let (ActionKind::Publish, Some(run_id)) = (result.action, result.run_id) {
                    self.present_publish(PublishOutcome::Failed { run_id, error });
                    self.refresh();
                    return;
                }
                let scope = result
                    .run_id
                    .map_or(String::new(), |run_id| format!(" for {run_id}"));
                self.state
                    .set_error(format!("{} failed{scope}: {error}", result.action.label()));
            }
        }
        self.refresh();
    }

    fn consul_mission_for_ticket(&self, ticket: u64) -> Option<MissionId> {
        self.state
            .consul_pending
            .iter()
            .find_map(|(mission_id, pending)| (pending.ticket == ticket).then_some(*mission_id))
    }

    fn settle_consul_ask(
        &mut self,
        ticket: u64,
        answer: Option<LeadAnswer>,
        details: crate::app::MissionDetails,
    ) {
        let Some(mission_id) = self.consul_mission_for_ticket(ticket) else {
            return;
        };
        let Some(pending) = self.state.consul_pending.remove(&mission_id) else {
            return;
        };
        if details.id != mission_id {
            self.state.set_error(format!(
                "Consul response for {mission_id} named another Mission; the draft is preserved."
            ));
            return;
        }
        if self
            .state
            .consul_drafts
            .get(&mission_id)
            .is_some_and(|draft| draft.text() == pending.sent_text)
        {
            self.state
                .consul_drafts
                .insert(mission_id, super::state::TextField::default());
        }
        if self.state.selected_mission == Some(mission_id) {
            self.state.replace_mission(details);
            if let Some(answer) = answer {
                self.state.lead_answer = Some(answer);
            } else {
                self.refresh_lead_answer(mission_id);
            }
        }
        self.state
            .set_message(format!("Consul turn finished for {mission_id}."));
    }

    fn settle_lead_plan(&mut self, ticket: u64, details: crate::app::MissionDetails) {
        if self.state.lead_plan_pending_ticket != Some(ticket) {
            return;
        }
        let preview_mission = self
            .state
            .lead_plan_preview
            .as_ref()
            .map(|preview| preview.mission_id);
        self.state.lead_plan_pending_ticket = None;
        self.state.lead_plan_preview = None;
        self.state.lead_plan_scroll = 0;
        if self.state.overlay == Some(Overlay::PlanConfirm) {
            self.state.overlay = None;
        }
        if preview_mission != Some(details.id) {
            self.state.set_error(
                "Plan approval returned a different Mission; reload before continuing.".to_owned(),
            );
            return;
        }
        if self.state.selected_mission == Some(details.id) {
            self.state.replace_mission(details);
        }
        self.state.set_message("Plan changes approved and applied.");
    }

    /// A mission action finishing is news about the package, not the run:
    /// the mission comes back current and the operator stays where they are.
    fn settle_mission_outcome(&mut self, success: WorkerSuccess) {
        match success {
            WorkerSuccess::Integrated(details) => {
                let package = self.state.selected_package.clone();
                self.state.replace_mission(*details);
                self.state.set_message(match package {
                    Some(id) => format!("{id} is in."),
                    None => "Package is in.".to_owned(),
                });
            }
            WorkerSuccess::PackageStarted(report, details) => {
                self.state.replace_mission(*details);
                self.state
                    .set_message(format!("Run {} finished its start.", report.details.id));
            }
            _ => {}
        }
    }

    /// Archives the selected run out of the default list, or brings it back
    /// when the list is showing archived runs. Synchronous on purpose: it is
    /// a single metadata update with no processes or workspaces involved,
    /// exactly like the settling writes `list_runs` already performs on this
    /// thread.
    fn toggle_selected_archived(&mut self) {
        let Some(run) = self.state.runs.get(self.state.selected_run_index) else {
            return;
        };
        let (run_id, archived) = (run.id, run.archived);
        if let Err(error) = self.reader.set_run_archived(run_id, !archived) {
            self.state.set_error(error.to_string());
            return;
        }
        self.refresh();
    }

    /// Arms or disarms automatic approval for the open run.
    ///
    /// Arming is an instruction about what happens next, and a run already
    /// stopped on a permission request is waiting *now* — so arming one also
    /// resumes it, which is what the operator meant by "stop asking me".
    /// Disarming only disarms: a run mid-flight is left alone.
    fn toggle_auto_approve(&mut self) {
        let Some(details) = self.state.details.as_ref() else {
            return;
        };
        let (run_id, armed) = (details.id, details.auto_approve);
        if let Err(error) = self.reader.set_run_auto_approve(run_id, !armed) {
            self.state.set_error(error.to_string());
            return;
        }
        let waiting = details.status == RunStatus::NeedsUser;
        self.refresh();
        if armed {
            self.state
                .set_message("Auto-approve off — permission requests wait for you.".to_owned());
        } else {
            self.state.set_message(
                "Auto-approve on — grantable permission requests are approved for this run. Questions still stop it.".to_owned(),
            );
            if waiting {
                self.resume();
            }
        }
    }

    /// Offers to delete the selected run for good. Only an archived run is
    /// offered: setting a run aside is the step that makes deleting it
    /// deliberate, and the refusal says so rather than doing nothing.
    fn open_delete_confirmation(&mut self) {
        let Some(run) = self.state.runs.get(self.state.selected_run_index) else {
            return;
        };
        if !run.archived {
            self.state.set_error(
                "Archive this run first — only an archived run can be deleted.".to_owned(),
            );
            return;
        }
        self.state.selected_run = Some(run.id);
        self.state.overlay = Some(Overlay::DeleteConfirm);
    }

    fn refresh(&mut self) {
        match self.reader.list_runs() {
            Ok(runs) => {
                self.recover_orphaned_runs(&runs);
                let (visible, archived_count) = if self.state.show_archived {
                    (runs, 0)
                } else {
                    let archived = runs.iter().filter(|run| run.archived).count();
                    (
                        runs.into_iter().filter(|run| !run.archived).collect(),
                        archived,
                    )
                };
                self.state.archived_count = archived_count;
                self.state.replace_runs(visible);
                self.start_booked_fixes();
                self.refresh_selected();
            }
            Err(error) => self.state.set_error(error.to_string()),
        }
        if self.state.screen == Screen::Logs
            && let Some((run_id, stage_id)) = self.selected_stage_identity()
            && let Ok(logs) = self.reader.read_process_log_tail(run_id, &stage_id)
        {
            self.state.logs = Some(logs);
        }
        // The list observes every mission's runs; the open mission needs
        // only its own, so the detail screen skips the list.
        match self.state.screen {
            Screen::Missions => self.refresh_missions(),
            Screen::MissionDetail => self.refresh_selected_mission(),
            _ => {}
        }
        self.last_refresh = Instant::now();
    }

    /// Starts looking up the title of the pull request `task` names, once
    /// per URL per session. `gh` can take as long as a network timeout, so
    /// the lookup never runs on this thread.
    fn look_up_pull_request_title(&mut self, task: Option<&str>) {
        let Some(pull_request) = task.and_then(crate::workspace::PullRequestRef::parse) else {
            return;
        };
        let url = pull_request.url();
        if self.state.pull_request_titles.contains_key(&url) {
            return;
        }
        self.state.pull_request_titles.insert(url.clone(), None);
        let sender = self.titles.sender.clone();
        std::thread::spawn(move || {
            let title = crate::workspace::GhClient::default().pull_request_title(&pull_request);
            let _ = sender.send((url, title));
        });
    }

    fn receive_pull_request_titles(&mut self) {
        while let Ok((url, title)) = self.titles.receiver.try_recv() {
            self.state.pull_request_titles.insert(url, title);
        }
    }

    fn refresh_selected(&mut self) {
        let Some(run_id) = self.state.selected_run else {
            return;
        };
        match self.reader.inspect_run(run_id) {
            Ok(details) => {
                self.look_up_pull_request_title(details.task.as_deref());
                self.state.replace_details(details);
            }
            Err(error) => {
                // The panel must never keep narrating a different run than the
                // one selected. When the selected run cannot be inspected, its
                // own last details may stand, but another run's are dropped —
                // otherwise two list rows appear to share one run.
                if self
                    .state
                    .details
                    .as_ref()
                    .is_some_and(|details| details.id != run_id)
                {
                    self.state.details = None;
                }
                self.state.set_error(error.to_string());
            }
        }
        match self.reader.list_artifacts(run_id) {
            Ok(artifacts) => {
                let complete: Vec<_> = artifacts
                    .into_iter()
                    .filter(|artifact| artifact.status == crate::domain::ArtifactStatus::Complete)
                    .collect();
                self.state.stages_with_artifacts = complete
                    .iter()
                    .map(|artifact| artifact.stage_id.clone())
                    .collect();
                self.state.artifacts_unavailable = None;
                self.refresh_headline(run_id, &complete);
            }
            Err(error) => {
                // A store that cannot be read says so. Swallowing this once
                // rendered every completed stage of a finished run as "No
                // verified artifact" while the database had merely been
                // migrated past this build; see run 01M34EB4YY6G7J3HE2XT2G5KFP.
                self.state.stages_with_artifacts.clear();
                self.state.headline = None;
                self.state.artifacts_unavailable = Some(error.to_string());
                self.state.set_error(error.to_string());
            }
        }
        self.refresh_evidence();
    }

    /// The selected stage's own opening line, for the panel that shows it
    /// before the artifact is opened.
    ///
    /// The artifact is read only when the selection lands on a different
    /// artifact than the one already quoted, so the half-second refresh does
    /// not reread and rehash the same file forever. An artifact that fails
    /// its integrity check quotes nothing; opening it is what reports that.
    fn refresh_headline(&mut self, run_id: crate::domain::RunId, artifacts: &[ArtifactSummary]) {
        let Some((_, stage_id)) = self.selected_stage_identity() else {
            self.state.headline = None;
            return;
        };
        let Some(source) = artifacts
            .iter()
            .filter(|artifact| artifact.stage_id == stage_id)
            .max_by_key(|artifact| artifact.attempt)
        else {
            self.state.headline = None;
            return;
        };
        if self
            .state
            .headline
            .as_ref()
            .is_some_and(|headline| headline.describes(&stage_id, source))
        {
            return;
        }
        let Ok(artifact) = self.reader.read_artifact(run_id, &stage_id) else {
            self.state.headline = None;
            return;
        };
        let opening = bottom_line::extract(&artifact.text);
        self.state.headline = Some(StageHeadline {
            stage_id,
            attempt: source.attempt,
            content_size: source.content_size,
            contracted: opening.as_ref().is_some_and(|opening| opening.contracted),
            text: opening.map(|opening| opening.text),
        });
    }

    /// Per-stage diagnostic evidence is only needed by the technical view, so
    /// it is fetched only while that view is open.
    fn refresh_evidence(&mut self) {
        if !self.state.technical || self.state.screen != Screen::RunDetail {
            self.state.evidence = None;
            return;
        }
        let Some((run_id, stage_id)) = self.selected_stage_identity() else {
            self.state.evidence = None;
            return;
        };
        self.state.evidence = self.reader.stage_execution_evidence(run_id, &stage_id).ok();
    }
}

/// Explains, in operational language, why a fix is not offered yet.
fn fix_unavailable_reason(state: &TuiState) -> String {
    let Some(details) = state.details.as_ref() else {
        return "Select a run before asking for a fix.".to_owned();
    };
    if details.status != crate::domain::RunStatus::Completed {
        return format!(
            "{} A fix answers a decision, so the run has to reach one first.",
            apply_unavailable_reason(state)
        );
    }
    "This workflow has no decision stage, so there is no verdict to answer.".to_owned()
}

/// Explains, in operational language, why Continue/Work on follow-ups is not
/// offered yet. Same shape as [`fix_unavailable_reason`]: both cycles need a
/// completed run that reached a decision.
fn continue_unavailable_reason(state: &TuiState) -> String {
    let Some(details) = state.details.as_ref() else {
        return "Select a run before continuing it.".to_owned();
    };
    if details.status != crate::domain::RunStatus::Completed {
        return format!(
            "{} Continue answers a decision, so the run has to reach one first.",
            apply_unavailable_reason(state)
        );
    }
    "This workflow has no decision stage, so there is nothing to continue.".to_owned()
}

/// Why `S` cannot start the selected package right now.
fn start_package_unavailable_reason(
    status: WorkPackageStatus,
    package_id: &crate::domain::WorkPackageId,
) -> String {
    match status {
        WorkPackageStatus::Planned => {
            format!("{package_id} waits on the packages it depends on; bring those in first.")
        }
        WorkPackageStatus::Running | WorkPackageStatus::Blocked => {
            format!("{package_id} is already being worked on — Enter opens its run.")
        }
        WorkPackageStatus::Delivered => {
            format!("{package_id} is done — I brings it in.")
        }
        WorkPackageStatus::Failed => {
            format!("{package_id} failed; retry it from the CLI (senate mission retry).")
        }
        WorkPackageStatus::Integrated | WorkPackageStatus::Cancelled => {
            format!(
                "{package_id} is {}; nothing left to start.",
                package_status_word(status)
            )
        }
        WorkPackageStatus::Ready => String::new(),
    }
}

/// The package status in the words `senate mission show` uses.
fn package_status_word(status: WorkPackageStatus) -> &'static str {
    match status {
        WorkPackageStatus::Planned => "planned",
        WorkPackageStatus::Ready => "ready",
        WorkPackageStatus::Running => "running",
        WorkPackageStatus::Blocked => "blocked",
        WorkPackageStatus::Delivered => "delivered",
        WorkPackageStatus::Integrated => "integrated",
        WorkPackageStatus::Failed => "failed",
        WorkPackageStatus::Cancelled => "cancelled",
    }
}

/// Explains, in operational language, why apply is not offered yet.
/// Why Stop is unavailable, in the run's own terms.
fn stop_unavailable_reason(state: &TuiState) -> String {
    let Some(details) = state.details.as_ref() else {
        return "No run selected.".to_owned();
    };
    match details.status {
        crate::domain::RunStatus::Interrupted => "Run is already stopped — [r] resumes it.",
        crate::domain::RunStatus::Paused => "Run is already suspended — [r] resumes it.",
        crate::domain::RunStatus::Completed | crate::domain::RunStatus::Applied => {
            "Run has already finished."
        }
        crate::domain::RunStatus::Failed => "Run already stopped on a failure.",
        crate::domain::RunStatus::Discarded => "This run was discarded.",
        _ => "Run is not executing yet.",
    }
    .to_owned()
}

fn apply_unavailable_reason(state: &TuiState) -> String {
    let Some(details) = state.details.as_ref() else {
        return "Select a run before applying.".to_owned();
    };
    if details.workflow == crate::domain::WorkflowKind::Review {
        return "Review runs make no workspace changes, so there is nothing to apply.".to_owned();
    }
    match details.status {
        crate::domain::RunStatus::Applied => "This run has already been applied.".to_owned(),
        crate::domain::RunStatus::Discarded => "This run was discarded.".to_owned(),
        crate::domain::RunStatus::NeedsUser => {
            "Run needs you — resolve the attention request before applying.".to_owned()
        }
        crate::domain::RunStatus::Failed => {
            "Run failed — retry the failed stage before applying.".to_owned()
        }
        _ => "Run is still working — Apply becomes available when it completes.".to_owned(),
    }
}

/// Presentation severity for "no verified artifact" keyed on typed stage
/// state: absence is expected while a stage has not completed, and only a
/// completed stage missing its artifact is a genuine error.
fn artifact_unavailable_feedback(
    status: Option<StageStatus>,
    stage_id: &StageId,
) -> (UiMessageKind, String) {
    match status {
        Some(StageStatus::Running) => (
            UiMessageKind::Info,
            format!("Artifact not available yet — {stage_id} is still running."),
        ),
        Some(StageStatus::Pending | StageStatus::Ready) => (
            UiMessageKind::Info,
            "Artifact not available yet — this stage has not completed.".to_owned(),
        ),
        Some(
            StageStatus::Failed
            | StageStatus::NeedsUser
            | StageStatus::Paused
            | StageStatus::Interrupted,
        ) => (
            UiMessageKind::Warning,
            "No verified artifact is available for this stage yet.".to_owned(),
        ),
        Some(StageStatus::Completed | StageStatus::Skipped) | None => (
            UiMessageKind::Error,
            format!("Stage {stage_id} completed but has no verified artifact."),
        ),
    }
}

const fn is_viewer(screen: Screen) -> bool {
    matches!(screen, Screen::Artifact | Screen::Logs | Screen::Diff)
}

/// Downloads, verifies, and installs one release on a detached thread. Every
/// safety decision lives in the installer; this only moves it off the render
/// loop and turns the outcome into a message.
fn spawn_update_install(info: UpdateInfo) -> Receiver<InstallOutcome> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(install_release(&info).map_err(|error| error.to_string()));
    });
    receiver
}

/// Re-reads the release so the binary and its checksums come from one
/// consistent listing rather than from cached metadata.
fn install_release(info: &UpdateInfo) -> anyhow::Result<String> {
    use crate::update::ReleaseSource as _;
    let source = crate::update::GitHubReleases::new(
        crate::update::OFFICIAL_REPOSITORY,
        Duration::from_secs(10),
    );
    let release = source
        .latest_stable()?
        .filter(|release| release.version == info.available_version)
        .ok_or_else(|| anyhow::anyhow!("release {} is no longer published", info.tag))?;
    let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
    let downloader = crate::update::HttpDownloader::new(Duration::from_secs(120));
    let installed = crate::update::install(&release, &std::env::current_exe()?, &downloader, now)?;
    // A binary that installed but did not register is reported as such; the
    // alternative is silently losing automatic updates.
    Ok(match installed.registration_warning() {
        Some(warning) => format!("{} {warning}", installed.restart_notice()),
        None => installed.restart_notice(),
    })
}

/// Runs one update check on a detached thread so startup never waits on the
/// network. Every failure — including an unresolvable data directory — simply
/// produces no update.
/// Returns the worktrees of runs that closed before reclamation existed, or
/// whose reclamation was interrupted part-way.
///
/// Off the startup path entirely: the sweep opens Git repositories and removes
/// directories that can be gigabytes each, and none of that is work the user
/// asked for or should wait behind. Nothing reports the result, because
/// nothing about it changes what the user can do — the runs are already closed
/// and their branches are kept either way. Failures are left for the next
/// session's sweep.
fn spawn_workspace_reclaim() {
    std::thread::spawn(|| {
        let Ok(service) = RunService::from_environment(RuntimeProviderFactory) else {
            return;
        };
        match service.reclaim_closed_workspaces() {
            Ok(reclaimed) if !reclaimed.is_empty() => {
                tracing::info!(count = reclaimed.len(), "reclaimed closed run worktrees");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "worktree reclaim sweep failed"),
        }
    });
}

fn spawn_update_check() -> Receiver<UpdateOutcome> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok(service) = crate::update::UpdateService::from_environment() else {
            return;
        };
        let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
        // Background detection stays cache-aware: it is not a check the user
        // asked for, and it must never make starting the TUI wait on GitHub.
        let info = service.cached_status(now).available().cloned();
        // Install source is only interesting when an update exists.
        let source = info
            .as_ref()
            .and_then(|_| crate::update::detect_install_source().ok());
        let _ = sender.send((info, source));
    });
    receiver
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tempfile::TempDir;

    use super::*;
    use crate::app::{LeadPlanPreview, LeadSummary, RunDetails, RunListItem, StageSummary};
    use crate::domain::{EffortSetting, Role, RunId, RunStatus, StageId, StageKind, WorkflowKind};
    use crate::tui::state::{CONCURRENT_AGENTS, ExecutionChoice};

    /// A `TuiApp` wired to an empty temporary store: enough to drive intents
    /// without touching the user's data directory or any provider.
    /// Ctrl-S in the attention overlay is one keypress: it resolves the
    /// selected permission with the canned decline and closes the overlay,
    /// with nothing typed.
    #[test]
    fn ctrl_s_in_the_attention_overlay_skips_the_selected_permission() {
        let mut run = details(RunStatus::NeedsUser, WorkflowKind::Deep);
        run.attention = vec![crate::app::AttentionSummary {
            id: crate::domain::AttentionRequestId::from_u128(1),
            stage_id: StageId::new("implementation").unwrap(),
            kind: crate::domain::AttentionKind::Permission,
            summary: "Claude Code requests permission for: Bash".to_owned(),
        }];
        let (mut app, _fixture) = app_with(run);
        app.state.overlay = Some(Overlay::Attention);
        app.handle_intent(Intent::Skip);
        assert_eq!(app.state.overlay, None);
        assert!(
            app.state
                .in_flight
                .iter()
                .any(|action| action.action == ActionKind::ResolveAttention),
            "skip must dispatch a resolution: {:?}",
            app.state.in_flight
        );
        assert!(app.state.attention_response.text().is_empty());
    }

    #[test]
    fn consul_chat_types_command_letters_unicode_and_paste_without_dispatching() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.screen = Screen::MissionDetail;
        app.state.selected_mission = Some(mission_id);
        app.state.mission = Some(mission_details(mission_id, None));

        app.handle_intent(Intent::Consul);
        for (character, modifiers) in [
            ('q', KeyModifiers::NONE),
            ('C', KeyModifiers::SHIFT),
            ('é', KeyModifiers::NONE),
        ] {
            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                modifiers,
            )));
        }
        app.handle_event(Event::Paste("\nreview the tests".to_owned()));

        assert_eq!(app.state.overlay, Some(Overlay::ConsulChat));
        assert_eq!(
            app.state.consul_drafts[&mission_id].text(),
            "qCé review the tests"
        );
        assert!(!app.state.quit, "plain q in the focused composer is text");
        assert!(
            app.state.in_flight.is_empty(),
            "typing and paste do not send"
        );

        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.consul_chat_mission, None);
        assert_eq!(
            app.state.consul_drafts[&mission_id].text(),
            "qCé review the tests"
        );
    }

    #[test]
    fn consul_drafts_stay_with_their_mission_when_chat_is_reopened() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let first = MissionId::from_u128(41);
        let second = MissionId::from_u128(42);
        app.state.screen = Screen::MissionDetail;
        app.state.selected_mission = Some(first);
        app.state.mission = Some(mission_details(first, None));
        app.handle_intent(Intent::Consul);
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )));
        app.handle_intent(Intent::Escape);

        app.state.selected_mission = Some(second);
        app.state.mission = Some(mission_details(second, None));
        app.handle_intent(Intent::Consul);
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::SHIFT,
        )));
        assert_eq!(app.state.consul_drafts[&second].text(), "C");
        app.handle_intent(Intent::Escape);

        app.state.selected_mission = Some(first);
        app.state.mission = Some(mission_details(first, None));
        app.handle_intent(Intent::Consul);
        assert_eq!(app.state.consul_drafts[&first].text(), "q");
        assert_eq!(app.state.consul_drafts[&second].text(), "C");
    }

    #[test]
    fn consul_enter_dispatches_a_ticketed_worker_and_keeps_the_draft_pending() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.screen = Screen::MissionDetail;
        app.state.selected_mission = Some(mission_id);
        app.state.mission = Some(mission_details(mission_id, None));
        app.state.consul_chat_mission = Some(mission_id);
        app.state.overlay = Some(Overlay::ConsulChat);
        app.state.consul_drafts.insert(
            mission_id,
            super::super::state::TextField::new("please inspect the plan"),
        );

        app.handle_intent(Intent::Enter);

        assert!(app.state.consul_pending.contains_key(&mission_id));
        assert_eq!(
            app.state.consul_drafts[&mission_id].text(),
            "please inspect the plan"
        );
        assert!(
            app.state
                .in_flight
                .iter()
                .any(|action| action.action == ActionKind::AskLead)
        );
        assert_eq!(app.state.overlay, Some(Overlay::ConsulChat));
    }

    #[test]
    fn successful_consul_result_clears_only_the_unchanged_submitted_draft() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.selected_mission = Some(mission_id);
        app.state.mission = Some(mission_details(mission_id, None));
        app.state.consul_drafts.insert(
            mission_id,
            super::super::state::TextField::new("sent words"),
        );
        app.state.consul_pending.insert(
            mission_id,
            ConsulAskInFlight {
                ticket: 29,
                sent_text: "sent words".to_owned(),
            },
        );

        app.settle_consul_ask(29, None, mission_details(mission_id, None));

        assert!(!app.state.consul_pending.contains_key(&mission_id));
        assert!(app.state.consul_drafts[&mission_id].text().is_empty());
        assert_eq!(app.state.mission.as_ref().unwrap().id, mission_id);
    }

    #[test]
    fn late_consul_result_does_not_replace_the_selected_mission() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let requested = MissionId::from_u128(41);
        let selected = MissionId::from_u128(42);
        app.state.selected_mission = Some(selected);
        app.state.mission = Some(mission_details(selected, None));
        app.state.consul_drafts.insert(
            requested,
            super::super::state::TextField::new("newer draft"),
        );
        app.state.consul_pending.insert(
            requested,
            ConsulAskInFlight {
                ticket: 29,
                sent_text: "older submitted words".to_owned(),
            },
        );

        app.settle_consul_ask(29, None, mission_details(requested, None));

        assert_eq!(app.state.selected_mission, Some(selected));
        assert_eq!(app.state.mission.as_ref().unwrap().id, selected);
        assert_eq!(app.state.consul_drafts[&requested].text(), "newer draft");
    }

    #[test]
    fn consul_pending_duplicate_and_provider_capacity_refusals_keep_the_draft() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.consul_chat_mission = Some(mission_id);
        app.state.overlay = Some(Overlay::ConsulChat);
        app.state.consul_drafts.insert(
            mission_id,
            super::super::state::TextField::new("keep this draft"),
        );
        app.state.consul_pending.insert(
            mission_id,
            ConsulAskInFlight {
                ticket: 7,
                sent_text: "older ask".to_owned(),
            },
        );
        app.handle_intent(Intent::Enter);
        assert_eq!(
            app.state.consul_drafts[&mission_id].text(),
            "keep this draft"
        );
        assert!(
            app.state.in_flight.is_empty(),
            "a duplicate ask is not dispatched"
        );

        app.state.consul_pending.remove(&mission_id);
        for _ in 0..CONCURRENT_AGENTS {
            app.state.begin_action(ActionKind::AskLead, None);
        }
        app.handle_intent(Intent::Enter);
        assert_eq!(
            app.state.consul_drafts[&mission_id].text(),
            "keep this draft"
        );
        assert_eq!(app.state.in_flight.len(), CONCURRENT_AGENTS);
        assert!(
            app.state
                .message
                .as_ref()
                .unwrap()
                .text
                .contains("already working")
        );
        assert!(!app.state.consul_pending.contains_key(&mission_id));
    }

    #[test]
    fn a_consul_waiting_on_the_operator_routes_attention_instead_of_sending() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        let lead_id = RunId::from_u128(77);
        app.state.screen = Screen::MissionDetail;
        app.state.selected_mission = Some(mission_id);
        app.state.mission = Some(mission_details(
            mission_id,
            Some(LeadSummary {
                run_id: lead_id,
                run_status: RunStatus::NeedsUser,
                turns: 1,
            }),
        ));
        app.state.consul_chat_mission = Some(mission_id);
        app.state.overlay = Some(Overlay::ConsulChat);
        app.state
            .consul_drafts
            .insert(mission_id, super::super::state::TextField::new("continue"));

        app.handle_intent(Intent::Enter);

        assert!(!app.state.consul_pending.contains_key(&mission_id));
        assert!(app.state.in_flight.is_empty());
        let message = &app.state.message.as_ref().unwrap().text;
        assert!(message.contains(&lead_id.to_string()));
        assert!(message.contains("`u`"));
        assert_eq!(app.state.consul_drafts[&mission_id].text(), "continue");
    }

    #[test]
    fn failed_consul_worker_result_preserves_draft_and_releases_mission_lock() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.screen = Screen::Runs;
        app.state.consul_drafts.insert(
            mission_id,
            super::super::state::TextField::new("please keep this"),
        );
        app.state.consul_pending.insert(
            mission_id,
            ConsulAskInFlight {
                ticket: 29,
                sent_text: "please keep this".to_owned(),
            },
        );
        app.state.begin_action(ActionKind::AskLead, None);

        app.handle_worker_result(WorkerResult {
            ticket: 29,
            action: ActionKind::AskLead,
            run_id: None,
            result: Err("provider unavailable".to_owned()),
        });

        assert!(!app.state.consul_pending.contains_key(&mission_id));
        assert_eq!(
            app.state.consul_drafts[&mission_id].text(),
            "please keep this"
        );
        assert!(
            app.state
                .message
                .as_ref()
                .unwrap()
                .text
                .contains("draft is preserved")
        );
        assert!(app.state.in_flight.is_empty());
    }

    #[test]
    fn plan_changes_require_enter_and_stale_refusal_discards_the_preview() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.screen = Screen::Runs;
        app.state.lead_plan_preview = Some(lead_plan_preview(mission_id));
        app.state.overlay = Some(Overlay::PlanConfirm);
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        assert!(app.state.lead_plan_preview.is_none());
        assert!(app.state.in_flight.is_empty(), "Esc never approves");

        app.state.lead_plan_preview = Some(lead_plan_preview(mission_id));
        app.state.overlay = Some(Overlay::PlanConfirm);
        app.state.lead_plan_pending_ticket = Some(35);
        app.state.begin_action(ActionKind::ApplyLeadPlan, None);
        app.handle_worker_result(WorkerResult {
            ticket: 35,
            action: ActionKind::ApplyLeadPlan,
            run_id: None,
            result: Err("stale approval".to_owned()),
        });
        assert_eq!(app.state.overlay, None);
        assert!(app.state.lead_plan_preview.is_none());
        assert_eq!(app.state.lead_plan_pending_ticket, None);
        let message = &app.state.message.as_ref().unwrap().text;
        assert!(message.contains("Plan approval failed"));
        assert!(message.contains("preview again"));
        assert!(app.state.in_flight.is_empty());
    }

    #[test]
    fn enter_is_the_only_plan_confirmation_that_dispatches_approval() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.lead_plan_preview = Some(lead_plan_preview(mission_id));
        app.state.overlay = Some(Overlay::PlanConfirm);

        app.handle_intent(Intent::Enter);

        assert!(app.state.lead_plan_pending_ticket.is_some());
        assert!(app.state.lead_plan_preview.is_some());
        assert_eq!(app.state.overlay, Some(Overlay::PlanConfirm));
        assert!(
            app.state
                .in_flight
                .iter()
                .any(|action| action.action == ActionKind::ApplyLeadPlan)
        );
    }

    #[test]
    fn hiding_pending_approval_keeps_its_ticket_and_cannot_replace_its_mission() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let mission_id = MissionId::from_u128(41);
        app.state.lead_plan_preview = Some(lead_plan_preview(mission_id));
        app.state.lead_plan_pending_ticket = Some(35);
        app.state.overlay = Some(Overlay::PlanConfirm);
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.lead_plan_pending_ticket, Some(35));
        app.state.selected_mission = Some(MissionId::from_u128(42));
        app.open_lead_plan_preview();
        assert_eq!(
            app.state.lead_plan_preview.as_ref().unwrap().mission_id,
            mission_id
        );
        assert_eq!(app.state.lead_plan_pending_ticket, Some(35));
        app.settle_lead_plan(35, mission_details(mission_id, None));
        assert_eq!(app.state.lead_plan_pending_ticket, None);
        assert!(app.state.lead_plan_preview.is_none());
        assert_eq!(app.state.selected_mission, Some(MissionId::from_u128(42)));
    }

    fn mission_details(id: MissionId, lead: Option<LeadSummary>) -> crate::app::MissionDetails {
        let at = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap();
        crate::app::MissionDetails {
            id,
            title: "Fixture mission".to_owned(),
            goal: "Exercise Consul UI".to_owned(),
            repository: PathBuf::from("/repo"),
            base_commit: "abc".to_owned(),
            status: crate::domain::MissionStatus::Active,
            packages: vec![],
            decisions: vec![],
            attention: crate::domain::MissionAttention::default(),
            lead,
            revision: crate::store::MissionRevision::initial(),
            created_at: at,
            updated_at: at,
        }
    }

    fn lead_plan_preview(mission_id: MissionId) -> LeadPlanPreview {
        LeadPlanPreview {
            mission_id,
            approval: crate::store::LeadProposalApproval {
                mission_revision: crate::store::MissionRevision::initial(),
                run_id: RunId::from_u128(77),
                run_revision: crate::store::RunRevision::initial(),
                stage_id: StageId::new("engineering-lead").unwrap(),
                answer_sha256: "ab".repeat(32),
            },
            changes: vec![],
            text: "No changes".to_owned(),
        }
    }

    fn app_with(details: RunDetails) -> (TuiApp, TempDir) {
        let fixture = TempDir::new().unwrap();
        let database = fixture.path().join("senate.db");
        let worktrees = fixture.path().join("worktrees");
        let mut state = TuiState::new(fixture.path());
        state.screen = Screen::RunDetail;
        state.selected_run = Some(details.id);
        state.replace_details(details);
        let app = TuiApp {
            state,
            reader: RunService::new(database.clone(), worktrees.clone(), RuntimeProviderFactory),
            missions: MissionService::new(database.clone(), worktrees.clone()),
            worker: Worker::spawn(
                RunService::new(database.clone(), worktrees.clone(), RuntimeProviderFactory),
                MissionService::new(database, worktrees),
            ),
            // Tests never check for updates: an immediately dropped sender
            // reads as "no update will ever arrive".
            update: mpsc::channel().1,
            installing: None,
            last_refresh: Instant::now(),
            started: Instant::now(),
            orphaned: None,
            titles: TitleLookups::default(),
        };
        (app, fixture)
    }

    fn details(status: RunStatus, workflow: WorkflowKind) -> RunDetails {
        RunDetails {
            id: RunId::from_u128(7),
            auto_approve: false,
            task: Some("Add OAuth provider support".to_owned()),
            workflow,
            status,
            repository: Some(std::path::PathBuf::from("/repo")),
            // A review is prepared detached, and adopts a branch only when it
            // is sent back to fix what it found. The fixture says so, because
            // that is what decides whether apply is offered.
            workspace_mode: Some(if workflow == WorkflowKind::Review {
                crate::workspace::WorkspaceMode::Detached
            } else {
                crate::workspace::WorkspaceMode::Branch
            }),
            workspace_status: Some(crate::workspace::WorkspaceStatus::Ready),
            base_commit: Some("abc1234".to_owned()),
            profile: "recommended".to_owned(),
            profile_version: "recommended_v2".to_owned(),
            // A run can only be fixed if its configuration routes the roles a
            // fix adds and states their effort, so the fixture carries both.
            routes: [Role::Implementer, Role::EngineeringLead]
                .into_iter()
                .map(|role| crate::app::RouteSummary {
                    role,
                    configured_provider: "codex".to_owned(),
                    configured_model: None,
                    reason: "test".to_owned(),
                    requested_effort: Some(EffortSetting::NativeDefault),
                })
                .collect(),
            revision: crate::store::RunRevision::initial(),
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            stages: vec![StageSummary {
                id: StageId::new("implementation").unwrap(),
                kind: StageKind::Implementation,
                role: Role::Implementer,
                status: StageStatus::Running,
                configured_provider: "codex".to_owned(),
                requested_effort: EffortSetting::NativeDefault,
                observed_effort: None,
                configured_model: None,
                route_overridden: false,
                actual_provider: None,
                actual_model: None,
                provider_session_record: None,
                native_session: None,
                provider_session_status: None,
                process_status: None,
                started_at: None,
                finished_at: None,
                waiting: None,
                failure_reason: None,
                blocking: false,
            }],
            attention: Vec::new(),
            usage: crate::app::RunUsage::default(),
            started_at: None,
            finished_at: None,
            failure_reason: None,
            image_generations: Vec::new(),
            publication: None,
        }
    }

    /// The same run, having reached a verdict a person can reject.
    fn decided(mut details: RunDetails) -> RunDetails {
        let mut decision = details.stages[0].clone();
        decision.id = StageId::new("decision").unwrap();
        decision.kind = StageKind::Decision;
        decision.role = Role::EngineeringLead;
        decision.status = StageStatus::Completed;
        details.stages.push(decision);
        details
    }

    /// The same run as configuration sealed before fix-cycle routing wrote it:
    /// a verdict to answer, and no route for the role that would answer it.
    fn unroutable(mut details: RunDetails) -> RunDetails {
        details
            .routes
            .retain(|route| route.role != Role::Implementer);
        details
    }

    /// A panel narrating run A while run B is selected makes two list rows
    /// appear to share one run. When the selected run cannot be inspected,
    /// another run's details are dropped rather than left standing.
    #[test]
    fn anothers_details_are_dropped_when_the_selected_run_cannot_be_inspected() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Review));
        app.state.selected_run = Some(RunId::from_u128(8));
        app.refresh_selected();
        assert!(
            app.state.details.is_none(),
            "run 7's details must not stand in for unreadable run 8"
        );
        assert!(app.state.message.is_some());
    }

    /// The selected run's own last details may stand while it is unreadable:
    /// they are stale, but they are its own story, and dropping them would
    /// blank the panel on every transient read failure.
    #[test]
    fn a_runs_own_details_survive_its_inspect_failure() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Review));
        app.refresh_selected();
        assert!(app.state.details.is_some());
        assert_eq!(app.state.details.as_ref().unwrap().id, RunId::from_u128(7));
    }

    /// "Fix it" answers a decision, so it is offered exactly where one exists
    /// and the run has finished reaching it.
    #[test]
    fn fix_is_dispatched_only_for_a_completed_run_that_reached_a_decision() {
        for (label, run) in [
            (
                "a rejected standard run is exactly what fix is for",
                decided(details(RunStatus::Completed, WorkflowKind::Standard)),
            ),
            (
                "a review reaches a verdict too, and may be sent back to act on it",
                decided(details(RunStatus::Completed, WorkflowKind::Review)),
            ),
        ] {
            let (mut app, _fixture) = app_with(run);
            app.handle_intent(Intent::Fix);
            // A run that can also continue gets the picker first; `f` there
            // picks the fix.
            if app.state.overlay == Some(Overlay::NextCycle) {
                assert_eq!(app.state.next_cycle_choices[0], CycleChoice::Fix);
                app.handle_intent(Intent::Fix);
            }
            assert_eq!(
                app.state.busy_label().as_deref(),
                Some("fixing run"),
                "{label}"
            );
        }

        for (label, run) in [
            (
                "already applied",
                decided(details(RunStatus::Applied, WorkflowKind::Standard)),
            ),
            (
                "no decision stage to answer",
                details(RunStatus::Completed, WorkflowKind::Fast),
            ),
            (
                "configuration sealed before fix-cycle routing",
                unroutable(decided(details(RunStatus::Completed, WorkflowKind::Review))),
            ),
        ] {
            let (mut app, _fixture) = app_with(run);
            app.handle_intent(Intent::Fix);
            assert!(app.state.in_flight.is_empty(), "fix dispatched for {label}");
            // A refusal explains itself rather than doing nothing visible.
            assert!(
                app.state.message.is_some(),
                "no explanation offered for {label}"
            );
        }
    }

    /// The operator usually knows they want a fix long before the verdict
    /// lands. Booking it means they say so once, and stop watching for the run
    /// to end.
    #[test]
    fn a_fix_booked_while_a_run_works_starts_itself_when_the_run_finishes() {
        let run_id = RunId::from_u128(7);
        let (mut app, _fixture) =
            app_with(decided(details(RunStatus::Running, WorkflowKind::Review)));

        app.handle_intent(Intent::Fix);
        assert!(
            app.state.in_flight.is_empty(),
            "booking starts nothing while the run is still working"
        );
        assert!(app.state.fix_when_finished.contains(&run_id));

        app.handle_intent(Intent::Fix);
        assert!(
            !app.state.fix_when_finished.contains(&run_id),
            "the same key cancels a booking the operator changed their mind about"
        );

        app.handle_intent(Intent::Fix);
        app.state.runs = vec![RunListItem {
            id: run_id,
            workflow: WorkflowKind::Review,
            status: RunStatus::Completed,
            task_summary: "Add OAuth provider support".to_owned(),
            repository: Some(std::path::PathBuf::from("/repo")),
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            archived: false,
        }];
        app.start_booked_fixes();

        assert_eq!(app.state.busy_label().as_deref(), Some("fixing run"));
        assert!(
            app.state.fix_when_finished.is_empty(),
            "a booking is spent once it is honoured, not replayed every refresh"
        );
    }

    #[test]
    fn apply_is_refused_for_every_non_applyable_run_state() {
        // The workspace layer rejects apply outside a completed branch run;
        // the TUI must not dispatch into that rejection. The confirmation
        // overlay is the only path to ApplyRun, so an overlay that never
        // opens is a command that never dispatches.
        for (status, workflow) in [
            (RunStatus::Running, WorkflowKind::Standard),
            (RunStatus::NeedsUser, WorkflowKind::Standard),
            (RunStatus::Paused, WorkflowKind::Standard),
            (RunStatus::Failed, WorkflowKind::Standard),
            (RunStatus::Applied, WorkflowKind::Standard),
            (RunStatus::Completed, WorkflowKind::Review),
        ] {
            // Publish shares apply's gate, so both intents refuse identically.
            for intent in [Intent::Apply, Intent::Publish] {
                let (mut app, _fixture) = app_with(details(status, workflow));
                app.handle_intent(intent);
                assert_eq!(
                    app.state.overlay, None,
                    "{intent:?} confirmation must not open for {status:?}/{workflow:?}"
                );
                assert!(
                    app.state.in_flight.is_empty(),
                    "no worker command dispatched for {status:?}/{workflow:?}"
                );
                let message = app.state.message.as_ref().expect("user is told why");
                assert_eq!(message.kind, UiMessageKind::Info, "refusal is not an error");
            }
        }
    }

    #[test]
    fn apply_opens_confirmation_for_a_real_completed_branch_run() {
        // Driven against a real committed fake run so the whole path runs:
        // eligibility gate, diff preview, then the confirmation overlay.
        let fixture = TempDir::new().unwrap();
        let repo = fixture.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for arguments in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(&arguments)
                    .current_dir(&repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        std::fs::write(repo.join("README.md"), "baseline\n").unwrap();
        for arguments in [vec!["add", "README.md"], vec!["commit", "-qm", "initial"]] {
            assert!(
                std::process::Command::new("git")
                    .args(&arguments)
                    .current_dir(&repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let database = fixture.path().join("senate.db");
        let worktrees = fixture.path().join("worktrees");
        let report = RunService::new(
            database.clone(),
            worktrees.clone(),
            crate::app::DevelopmentFakeProviderFactory::new(fixture.path().join("runs")),
        )
        .start_run(
            WorkflowKind::Standard,
            "mission deck apply gate",
            repo,
            Some(crate::app::ExecutionSelection::Uniform(
                crate::app::UniformProvider::Fake,
            )),
            EffortSetting::NativeDefault,
            &crate::app::ImageGenerationPlan::disabled(),
        )
        .unwrap();
        assert_eq!(report.details.status, RunStatus::Completed);

        let mut state = TuiState::new(fixture.path());
        state.screen = Screen::RunDetail;
        state.selected_run = Some(report.details.id);
        state.replace_details(report.details);
        let mut app = TuiApp {
            state,
            reader: RunService::new(database.clone(), worktrees.clone(), RuntimeProviderFactory),
            missions: MissionService::new(database.clone(), worktrees.clone()),
            worker: Worker::spawn(
                RunService::new(database.clone(), worktrees.clone(), RuntimeProviderFactory),
                MissionService::new(database, worktrees),
            ),
            // Tests never check for updates: an immediately dropped sender
            // reads as "no update will ever arrive".
            update: mpsc::channel().1,
            installing: None,
            last_refresh: Instant::now(),
            started: Instant::now(),
            orphaned: None,
            titles: TitleLookups::default(),
        };
        assert!(app.state.run_is_applyable());
        app.handle_intent(Intent::Apply);
        assert_eq!(app.state.overlay, Some(Overlay::ApplyConfirm));
        assert!(
            app.state.in_flight.is_empty(),
            "opening the confirmation still dispatches nothing"
        );

        // Publish rides the same gate and diff preview, behind its own
        // confirmation.
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        app.handle_intent(Intent::Publish);
        assert_eq!(app.state.overlay, Some(Overlay::PublishConfirm));
        assert!(
            app.state.in_flight.is_empty(),
            "opening the publish confirmation still dispatches nothing"
        );
    }

    /// Continue and Work on follow-ups share Fix's own eligibility: a
    /// completed run that reached a decision whose sealed configuration can
    /// route the cycle it would grow.
    #[test]
    fn continue_and_follow_ups_share_fixs_own_eligibility() {
        for (label, run) in [
            (
                "a rejected standard run",
                decided(details(RunStatus::Completed, WorkflowKind::Standard)),
            ),
            (
                "a decided review",
                decided(details(RunStatus::Completed, WorkflowKind::Review)),
            ),
        ] {
            let (app, _fixture) = app_with(run);
            assert!(app.state.run_can_be_continued(), "{label}");
        }

        for (label, run) in [
            (
                "still running",
                details(RunStatus::Running, WorkflowKind::Standard),
            ),
            (
                "no decision stage",
                details(RunStatus::Completed, WorkflowKind::Fast),
            ),
            (
                "configuration sealed before continue-cycle routing",
                unroutable(decided(details(RunStatus::Completed, WorkflowKind::Review))),
            ),
        ] {
            let (app, _fixture) = app_with(run);
            assert!(!app.state.run_can_be_continued(), "{label}");
        }
    }

    /// `[f]` on a finished run that can take more than one cycle asks which;
    /// the arrows and Enter pick, and so does each cycle's own key.
    #[test]
    fn next_cycle_offers_each_cycle_the_run_can_take() {
        let (mut app, _fixture) = app_with(decided(details(
            RunStatus::Completed,
            WorkflowKind::Standard,
        )));
        app.handle_intent(Intent::Fix);
        assert_eq!(app.state.overlay, Some(Overlay::NextCycle));
        // No Follow-ups section was written, so none is offered.
        assert_eq!(
            app.state.next_cycle_choices,
            vec![CycleChoice::Fix, CycleChoice::Continue]
        );
        app.handle_intent(Intent::Down);
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, Some(Overlay::Continue));

        app.state.overlay = None;
        app.handle_intent(Intent::Fix);
        app.handle_intent(Intent::FollowUps);
        assert_eq!(
            app.state.overlay,
            Some(Overlay::NextCycle),
            "a cycle not on offer is not picked"
        );
        app.handle_intent(Intent::Continue);
        assert_eq!(app.state.overlay, Some(Overlay::Continue));
    }

    /// `[c]` opens the overlay only where eligible, and explains itself
    /// otherwise — the same shape as Fix's own refusal.
    #[test]
    fn continue_overlay_opens_only_for_a_completed_run_that_reached_a_decision() {
        let (mut app, _fixture) = app_with(decided(details(
            RunStatus::Completed,
            WorkflowKind::Standard,
        )));
        app.handle_intent(Intent::Continue);
        assert_eq!(app.state.overlay, Some(Overlay::Continue));
        assert!(app.state.continue_instruction.text().is_empty());

        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Fast));
        app.handle_intent(Intent::Continue);
        assert_eq!(
            app.state.overlay, None,
            "no decision stage to continue past"
        );
        assert!(app.state.message.is_some(), "the refusal explains itself");
    }

    /// Typed text becomes the dispatched instruction; a blank one is
    /// refused without closing the overlay; Escape cancels without
    /// dispatching anything.
    #[test]
    fn continue_overlay_input_flow_submits_or_cancels() {
        let (mut app, _fixture) = app_with(decided(details(
            RunStatus::Completed,
            WorkflowKind::Standard,
        )));
        app.handle_intent(Intent::Continue);
        for character in "add tests".chars() {
            app.handle_intent(Intent::Character(character));
        }
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.busy_label().as_deref(), Some("continuing run"));

        let (mut app, _fixture) = app_with(decided(details(
            RunStatus::Completed,
            WorkflowKind::Standard,
        )));
        app.handle_intent(Intent::Continue);
        app.handle_intent(Intent::Enter);
        assert_eq!(
            app.state.overlay,
            Some(Overlay::Continue),
            "a blank instruction is refused, and the overlay stays open to fix it"
        );
        assert!(app.state.message.is_some());

        let (mut app, _fixture) = app_with(decided(details(
            RunStatus::Completed,
            WorkflowKind::Standard,
        )));
        app.handle_intent(Intent::Continue);
        app.handle_intent(Intent::Character('x'));
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        assert!(
            app.state.in_flight.is_empty(),
            "Escape cancels without dispatching"
        );
    }

    /// Builds a real completed Standard run through a real git repository and
    /// the Fake provider, mirroring
    /// `apply_opens_confirmation_for_a_real_completed_branch_run`'s own
    /// setup. Follow-ups extraction reads an actual artifact file through the
    /// store, so it needs a genuine run rather than the lightweight
    /// `RunDetails` fixtures the eligibility tests use.
    fn real_completed_standard_run() -> (TempDir, PathBuf, PathBuf, PathBuf, RunDetails) {
        let fixture = TempDir::new().unwrap();
        let repo = fixture.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for arguments in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(&arguments)
                    .current_dir(&repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        std::fs::write(repo.join("README.md"), "baseline\n").unwrap();
        for arguments in [vec!["add", "README.md"], vec!["commit", "-qm", "initial"]] {
            assert!(
                std::process::Command::new("git")
                    .args(&arguments)
                    .current_dir(&repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let database = fixture.path().join("senate.db");
        let worktrees = fixture.path().join("worktrees");
        let report = RunService::new(
            database.clone(),
            worktrees.clone(),
            crate::app::DevelopmentFakeProviderFactory::new(fixture.path().join("runs")),
        )
        .start_run(
            WorkflowKind::Standard,
            "follow-ups fixture",
            &repo,
            Some(crate::app::ExecutionSelection::Uniform(
                crate::app::UniformProvider::Fake,
            )),
            EffortSetting::NativeDefault,
            &crate::app::ImageGenerationPlan::disabled(),
        )
        .unwrap();
        assert_eq!(report.details.status, RunStatus::Completed);
        (fixture, database, worktrees, repo, report.details)
    }

    fn tui_app_over(
        fixture_path: &std::path::Path,
        database: PathBuf,
        worktrees: PathBuf,
        details: RunDetails,
    ) -> TuiApp {
        let mut state = TuiState::new(fixture_path);
        state.screen = Screen::RunDetail;
        state.selected_run = Some(details.id);
        state.replace_details(details);
        TuiApp {
            state,
            reader: RunService::new(database.clone(), worktrees.clone(), RuntimeProviderFactory),
            missions: MissionService::new(database.clone(), worktrees.clone()),
            worker: Worker::spawn(
                RunService::new(database.clone(), worktrees.clone(), RuntimeProviderFactory),
                MissionService::new(database, worktrees),
            ),
            update: mpsc::channel().1,
            installing: None,
            last_refresh: Instant::now(),
            started: Instant::now(),
            orphaned: None,
            titles: TitleLookups::default(),
        }
    }

    /// Persists one Markdown artifact directly through the store, the same
    /// way `artifact_read_revalidates_hash_and_does_not_mutate_run` in
    /// `run_service.rs` does: real content behind a real artifact record,
    /// without needing a provider to have written it.
    fn insert_markdown_artifact(
        fixture_path: &std::path::Path,
        database: &std::path::Path,
        run_id: RunId,
        stage_id: &StageId,
        content: &str,
    ) {
        let artifact_path = fixture_path.join(format!("{stage_id}.md"));
        std::fs::write(&artifact_path, content).unwrap();
        let created_at: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();
        let metadata = crate::domain::ArtifactMetadata::new(
            crate::domain::ArtifactId::new(),
            run_id,
            stage_id.clone(),
            crate::domain::ArtifactKind::Decision,
            Role::EngineeringLead,
            crate::domain::ArtifactStatus::Complete,
            created_at,
        )
        .with_provider(crate::domain::ProviderId::new("fake").unwrap(), None);
        let bytes = content.as_bytes();
        let artifact = crate::providers::ArtifactRecord::new(
            metadata,
            1,
            artifact_path,
            sha256_hex(bytes),
            u64::try_from(bytes.len()).unwrap(),
            created_at,
        )
        .unwrap();
        let mut store = crate::store::SqliteStore::open(database).unwrap();
        store.insert_artifact(&artifact).unwrap();
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;

        let mut hash = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            write!(hash, "{byte:02x}").expect("writing to String cannot fail");
        }
        hash
    }

    fn decision_stage_id(details: &RunDetails) -> StageId {
        details
            .stages
            .iter()
            .rev()
            .find(|stage| stage.kind == StageKind::Decision)
            .expect("Standard workflow reaches a decision")
            .id
            .clone()
    }

    /// The whole `[w]` path, end to end: the decision's own `## Follow-ups`
    /// section is extracted verbatim and offered back either to this run or
    /// to a new one, exactly the choice the spec describes.
    #[test]
    fn follow_ups_offers_working_in_this_run_or_starting_a_new_one() {
        let (fixture, database, worktrees, repo, details) = real_completed_standard_run();
        let decision_id = decision_stage_id(&details);
        insert_markdown_artifact(
            fixture.path(),
            &database,
            details.id,
            &decision_id,
            "# Decision\n\n## Verdict\n\nApproved.\n\n## Follow-ups\n- Add integration coverage\n- Generalize the helper\n",
        );

        // Default choice: continue in this run.
        let mut app = tui_app_over(
            fixture.path(),
            database.clone(),
            worktrees.clone(),
            details.clone(),
        );
        app.handle_intent(Intent::FollowUps);
        assert_eq!(app.state.overlay, Some(Overlay::FollowUps));
        assert_eq!(
            app.state.follow_ups_text.as_deref(),
            Some("- Add integration coverage\n- Generalize the helper")
        );
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.busy_label().as_deref(), Some("continuing run"));

        // Toggled choice: hand the same text to a new run's composer, with
        // the repository the run itself was created against — canonicalized
        // by discovery, which is why this compares against the run's own
        // recorded repository rather than the pre-canonicalization path the
        // fixture built it from.
        let expected_repository = details
            .repository
            .clone()
            .expect("a started run always records its repository");
        assert_eq!(expected_repository.file_name(), repo.file_name());
        let mut app = tui_app_over(fixture.path(), database, worktrees, details);
        app.handle_intent(Intent::FollowUps);
        app.handle_intent(Intent::Up);
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.screen, Screen::NewRun);
        assert_eq!(
            app.state.new_run.task.text(),
            "- Add integration coverage\n- Generalize the helper"
        );
        assert_eq!(
            app.state.new_run.repository.text(),
            expected_repository.display().to_string()
        );
    }

    /// A decision that wrote no Follow-ups section explains why, rather than
    /// opening an empty chooser.
    #[test]
    fn follow_ups_explains_when_the_decision_wrote_no_such_section() {
        let (fixture, database, worktrees, _repo, details) = real_completed_standard_run();
        let decision_id = decision_stage_id(&details);
        insert_markdown_artifact(
            fixture.path(),
            &database,
            details.id,
            &decision_id,
            "# Decision\n\n## Verdict\n\nApproved.\n",
        );

        let mut app = tui_app_over(fixture.path(), database, worktrees, details);
        app.handle_intent(Intent::FollowUps);
        assert_eq!(app.state.overlay, None);
        assert!(
            app.state
                .message
                .as_ref()
                .is_some_and(|message| message.text.contains("Follow-ups")),
            "the refusal names what is missing"
        );
    }

    #[test]
    fn refusal_wording_names_the_blocking_state() {
        let (app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        assert!(apply_unavailable_reason(&app.state).contains("still working"));

        let (app, _fixture) = app_with(details(RunStatus::NeedsUser, WorkflowKind::Standard));
        assert!(apply_unavailable_reason(&app.state).contains("resolve the attention"));

        let (app, _fixture) = app_with(details(RunStatus::Applied, WorkflowKind::Standard));
        assert!(apply_unavailable_reason(&app.state).contains("already been applied"));

        let (app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Review));
        assert!(apply_unavailable_reason(&app.state).contains("no workspace changes"));
    }

    fn update_info() -> crate::update::UpdateInfo {
        crate::update::UpdateInfo {
            current_version: semver::Version::parse("0.1.0").unwrap(),
            available_version: semver::Version::parse("0.2.0").unwrap(),
            tag: "v0.2.0".to_owned(),
            release_url: "https://example.invalid/r".to_owned(),
            published_at: None,
        }
    }

    #[test]
    fn declining_the_update_dismisses_it_without_installing() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::Runs;
        app.state.update = Some(update_info());
        app.state.update_install = Some(crate::update::InstallSource::OfficialBinary);
        app.state.overlay = Some(Overlay::Update);

        // Moving the selection to No and confirming must not start anything.
        app.handle_intent(Intent::Down);
        assert!(!app.state.update_install_selected);
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        assert!(app.state.update_dismissed);
        assert!(
            app.installing.is_none(),
            "declining never starts an installation"
        );
        assert!(!app.state.update_prompt_is_due(), "it does not reopen");
    }

    #[test]
    fn escape_dismisses_the_update_for_the_session() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::Runs;
        app.state.update = Some(update_info());
        app.state.update_install = Some(crate::update::InstallSource::OfficialBinary);
        app.state.overlay = Some(Overlay::Update);
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        assert!(app.state.update_dismissed);
        assert!(app.installing.is_none());
    }

    #[test]
    fn an_unsupported_installation_can_never_start_an_install() {
        for source in [
            crate::update::InstallSource::Source,
            crate::update::InstallSource::Cargo,
            crate::update::InstallSource::Homebrew,
            crate::update::InstallSource::Unknown,
        ] {
            let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
            app.state.screen = Screen::Runs;
            app.state.update = Some(update_info());
            app.state.update_install = Some(source);
            app.state.overlay = Some(Overlay::Update);
            // Even with the install answer selected, the strategy refuses.
            app.state.update_install_selected = true;
            app.handle_intent(Intent::Enter);
            assert!(
                app.installing.is_none(),
                "{source:?} must never begin an installation"
            );
            assert!(app.state.update_dismissed);
        }
    }

    #[test]
    fn the_update_prompt_does_not_hijack_run_keys() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::Runs;
        app.state.update = Some(update_info());
        app.state.update_install = Some(crate::update::InstallSource::OfficialBinary);
        app.state.overlay = Some(Overlay::Update);
        // Run actions are inert while the prompt owns the keyboard.
        app.handle_intent(Intent::Apply);
        assert_eq!(app.state.overlay, Some(Overlay::Update));
        assert!(app.state.in_flight.is_empty());
    }

    #[test]
    fn stop_dispatches_only_for_an_executing_run_and_never_discards() {
        for status in [RunStatus::Running, RunStatus::NeedsUser] {
            let (mut app, _fixture) = app_with(details(status, WorkflowKind::Standard));
            app.handle_intent(Intent::Stop);
            assert!(
                !app.state.in_flight.is_empty(),
                "{status:?} is stoppable, so the action dispatches"
            );
            assert_eq!(
                app.state.overlay, None,
                "{status:?}: stop is non-destructive and opens no confirmation"
            );
        }

        for status in [
            RunStatus::Completed,
            RunStatus::Applied,
            RunStatus::Discarded,
            RunStatus::Failed,
            RunStatus::Interrupted,
            RunStatus::Paused,
        ] {
            let (mut app, _fixture) = app_with(details(status, WorkflowKind::Standard));
            app.handle_intent(Intent::Stop);
            assert!(
                app.state.in_flight.is_empty(),
                "{status:?} must not dispatch a stop"
            );
            assert!(
                app.state.message.is_some(),
                "{status:?}: the refusal explains itself"
            );
            assert_eq!(app.state.overlay, None);
        }
    }

    /// The same run under a different identity, for the cases that need two.
    fn other_run(mut details: RunDetails, id: RunId) -> RunDetails {
        details.id = id;
        details
    }

    fn start_command() -> WorkerCommand {
        WorkerCommand::StartRun {
            workflow: WorkflowKind::Standard,
            task: "a second piece of work".to_owned(),
            repository: std::path::PathBuf::from("/repo"),
            selection: crate::app::ExecutionSelection::Uniform(crate::app::UniformProvider::Fake),
            effort: crate::app::EffortRequest::ProfileDefault,
        }
    }

    /// The regression this gate was rebuilt around: one busy run used to lock
    /// the whole interface, so starting a second run — the thing the command
    /// line could always do — was refused for as long as the first one ran.
    #[test]
    fn work_on_one_run_never_holds_back_another_run_or_a_new_one() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        let first = RunId::from_u128(7);
        let second = RunId::from_u128(8);

        assert!(app.dispatch(WorkerCommand::ResumeRun { run_id: first }));
        assert!(
            app.dispatch(WorkerCommand::ResumeRun { run_id: second }),
            "another run's work is none of the first run's business"
        );
        assert!(
            app.dispatch(start_command()),
            "a new run holds no existing run, so nothing can hold it back"
        );
        assert_eq!(app.state.in_flight.len(), 3, "all three are working");
    }

    /// Every choice field in the composer answers to the arrow keys. Effort
    /// is the fourth field; it must cycle like workflow and execution do,
    /// not fall through to the text editor that ignores it.
    #[test]
    fn arrow_keys_cycle_the_composer_effort_field() {
        use crate::tui::state::EFFORT_CHOICES;
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;
        app.state.new_run.focus = 4;
        let start = app.state.new_run.effort;
        app.handle_intent(Intent::Right);
        let next = app.state.new_run.effort;
        assert_ne!(next, start, "Right must move Effort to another choice");
        assert_eq!(
            next,
            EFFORT_CHOICES[(EFFORT_CHOICES.iter().position(|c| *c == start).unwrap() + 1)
                % EFFORT_CHOICES.len()],
            "Right steps forward through EFFORT_CHOICES"
        );
        app.handle_intent(Intent::Left);
        assert_eq!(
            app.state.new_run.effort, start,
            "Left steps back to where it was"
        );
    }

    /// The pipeline is a ring: stepping back from the first stage lands on
    /// the last, and forward from the last returns to the first, so the far
    /// end of a long pipeline is always one key away.
    #[test]
    fn stage_selection_wraps_around_both_ends_of_the_pipeline() {
        let mut details = details(RunStatus::Running, WorkflowKind::Standard);
        let first = details.stages[0].clone();
        for name in ["review", "decision"] {
            let mut stage = first.clone();
            stage.id = StageId::new(name).unwrap();
            details.stages.push(stage);
        }
        let (mut app, _fixture) = app_with(details);
        app.state.screen = Screen::RunDetail;
        assert_eq!(
            app.state.selected_stage_index, 0,
            "a run opens at its first stage"
        );

        app.handle_intent(Intent::Up);
        assert_eq!(
            app.state.selected_stage_index, 2,
            "stepping back from the first stage has to reach the last"
        );
        assert_eq!(
            app.state.selected_stage.as_ref().map(StageId::as_str),
            Some("decision"),
            "the selected stage id has to follow the index"
        );

        app.handle_intent(Intent::Down);
        assert_eq!(
            app.state.selected_stage_index, 0,
            "stepping forward from the last stage has to reach the first"
        );
    }

    /// Backspacing a long task one character at a time is the complaint that
    /// motivated the line kills; the composer's task field must answer them.
    #[test]
    fn the_composer_task_field_answers_the_readline_line_kills() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;
        app.state.new_run.focus = 0;
        app.state.new_run.task = crate::tui::state::TextField::new("rewrite the whole task");

        app.handle_intent(Intent::DeleteWordBefore);
        assert_eq!(app.state.new_run.task.text(), "rewrite the whole ");
        app.handle_intent(Intent::DeleteToStart);
        assert_eq!(
            app.state.new_run.task.text(),
            "",
            "Ctrl-U must clear the line, not one character"
        );
    }

    /// The composer's task belongs to the run it starts; reopening the
    /// composer must not offer a finished run's words as the next draft.
    #[test]
    fn a_dispatched_task_does_not_linger_in_the_composer() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;
        app.state.new_run.task =
            crate::tui::state::TextField::new("ship the follow-ups".to_owned());
        app.state.new_run.focus = 3;
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.busy_label().as_deref(), Some("starting run"));
        assert_eq!(
            app.state.new_run.task.text(),
            "",
            "the dispatched task must not become the next run's draft"
        );
        assert_eq!(
            app.state.new_run.focus, 0,
            "an empty composer starts at its task"
        );
        assert!(
            !app.state.new_run.repository.text().is_empty(),
            "the repository is a standing preference, not run content, and survives"
        );
    }

    /// Two actions on one run would race each other over the same durable
    /// state, so that pairing — and only that pairing — is still refused.
    #[test]
    fn a_second_action_on_the_same_run_is_refused_and_names_the_holder() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        let run_id = RunId::from_u128(7);

        assert!(app.dispatch(WorkerCommand::ResumeRun { run_id }));
        assert!(!app.dispatch(WorkerCommand::ApplyRun { run_id }));
        let message = app
            .state
            .message
            .as_ref()
            .expect("the refusal explains itself");
        assert!(
            message.text.contains("resuming run"),
            "the refusal names what is holding the run: {:?}",
            message.text
        );
        assert_eq!(app.state.in_flight.len(), 1, "nothing extra was started");
    }

    /// Concurrency is not unlimited. Each agent at work holds a worktree, a
    /// terminal session and a provider process, so the interface stops handing
    /// out more of them than it means to run at once.
    #[test]
    fn agents_stop_being_started_at_the_ceiling() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        for index in 0..CONCURRENT_AGENTS {
            let run_id = RunId::from_u128(100 + index as u128);
            assert!(
                app.dispatch(WorkerCommand::ResumeRun { run_id }),
                "agent {index} is within the ceiling"
            );
        }

        assert!(
            !app.dispatch(start_command()),
            "one more agent than the ceiling is refused"
        );
        let message = app
            .state
            .message
            .as_ref()
            .expect("the refusal explains itself");
        assert!(
            message.text.contains("already working"),
            "the refusal says why: {:?}",
            message.text
        );

        // The ceiling counts agents, not actions: the work that only touches
        // the store and the checkout still goes out, and stop most of all —
        // it is how the user gets back under the ceiling.
        assert!(app.dispatch(WorkerCommand::StopRun {
            run_id: RunId::from_u128(100)
        }));
        assert!(app.dispatch(WorkerCommand::DiscardRun {
            run_id: RunId::from_u128(200)
        }));
    }

    /// Stop is the way out of a run that has stopped making progress, so the
    /// action it is interrupting must never be the reason it cannot run.
    #[test]
    fn stop_is_never_held_back_by_the_action_it_interrupts() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        let run_id = RunId::from_u128(7);
        assert!(app.dispatch(WorkerCommand::ResumeRun { run_id }));

        app.handle_intent(Intent::Stop);

        assert_eq!(
            app.state.in_flight.len(),
            2,
            "the stop went out alongside the resume holding the run"
        );
        assert!(
            app.state
                .in_flight
                .iter()
                .any(|entry| entry.action == ActionKind::Stop),
            "the stop is one of them"
        );
    }

    /// Several runs at once means results arrive while the user is reading
    /// something else. A finished background run reports; it does not grab
    /// the screen the user chose.
    #[test]
    fn a_background_run_finishing_never_moves_the_user() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::Logs;
        let finished = other_run(
            details(RunStatus::Completed, WorkflowKind::Standard),
            RunId::from_u128(8),
        );

        app.handle_worker_result(WorkerResult {
            ticket: u64::MAX,
            action: ActionKind::Start,
            run_id: None,
            result: Ok(WorkerSuccess::Execution(crate::app::ExecutionReport {
                details: finished,
                committed_events: Vec::new(),
                outcome: crate::app::QuiescentState::Completed,
            })),
        });

        assert_eq!(
            app.state.screen,
            Screen::Logs,
            "the user stays where it was"
        );
        // The fixture store holds no runs, so the refresh that follows every
        // result clears the selection here. What matters is that the finished
        // run did not take it.
        assert_ne!(
            app.state.selected_run,
            Some(RunId::from_u128(8)),
            "a background run never selects itself"
        );
        let message = app.state.message.as_ref().expect("but it is announced");
        assert!(
            message.text.contains(&RunId::from_u128(8).to_string()),
            "the announcement says which run finished: {:?}",
            message.text
        );
    }

    /// One rung per press, in both directions: → is Enter's twin on a row the
    /// cursor is sitting on, ← is Esc's.
    #[test]
    fn the_arrows_walk_in_and_out_of_a_run() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::Runs;

        app.handle_intent(Intent::Right);
        assert_eq!(app.state.screen, Screen::RunDetail);

        app.handle_intent(Intent::Left);
        assert_eq!(app.state.screen, Screen::Runs);
    }

    /// The arrows are navigation on every screen that is not editing text —
    /// the viewers included, where ← is the only way back besides Esc.
    #[test]
    fn left_leaves_a_viewer_for_the_run_it_belongs_to() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::Logs;
        app.state.scroll = 12;

        app.handle_intent(Intent::Left);

        assert_eq!(app.state.screen, Screen::RunDetail);
        assert_eq!(app.state.scroll, 0);
    }

    /// The composer's fields are chosen and edited with the arrows, so it must
    /// keep them: leaving the screen there is Esc's job alone.
    #[test]
    fn the_composer_keeps_the_arrows_for_its_own_fields() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;

        app.handle_intent(Intent::Left);

        assert_eq!(app.state.screen, Screen::NewRun);
    }

    /// The run on screen is the one the user is waiting on, so its result
    /// still opens by itself.
    #[test]
    fn the_selected_run_finishing_still_opens_its_result() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.state.screen = Screen::Runs;

        app.handle_worker_result(WorkerResult {
            ticket: u64::MAX,
            action: ActionKind::Resume,
            run_id: Some(RunId::from_u128(7)),
            result: Ok(WorkerSuccess::Execution(crate::app::ExecutionReport {
                details: details(RunStatus::Completed, WorkflowKind::Standard),
                committed_events: Vec::new(),
                outcome: crate::app::QuiescentState::Completed,
            })),
        });

        assert_eq!(app.state.screen, Screen::RunDetail);
        assert_eq!(
            app.state.quiescent,
            Some(crate::app::QuiescentState::Completed)
        );
        assert!(
            app.state.in_flight.is_empty(),
            "the finished action stopped holding its run"
        );
    }

    /// An apply the moved checkout refused opens the rebase that answers it,
    /// with the reason, instead of leaving an error to decode.
    #[test]
    fn an_apply_refused_by_a_moved_checkout_offers_the_rebase() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let run_id = app.state.selected_run.unwrap();
        app.handle_worker_result(WorkerResult {
            ticket: u64::MAX,
            action: ActionKind::Apply,
            run_id: Some(run_id),
            result: Ok(WorkerSuccess::ApplyNeedsRebase {
                run_id,
                reason: "the checkout moved 2 commit(s) past".to_owned(),
            }),
        });
        assert_eq!(app.state.overlay, Some(Overlay::RebaseConfirm));
        assert_eq!(
            app.state.rebase_reason.as_deref(),
            Some("the checkout moved 2 commit(s) past")
        );

        // Looking elsewhere, the same refusal is only reported.
        app.state.overlay = None;
        app.state.screen = Screen::Runs;
        app.handle_worker_result(WorkerResult {
            ticket: u64::MAX,
            action: ActionKind::Apply,
            run_id: Some(run_id),
            result: Ok(WorkerSuccess::ApplyNeedsRebase {
                run_id,
                reason: "moved".to_owned(),
            }),
        });
        assert_eq!(app.state.overlay, None);
        assert!(app.state.message.is_some());
    }

    fn published(pull_request: crate::workspace::PullRequestStatus) -> WorkerResult {
        WorkerResult {
            ticket: u64::MAX,
            action: ActionKind::Publish,
            run_id: Some(RunId::from_u128(7)),
            result: Ok(WorkerSuccess::Published(
                crate::workspace::PublishReceipt {
                    branch: "senate/run-7".to_owned(),
                    commit: "abcdef1234567890".to_owned(),
                    pull_request,
                    note: None,
                },
                crate::app::ExecutionReport {
                    details: details(RunStatus::Completed, WorkflowKind::Standard),
                    committed_events: Vec::new(),
                    outcome: crate::app::QuiescentState::Completed,
                },
            )),
        }
    }

    /// A paste lands in the field that has the operator's attention, and a
    /// card open over the new-run form has it — the form behind stays as it
    /// was.
    #[test]
    fn a_paste_behind_an_overlay_leaves_the_new_run_form_alone() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;
        app.state.overlay = Some(Overlay::Help);
        app.handle_paste("stray text");
        assert_eq!(app.state.new_run.task.text(), "");

        app.state.overlay = None;
        app.handle_paste("line one\nline two");
        assert_eq!(app.state.new_run.task.text(), "line one line two");
    }

    /// A run finishing used to open its detail over whatever the operator
    /// was doing. Its data is refreshed wherever they are; only the runs
    /// list or its own detail, with nothing open, moves to show it.
    #[test]
    fn a_finished_run_never_pulls_the_operator_out_of_a_draft_or_a_viewer() {
        let (fixture, database, worktrees, _repo, finished) = real_completed_standard_run();
        for (screen, overlay) in [
            (Screen::NewRun, None),
            (Screen::Artifact, None),
            (Screen::Logs, None),
            (Screen::Diff, None),
            (Screen::Runs, Some(Overlay::Help)),
        ] {
            let mut app = tui_app_over(
                fixture.path(),
                database.clone(),
                worktrees.clone(),
                finished.clone(),
            );
            app.state.screen = screen;
            app.state.overlay = overlay;
            app.state
                .begin_action(ActionKind::Resume, Some(finished.id));
            app.handle_worker_result(WorkerResult {
                ticket: u64::MAX,
                action: ActionKind::Resume,
                run_id: Some(finished.id),
                result: Ok(WorkerSuccess::Execution(crate::app::ExecutionReport {
                    details: finished.clone(),
                    committed_events: Vec::new(),
                    outcome: crate::app::QuiescentState::Completed,
                })),
            });
            assert_eq!(app.state.screen, screen, "{screen:?} under {overlay:?}");
            assert_eq!(app.state.overlay, overlay, "{screen:?} under {overlay:?}");
        }

        let mut app = tui_app_over(fixture.path(), database, worktrees, finished.clone());
        app.state.screen = Screen::Runs;
        app.handle_worker_result(WorkerResult {
            ticket: u64::MAX,
            action: ActionKind::Resume,
            run_id: Some(finished.id),
            result: Ok(WorkerSuccess::Execution(crate::app::ExecutionReport {
                details: finished.clone(),
                committed_events: Vec::new(),
                outcome: crate::app::QuiescentState::Completed,
            })),
        });
        assert_eq!(
            app.state.screen,
            Screen::RunDetail,
            "the runs list still opens it"
        );
    }

    /// A start refused before its run exists used to take the task with it:
    /// the composer was cleared on dispatch. The words come back — but never
    /// over a newer draft, since starts overlap and fail in any order.
    #[test]
    fn a_start_refused_before_its_run_exists_gives_its_task_back() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        let refuse = |app: &mut TuiApp, ticket: u64| {
            app.state.begin_action(ActionKind::Start, None);
            app.handle_worker_result(WorkerResult {
                ticket,
                action: ActionKind::Start,
                run_id: None,
                result: Err("Repository has uncommitted changes.".to_owned()),
            });
        };
        app.state.start_drafts = vec![
            (1, "first task".to_owned()),
            (2, "second task".to_owned()),
            (3, "third task".to_owned()),
        ];

        refuse(&mut app, 2);
        assert_eq!(app.state.new_run.task.text(), "second task");
        let footer = app
            .state
            .message
            .as_ref()
            .expect("a footer error")
            .text
            .clone();
        assert!(footer.contains("back in the new run form"), "{footer}");

        refuse(&mut app, 1);
        assert_eq!(
            app.state.new_run.task.text(),
            "second task",
            "an older refusal never overwrites the draft in the form"
        );

        app.handle_start_progress(StartUpdate {
            ticket: 3,
            progress: StartProgress::PreparingWorkspace(RunId::from_u128(3)),
        });
        assert!(
            app.state.start_drafts.is_empty(),
            "a run that exists holds its own task"
        );
        app.state.new_run.task = super::super::state::TextField::default();
        refuse(&mut app, 3);
        assert_eq!(app.state.new_run.task.text(), "", "nothing to give back");
    }

    /// A start used to show nothing at all until it either ran or failed, and
    /// its pull request check can wait out a thirty-second network timeout.
    /// The card opens the moment the start is dispatched and follows the
    /// start's own steps.
    #[test]
    fn submitting_a_run_opens_the_starting_card_and_follows_its_steps() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;
        app.state.new_run.task = super::super::state::TextField::new(
            "Review https://github.a8c.com/Automattic/wpcom/pull/241491",
        );
        app.state.new_run.repository =
            super::super::state::TextField::new("/nonexistent/senate-test-repo");
        app.submit_new_run();

        assert_eq!(app.state.overlay, Some(Overlay::Starting));
        let ticket = app
            .state
            .starting
            .as_ref()
            .expect("a start to follow")
            .ticket;
        assert_eq!(app.state.busy_label().as_deref(), Some("starting run"));

        let checking = StartProgress::CheckingPullRequest {
            url: "https://github.a8c.com/Automattic/wpcom/pull/241491".to_owned(),
            host: "github.a8c.com".to_owned(),
        };
        app.handle_start_progress(StartUpdate {
            ticket: ticket + 1,
            progress: StartProgress::FindingCheckout {
                repository: "someone/else".to_owned(),
            },
        });
        assert_eq!(
            app.state.starting.as_ref().unwrap().progress,
            None,
            "another start's step is not this card's"
        );
        app.handle_start_progress(StartUpdate {
            ticket,
            progress: checking.clone(),
        });
        assert_eq!(
            app.state.starting.as_ref().unwrap().progress,
            Some(checking)
        );

        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        assert!(
            app.state.starting.is_some(),
            "hiding the card is not cancelling"
        );
    }

    /// Once the run is running the wait is over, and what the operator was
    /// waiting for is the run itself.
    #[test]
    fn a_running_start_closes_its_card_onto_the_new_run() {
        let (fixture, database, worktrees, _repo, started) = real_completed_standard_run();
        let mut app = tui_app_over(fixture.path(), database, worktrees, started.clone());
        app.state.screen = Screen::Runs;
        app.state.selected_run = None;
        app.state.details = None;
        app.state.overlay = Some(Overlay::Starting);
        app.state.starting = Some(StartInFlight {
            ticket: 4,
            task: "Review it".to_owned(),
            started: Instant::now(),
            progress: None,
        });
        let run_id = started.id;
        app.handle_start_progress(StartUpdate {
            ticket: 4,
            progress: StartProgress::Running(run_id),
        });
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.starting, None);
        assert_eq!(app.state.selected_run, Some(run_id));
        assert_eq!(app.state.screen, Screen::RunDetail);
        assert_eq!(
            app.state.details.as_ref().map(|details| details.id),
            Some(run_id)
        );
    }

    /// A refused start is the other way the wait ends, and its reason — with
    /// the fix on the lines after the first — lands on a card that stays put.
    #[test]
    fn a_refused_start_replaces_its_card_with_the_reason() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::Runs;
        app.state.overlay = Some(Overlay::Starting);
        app.state.starting = Some(StartInFlight {
            ticket: 4,
            task: "Review it".to_owned(),
            started: Instant::now(),
            progress: None,
        });
        app.state.begin_action(ActionKind::Start, None);
        let error = "Pull request https://github.a8c.com/Automattic/wpcom/pull/241491 cannot be read.\n  dial tcp: i/o timeout\n  Fix: restore access".to_owned();
        app.handle_worker_result(WorkerResult {
            ticket: 4,
            action: ActionKind::Start,
            run_id: None,
            result: Err(error.clone()),
        });

        assert_eq!(app.state.overlay, Some(Overlay::StartFailed));
        assert_eq!(app.state.starting, None, "the wait is over");
        assert!(app.state.in_flight.is_empty());
        assert_eq!(
            app.state
                .start_failure
                .as_ref()
                .map(|failure| failure.error.as_str()),
            Some(error.as_str())
        );
        assert!(
            app.state.message.is_none(),
            "the card speaks, not the footer"
        );

        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.start_failure, None);
        let footer = app
            .state
            .message
            .as_ref()
            .expect("the reason stays a moment")
            .text
            .clone();
        assert!(footer.contains("cannot be read"), "{footer}");
    }

    /// A refusal never covers a task the operator is already writing: with
    /// the new-run form open, it goes to the footer instead.
    #[test]
    fn a_refused_start_behind_the_new_run_form_goes_to_the_footer() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;
        app.state.overlay = None;
        app.state.starting = Some(StartInFlight {
            ticket: 4,
            task: "Review it".to_owned(),
            started: Instant::now(),
            progress: None,
        });
        app.state.begin_action(ActionKind::Start, None);
        app.handle_worker_result(WorkerResult {
            ticket: 4,
            action: ActionKind::Start,
            run_id: None,
            result: Err("Repository has uncommitted changes.".to_owned()),
        });
        assert_eq!(app.state.overlay, None);
        assert_eq!(app.state.screen, Screen::NewRun);
        let footer = app
            .state
            .message
            .as_ref()
            .expect("a footer error")
            .text
            .clone();
        assert!(footer.contains("uncommitted changes"), "{footer}");
    }

    /// Confirming a publish is the start of a wait, and the wait is shown:
    /// the card opens the moment the command is dispatched, times itself,
    /// and Esc hides it without stopping anything.
    #[test]
    fn confirming_a_publish_opens_the_publishing_card_and_esc_only_hides_it() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        // The confirmation itself needs a diff preview from a seeded store;
        // this is about what confirming does, so it starts from the card.
        app.state.overlay = Some(Overlay::PublishConfirm);
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, Some(Overlay::Publishing));
        assert_eq!(
            app.state.publishing.map(|p| p.run_id),
            Some(RunId::from_u128(7))
        );
        assert_eq!(
            app.state.busy_label().as_deref(),
            Some("publishing pull request")
        );

        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.overlay, None);
        assert!(
            app.state.publishing.is_some(),
            "hiding the card is not cancelling"
        );
        assert!(
            !app.state.in_flight.is_empty(),
            "the publish is still working"
        );
    }

    /// The URL is the result, and a result belongs on a card that stays put:
    /// the created pull request replaces the wait, and closing the card
    /// leaves the URL in the footer rather than losing it.
    #[test]
    fn a_created_pull_request_lands_on_a_card_that_holds_its_url() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.overlay = Some(Overlay::Publishing);
        app.state.publishing = Some(PublishInFlight {
            run_id: RunId::from_u128(7),
            started: Instant::now(),
        });
        app.state
            .begin_action(ActionKind::Publish, Some(RunId::from_u128(7)));

        app.handle_worker_result(published(crate::workspace::PullRequestStatus::Created(
            "https://example.invalid/pull/7".to_owned(),
        )));

        assert_eq!(app.state.overlay, Some(Overlay::Published));
        assert_eq!(app.state.publishing, None, "the wait is over");
        assert!(app.state.in_flight.is_empty());
        assert_eq!(
            app.state.published.as_ref().and_then(PublishOutcome::url),
            Some("https://example.invalid/pull/7")
        );
        assert!(
            app.state.message.is_none(),
            "the card speaks, not the footer"
        );

        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        let message = app
            .state
            .message
            .as_ref()
            .expect("the footer keeps the URL");
        assert_eq!(message.kind, UiMessageKind::Success);
        assert!(
            message.text.contains("https://example.invalid/pull/7"),
            "{}",
            message.text
        );
    }

    /// A publish that pushed but could not open the pull request, and one
    /// that failed outright, both get the card too: the branch name or the
    /// error is what the operator needs to read next, and a card does not
    /// expire.
    #[test]
    fn a_short_publish_still_reports_on_the_card() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state
            .begin_action(ActionKind::Publish, Some(RunId::from_u128(7)));
        app.handle_worker_result(published(crate::workspace::PullRequestStatus::Unavailable(
            "gh is not installed".to_owned(),
        )));
        assert_eq!(app.state.overlay, Some(Overlay::Published));
        assert_eq!(
            app.state.published.as_ref().and_then(PublishOutcome::url),
            None
        );
        app.handle_intent(Intent::Escape);
        let message = app.state.message.as_ref().unwrap();
        assert!(message.text.contains("senate/run-7"), "{}", message.text);
        assert!(
            message.text.contains("gh is not installed"),
            "{}",
            message.text
        );

        app.state
            .begin_action(ActionKind::Publish, Some(RunId::from_u128(7)));
        app.handle_worker_result(WorkerResult {
            ticket: u64::MAX,
            action: ActionKind::Publish,
            run_id: Some(RunId::from_u128(7)),
            result: Err("remote rejected the push".to_owned()),
        });
        assert_eq!(app.state.overlay, Some(Overlay::Published));
        assert!(
            app.state
                .published
                .as_ref()
                .is_some_and(PublishOutcome::is_failure)
        );
        app.handle_intent(Intent::Enter);
        let message = app.state.message.as_ref().unwrap();
        assert_eq!(message.kind, UiMessageKind::Error);
        assert!(
            message.text.contains("remote rejected the push"),
            "{}",
            message.text
        );
    }

    /// A card over the form the operator is typing into would cover their
    /// task; over another overlay it would cover their answer. The footer
    /// carries the result there, and the card is skipped, not queued.
    #[test]
    fn a_publish_finishing_while_the_operator_is_busy_elsewhere_stays_in_the_footer() {
        let (mut app, _fixture) = app_with(details(RunStatus::Completed, WorkflowKind::Standard));
        app.state.screen = Screen::NewRun;
        // A run other than the selected one: the selected run finishing
        // already opens its deck, as for every other action.
        app.state
            .begin_action(ActionKind::Publish, Some(RunId::from_u128(8)));
        let mut background = published(crate::workspace::PullRequestStatus::Created(
            "https://example.invalid/pull/7".to_owned(),
        ));
        background.run_id = Some(RunId::from_u128(8));
        if let Ok(WorkerSuccess::Published(_, report)) = &mut background.result {
            report.details.id = RunId::from_u128(8);
        }
        app.handle_worker_result(background);
        assert_eq!(app.state.screen, Screen::NewRun);
        assert_eq!(app.state.overlay, None, "no card over the form");
        assert!(app.state.published.is_none());
        let message = app.state.message.as_ref().expect("the footer announces it");
        assert!(
            message.text.contains("https://example.invalid/pull/7"),
            "{}",
            message.text
        );

        app.state.screen = Screen::RunDetail;
        app.state.overlay = Some(Overlay::Continue);
        app.state
            .begin_action(ActionKind::Publish, Some(RunId::from_u128(7)));
        app.handle_worker_result(published(
            crate::workspace::PullRequestStatus::AlreadyExists(
                "https://example.invalid/pull/7".to_owned(),
            ),
        ));
        assert_eq!(
            app.state.overlay,
            Some(Overlay::Continue),
            "the answer being typed wins"
        );
    }

    /// One label reads as itself; several would crowd out the run identity
    /// the header exists to show, so they are counted instead.
    #[test]
    fn the_header_names_one_action_and_counts_more() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        assert_eq!(app.state.busy_label(), None, "nothing to say when idle");

        assert!(app.dispatch(WorkerCommand::ResumeRun {
            run_id: RunId::from_u128(7)
        }));
        assert_eq!(app.state.busy_label().as_deref(), Some("resuming run"));

        assert!(app.dispatch(WorkerCommand::ResumeRun {
            run_id: RunId::from_u128(8)
        }));
        assert_eq!(
            app.state.busy_label().as_deref(),
            Some("2 actions in flight")
        );
    }

    #[test]
    fn quit_still_detaches_without_stopping_the_run() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.handle_intent(Intent::Quit);
        assert!(app.state.quit, "q still leaves the frontend");
        assert!(
            app.state.in_flight.is_empty(),
            "detaching never interrupts the run"
        );
    }

    #[test]
    fn stop_refusal_names_the_blocking_state() {
        for (status, expected) in [
            (RunStatus::Interrupted, "already stopped"),
            (RunStatus::Completed, "already finished"),
            (RunStatus::Discarded, "discarded"),
        ] {
            let (app, _fixture) = app_with(details(status, WorkflowKind::Standard));
            let reason = stop_unavailable_reason(&app.state);
            assert!(
                reason.contains(expected),
                "{status:?}: {reason:?} should mention {expected:?}"
            );
        }
    }

    #[test]
    fn technical_toggle_is_scoped_to_run_detail() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        assert!(!app.state.technical, "operational view is the default");
        app.handle_intent(Intent::TechnicalDetails);
        assert!(app.state.technical);
        app.handle_intent(Intent::TechnicalDetails);
        assert!(!app.state.technical);

        app.state.screen = Screen::Logs;
        app.handle_intent(Intent::TechnicalDetails);
        assert!(!app.state.technical, "viewers keep their own controls");
    }

    #[test]
    fn discard_confirmation_still_opens_for_any_run() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.handle_intent(Intent::Discard);
        assert_eq!(
            app.state.overlay,
            Some(Overlay::DiscardConfirm),
            "discard eligibility is unchanged by this milestone"
        );
    }

    #[test]
    fn viewer_identification_is_explicit() {
        assert!(is_viewer(Screen::Artifact));
        assert!(is_viewer(Screen::Logs));
        assert!(is_viewer(Screen::Diff));
        assert!(!is_viewer(Screen::RunDetail));
    }

    #[test]
    fn action_labels_are_stable_user_feedback() {
        assert_eq!(
            crate::tui::worker::ActionKind::Resume.label(),
            "resuming run"
        );
    }

    #[test]
    fn artifact_absence_severity_tracks_stage_state() {
        let stage_id = crate::domain::StageId::new("implementation").unwrap();
        let (kind, text) = artifact_unavailable_feedback(Some(StageStatus::Running), &stage_id);
        assert_eq!(kind, UiMessageKind::Info);
        assert!(text.contains("still running"));

        let (kind, _) = artifact_unavailable_feedback(Some(StageStatus::Pending), &stage_id);
        assert_eq!(kind, UiMessageKind::Info);

        let (kind, _) = artifact_unavailable_feedback(Some(StageStatus::Failed), &stage_id);
        assert_eq!(kind, UiMessageKind::Warning);
        let (kind, _) = artifact_unavailable_feedback(Some(StageStatus::NeedsUser), &stage_id);
        assert_eq!(kind, UiMessageKind::Warning);

        let (kind, text) = artifact_unavailable_feedback(Some(StageStatus::Completed), &stage_id);
        assert_eq!(
            kind,
            UiMessageKind::Error,
            "completed without artifact is real"
        );
        assert!(text.contains("completed but has no verified artifact"));
    }

    #[test]
    fn text_mode_allows_q_character() {
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(map_text_key(key), Intent::Character('q'));
    }

    fn listed(id: u128, status: RunStatus) -> RunListItem {
        RunListItem {
            id: RunId::from_u128(id),
            workflow: WorkflowKind::Standard,
            status,
            task_summary: "task".to_owned(),
            repository: None,
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            archived: false,
        }
    }

    fn holds_resume_for(app: &TuiApp, id: u128) -> bool {
        app.state.in_flight.iter().any(|entry| {
            entry.action == ActionKind::Resume && entry.run_id == Some(RunId::from_u128(id))
        })
    }

    #[test]
    fn first_refresh_resumes_runs_left_running_by_a_dead_instance() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        let runs = vec![
            listed(1, RunStatus::Running),
            listed(2, RunStatus::Completed),
            listed(3, RunStatus::Paused),
        ];

        app.recover_orphaned_runs(&runs);

        assert!(holds_resume_for(&app, 1), "the orphan gets a driver");
        assert!(!holds_resume_for(&app, 2), "finished runs are left alone");
        assert!(!holds_resume_for(&app, 3), "paused is the user's decision");
        assert!(
            app.state
                .message
                .as_ref()
                .is_some_and(|message| message.text.contains("previous session")),
            "the user hears why an agent started on its own"
        );
    }

    #[test]
    fn runs_turning_running_after_the_first_look_are_never_swept() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        app.recover_orphaned_runs(&[]);

        // A run started by this session shows up Running on a later refresh
        // while its own start action still has no run id to hold it with.
        app.recover_orphaned_runs(&[listed(4, RunStatus::Running)]);

        assert!(
            !holds_resume_for(&app, 4),
            "its driver lives here; a second one would race it"
        );
    }

    #[test]
    fn an_orphan_deferred_by_the_agent_ceiling_is_retried_until_it_starts() {
        let (mut app, _fixture) = app_with(details(RunStatus::Running, WorkflowKind::Standard));
        for _ in 0..CONCURRENT_AGENTS {
            app.state.begin_action(ActionKind::Start, None);
        }
        let runs = vec![listed(5, RunStatus::Running)];

        app.recover_orphaned_runs(&runs);
        assert!(!holds_resume_for(&app, 5), "no capacity yet");

        app.state.settle_action(ActionKind::Start, None);
        app.recover_orphaned_runs(&runs);
        assert!(holds_resume_for(&app, 5), "resumed once capacity frees up");

        app.recover_orphaned_runs(&runs);
        assert_eq!(
            app.state
                .in_flight
                .iter()
                .filter(|entry| entry.run_id == Some(RunId::from_u128(5)))
                .count(),
            1,
            "an orphan is resumed exactly once"
        );
    }

    /// When Codex refused the stage's model, a plain retry fails the same
    /// way, so the chooser opens on the fallback model row and Enter sends
    /// the stage there.
    #[test]
    fn retry_opens_on_the_fallback_model_when_codex_refused_the_model() {
        let mut failed = details(RunStatus::Failed, WorkflowKind::Fast);
        failed.stages[0].status = StageStatus::Failed;
        failed.stages[0].failure_reason = Some(
            "The 'gpt-6-sol' model is not supported when using Codex with a ChatGPT account."
                .to_owned(),
        );
        let (mut app, _fixture) = app_with(failed);

        app.handle_intent(Intent::Retry);
        let fallback = RetryRouteChoice::CodexModel(crate::app::CODEX_FALLBACK_MODEL);
        assert_eq!(app.state.retry_route_choice, fallback);
        assert_eq!(
            app.state.retry_route_choice.route(),
            Some(crate::app::RetryRoute::new(
                crate::app::UniformProvider::Codex,
                Some(crate::domain::ModelId::new(crate::app::CODEX_FALLBACK_MODEL).unwrap()),
            ))
        );
        app.handle_intent(Intent::Up);
        assert_eq!(app.state.retry_route_choice, RetryRouteChoice::Configured);
        app.handle_intent(Intent::Down);
        app.handle_intent(Intent::Down);
        assert_eq!(app.state.retry_route_choice, RetryRouteChoice::Claude);
    }

    /// `t` asks where the failed stage goes before retrying it. The first
    /// row is the configured provider, so Enter alone is the plain retry;
    /// the other rows record an override for this stage only.
    #[test]
    fn retry_opens_the_route_chooser_and_dispatches_the_chosen_provider() {
        let mut failed = details(RunStatus::Failed, WorkflowKind::Fast);
        failed.stages[0].status = StageStatus::Failed;
        let (mut app, _fixture) = app_with(failed);

        app.handle_intent(Intent::Retry);
        assert_eq!(app.state.overlay, Some(Overlay::RetryRoute));
        assert_eq!(app.state.retry_route_choice, RetryRouteChoice::Configured);
        assert!(
            app.state.in_flight.is_empty(),
            "choosing dispatches nothing"
        );

        app.handle_intent(Intent::Down);
        assert_eq!(app.state.retry_route_choice, RetryRouteChoice::Claude);
        assert_eq!(
            app.state.retry_route_choice.route(),
            Some(crate::app::RetryRoute::new(
                crate::app::UniformProvider::Claude,
                None
            ))
        );
        app.handle_intent(Intent::Up);
        assert_eq!(app.state.retry_route_choice, RetryRouteChoice::Configured);
        assert_eq!(app.state.retry_route_choice.route(), None);
        app.handle_intent(Intent::Up);
        assert_eq!(app.state.retry_route_choice, RetryRouteChoice::Codex);

        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        assert!(
            app.state
                .in_flight
                .iter()
                .any(|action| action.action == super::super::worker::ActionKind::Retry),
            "Enter dispatches the retry"
        );

        // A stage that is not failed has nothing to retry, so no chooser.
        let mut running = details(RunStatus::Running, WorkflowKind::Fast);
        running.stages[0].status = StageStatus::Running;
        let (mut app, _fixture) = app_with(running);
        app.handle_intent(Intent::Retry);
        assert_eq!(app.state.overlay, None);
    }

    /// The mission path from the control room: `M` lists missions, Enter
    /// opens one, `S` starts a ready package on the fake provider, Enter
    /// opens the package's run and Esc comes back to the mission, and `I`
    /// records the delivered package as integrated — readying the package
    /// that depended on it.
    #[test]
    #[allow(clippy::too_many_lines, reason = "one mission, start to finish")]
    fn a_mission_is_driven_from_the_tui_start_open_its_run_and_integrate() {
        let (fixture, database, worktrees, repo, _) = real_completed_standard_run();
        let missions = MissionService::new(database.clone(), worktrees.clone());
        let mission = missions
            .create_mission("Greeting tool", "greet by name", &repo)
            .unwrap();
        let contract = |title: &str| crate::domain::WorkPackageContract {
            title: title.to_owned(),
            goal: format!("deliver {title}"),
            rationale: String::new(),
            scope: String::new(),
            acceptance_criteria: vec![],
            verification: String::new(),
            workflow: WorkflowKind::Fast,
        };
        let core = crate::domain::WorkPackageId::new("core").unwrap();
        let docs = crate::domain::WorkPackageId::new("docs").unwrap();
        missions
            .add_package(
                mission.id,
                crate::app::NewWorkPackage {
                    id: core.clone(),
                    contract: contract("Core"),
                    dependencies: vec![],
                },
            )
            .unwrap();
        missions
            .add_package(
                mission.id,
                crate::app::NewWorkPackage {
                    id: docs.clone(),
                    contract: contract("Docs"),
                    dependencies: vec![core.clone()],
                },
            )
            .unwrap();
        let mut app = TuiApp {
            state: TuiState::new(fixture.path()),
            reader: RunService::new(database.clone(), worktrees.clone(), RuntimeProviderFactory),
            missions,
            worker: Worker::spawn(
                RunService::new(
                    database.clone(),
                    worktrees.clone(),
                    crate::app::DevelopmentFakeProviderFactory::new(fixture.path().join("runs")),
                ),
                MissionService::new(database, worktrees),
            ),
            update: mpsc::channel().1,
            installing: None,
            last_refresh: Instant::now(),
            started: Instant::now(),
            orphaned: None,
            titles: TitleLookups::default(),
        };

        app.handle_intent(Intent::Missions);
        assert_eq!(app.state.screen, Screen::Missions);
        assert_eq!(app.state.selected_mission, Some(mission.id));
        assert_eq!(app.state.missions.len(), 1);

        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.screen, Screen::MissionDetail);
        assert_eq!(app.state.selected_package, Some(core.clone()));

        // A package whose dependency is not integrated cannot start.
        app.handle_intent(Intent::Down);
        assert_eq!(app.state.selected_package, Some(docs.clone()));
        app.handle_intent(Intent::StartPackage);
        assert!(app.state.in_flight.is_empty());
        assert!(
            app.state
                .message
                .as_ref()
                .is_some_and(|message| message.text.contains("waits on")),
            "{:?}",
            app.state.message
        );
        app.handle_intent(Intent::Up);

        app.state.new_run.execution = ExecutionChoice::Fake;
        app.handle_intent(Intent::StartPackage);
        assert_eq!(app.state.overlay, Some(Overlay::Starting));
        assert!(
            app.state
                .in_flight
                .iter()
                .any(|entry| entry.action == ActionKind::StartPackage)
        );
        wait_for_worker(&mut app);
        assert_eq!(app.state.screen, Screen::MissionDetail);
        assert_eq!(app.state.overlay, None);
        let package = app.state.selected_package_summary().unwrap().clone();
        assert_eq!(package.status, WorkPackageStatus::Delivered);
        let run_id = package.current_run.expect("the package names its run");

        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.screen, Screen::RunDetail);
        assert_eq!(app.state.selected_run, Some(run_id));
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.screen, Screen::MissionDetail);
        assert!(!app.state.run_opened_from_mission);

        // Leaving the mission by another door forgets it: a run opened from
        // the Runs list goes back to the Runs list.
        app.handle_intent(Intent::Enter);
        app.handle_intent(Intent::NewRun);
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.screen, Screen::Runs);
        app.refresh();
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.screen, Screen::RunDetail);
        app.handle_intent(Intent::Escape);
        assert_eq!(app.state.screen, Screen::Runs);
        app.handle_intent(Intent::Missions);
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.screen, Screen::MissionDetail);
        assert_eq!(app.state.selected_package, Some(core.clone()));

        app.handle_intent(Intent::Integrate);
        assert_eq!(app.state.overlay, Some(Overlay::IntegrateConfirm));
        app.handle_intent(Intent::Enter);
        assert_eq!(app.state.overlay, None);
        wait_for_worker(&mut app);
        let mission = app.state.mission.as_ref().unwrap();
        assert_eq!(
            mission.package(&core).unwrap().status,
            WorkPackageStatus::Integrated
        );
        assert_eq!(
            mission.package(&docs).unwrap().status,
            WorkPackageStatus::Ready
        );
        assert!(
            app.state
                .message
                .as_ref()
                .is_some_and(|message| message.text == "core is in."),
            "{:?}",
            app.state.message
        );
    }

    /// Pumps the loop's receive steps until nothing is in flight.
    fn wait_for_worker(app: &mut TuiApp) {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !app.state.in_flight.is_empty() {
            assert!(Instant::now() < deadline, "worker result timed out");
            app.receive_start_progress();
            app.receive_worker_results();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
