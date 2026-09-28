//! Native Codex CLI adapter. No direct `OpenAI` API usage.

mod artifact;
mod command;
mod detection;
mod error;
mod prompt;
mod protocol;
pub(crate) mod session_meta;

use session_meta::ObservedRuntime;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::{
    EffortSetting, ModelId, ProviderId, ProviderSessionId, Role, StageKind, StageStatus,
};
use crate::engine::{Provider, ProviderError, ProviderPoll, ProviderRequest, ProviderSignal};
use crate::image::{ImageToolHost, ImageToolScope, ImageToolServerCommand, TOOL_NAME};
use crate::process::{
    ExitResult, ManagedProcessId, ManagedProcessStatus, OutputChunk, OutputStream, ProcessBackend,
    ProcessManager, TmuxBackend,
};
use crate::providers::stage_prompt;
use crate::providers::{
    ProviderCommit, ProviderSessionMutation, ProviderSessionRecord, ProviderSessionRecordId,
    ProviderSessionStatus, change_handoff,
};
use crate::store::{SqliteStore, process_root};

pub use detection::{CodexInstallation, suspicious_codex_environment};
pub use error::CodexProviderError;
use protocol::{CodexRecord, first_record};

const PROTOCOL_VERSION: u32 = 1;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Ceiling for one record. A single line larger than this fails the poll
/// rather than growing the read without bound.
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
/// Ceiling for one line held in memory while looking for the final agent
/// message. Generous against JSON escaping, and still far under the record
/// ceiling: a message whose record needs more than this could never be
/// persisted as an artifact anyway, so a longer line is skipped unread.
const MAX_MESSAGE_LINE_BYTES: u64 = 8 * 1024 * 1024;

pub struct CodexProvider<B = TmuxBackend> {
    id: ProviderId,
    installation: CodexInstallation,
    model: Option<ModelId>,
    effort: EffortSetting,
    manager: ProcessManager<B>,
    artifact_root: PathBuf,
    /// Where this process reads Codex's own session records, resolved once at
    /// construction. `None` disables the lookup entirely, which leaves the
    /// runtime unobserved rather than guessed.
    codex_home: Option<PathBuf>,
    /// Present only for a role the run's image-generation grant names.
    image_tool: Option<Arc<ImageToolHost>>,
}

impl CodexProvider<TmuxBackend> {
    /// Builds native adapter using discovered Codex CLI, tmux, and The Senate data root.
    ///
    /// # Errors
    /// Returns missing/auth/capability/process-path failures before execution starts.
    pub fn from_environment(model: Option<ModelId>) -> Result<Self, CodexProviderError> {
        let installation = CodexInstallation::discover()?;
        installation.require_authenticated()?;
        let root = process_root()?;
        Ok(Self {
            id: ProviderId::new("codex")
                .map_err(|error| CodexProviderError::Protocol(error.to_string()))?,
            installation,
            model,
            effort: EffortSetting::NativeDefault,
            manager: ProcessManager::from_environment()?,
            artifact_root: root,
            codex_home: session_meta::home_from_environment(),
            image_tool: None,
        })
    }

    pub(crate) fn from_runtime(
        model: Option<ModelId>,
        root: PathBuf,
        runner_executable: PathBuf,
    ) -> Result<Self, CodexProviderError> {
        let installation = CodexInstallation::discover()?;
        installation.require_authenticated()?;
        Ok(Self {
            id: ProviderId::new("codex")
                .map_err(|error| CodexProviderError::Protocol(error.to_string()))?,
            installation,
            model,
            effort: EffortSetting::NativeDefault,
            manager: ProcessManager::new(&root, TmuxBackend::new(runner_executable)),
            artifact_root: root,
            codex_home: session_meta::home_from_environment(),
            image_tool: None,
        })
    }
}

impl<B> CodexProvider<B> {
    /// Sets the requested effort translated onto the native
    /// `model_reasoning_effort` override. `NativeDefault` keeps invocations
    /// byte-identical to pre-effort policy.
    #[must_use]
    pub fn with_effort(mut self, effort: EffortSetting) -> Self {
        self.effort = effort;
        self
    }

    /// The image tool this adapter carries, if its role holds the grant.
    /// `None` keeps every invocation byte-identical to before the tool
    /// existed.
    #[must_use]
    pub fn with_image_tool(mut self, host: Option<Arc<ImageToolHost>>) -> Self {
        self.image_tool = host;
        self
    }

    /// Arms the tool host for this stage on every poll of a granted stage.
    /// Idempotent, so a Senate process that restarted and re-attached to a
    /// running agent re-authorizes the same stage without a new invocation.
    fn arm_image_tool(&self, store: &SqliteStore, request: &ProviderRequest) {
        if let Some(host) = &self.image_tool
            && let Some(database) = store.database_path()
        {
            host.activate(ImageToolScope {
                run_id: request.run_id(),
                stage_id: request.stage_id().clone(),
                attempt: request.attempt(),
                role: request.role(),
                worktree: request.workspace_path().to_path_buf(),
                database,
            });
        }
    }

    /// What the native CLI must launch to reach the tool, when granted.
    fn image_server_command(&self) -> Result<Option<ImageToolServerCommand>, std::io::Error> {
        self.image_tool
            .as_ref()
            .map(|host| host.server_command())
            .transpose()
    }

    /// Prompt addendum for a granted stage: the bound and what remains, read
    /// from the store so a resumed process states the truth.
    fn image_prompt_section(&self, store: &SqliteStore, request: &ProviderRequest) -> String {
        let Some(host) = &self.image_tool else {
            return String::new();
        };
        let max = host.service().max_generations();
        let used = store
            .count_image_generations(request.run_id())
            .unwrap_or(max);
        stage_prompt::image_tool_section(TOOL_NAME, max, max.saturating_sub(used))
    }
}

impl<B: ProcessBackend> CodexProvider<B> {
    #[must_use]
    pub const fn installation(&self) -> &CodexInstallation {
        &self.installation
    }

    fn now() -> DateTime<Utc> {
        std::time::SystemTime::now().into()
    }

    /// What Codex's own session record says this thread ran.
    ///
    /// Returns `None` when no home is configured, no rollout matches the
    /// thread, or the record names neither fact. An unobserved runtime stays
    /// unobserved: nothing here falls back to the configured model, because
    /// "what we asked for" is not evidence of "what ran".
    fn observe_runtime(&self, session: &ProviderSessionRecord) -> Option<ObservedRuntime> {
        let home = self.codex_home.as_deref()?;
        let thread = session.native_session_id()?;
        session_meta::observe(home, thread.as_str())
    }

    /// A follow-up stage's operator instruction, persisted by
    /// [`crate::app::RunService::request_continue`] under the shared process
    /// root before this stage's initial invocation ever runs. `None` for
    /// every other stage kind, which never had one to write.
    fn continue_instruction(
        &self,
        request: &ProviderRequest,
    ) -> Result<Option<String>, CodexProviderError> {
        if !matches!(request.stage_kind(), StageKind::FollowUp | StageKind::Lead) {
            return Ok(None);
        }
        Ok(crate::providers::continue_instruction::read(
            &self.artifact_root,
            request.run_id(),
            request.stage_id(),
        )?)
    }

    fn final_message_path(
        &self,
        request: &ProviderRequest,
        session: &ProviderSessionRecord,
        invocation: u32,
    ) -> PathBuf {
        self.artifact_root
            .join(request.run_id().to_string())
            .join("provider-output")
            .join("codex")
            .join(session.id().to_string())
            .join(format!("invocation-{invocation}.md"))
    }

    fn start_invocation(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
        mut session: ProviderSessionRecord,
    ) -> Result<ProviderPoll, CodexProviderError> {
        let invocation = session
            .invocation()
            .checked_add(1)
            .ok_or_else(|| CodexProviderError::Protocol("invocation overflow".to_owned()))?;
        if let Some(orphan) = store
            .load_managed_process_for_attempt(
                request.run_id(),
                request.stage_id(),
                request.attempt(),
            )?
            .filter(|process| {
                process.invocation() == invocation
                    && session.current_process_id() != Some(process.id())
            })
        {
            if orphan.status() != ManagedProcessStatus::Preparing {
                return Err(CodexProviderError::Protocol(
                    "unbound provider invocation is not safely restartable".to_owned(),
                ));
            }
            let expected = session.revision();
            session
                .bind_process(orphan.id(), invocation, Self::now())
                .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
            store.update_provider_session(&session, expected)?;
            self.manager.start(store, orphan.id())?;
            return Ok(ProviderPoll::Pending);
        }

        let final_message_path = self.final_message_path(request, &session, invocation);
        create_private_parent(&final_message_path)?;
        let image = self.image_server_command()?;
        let command = if let Some(native) = session.native_session_id() {
            command::resume(
                native,
                &prompt::continuation(request),
                request.stage_kind(),
                self.model.as_ref(),
                self.effort,
                &final_message_path,
                image.as_ref(),
            )
        } else {
            let artifacts = store.list_artifacts(request.run_id())?;
            let handoff = change_handoff::for_request(store, request)?;
            let continue_instruction = self.continue_instruction(request)?;
            let mut prompt = prompt::compose(
                request,
                &artifacts,
                handoff.as_ref(),
                continue_instruction.as_deref(),
            )?;
            prompt.push_str(&self.image_prompt_section(store, request));
            command::initial(
                &prompt,
                request.stage_kind(),
                self.model.as_ref(),
                self.effort,
                &final_message_path,
                image.as_ref(),
            )
        };
        debug_assert_eq!(command.final_message_path, final_message_path);
        let process = self.manager.prepare_with_input(
            store,
            request.run_id(),
            request.stage_id().clone(),
            request.attempt(),
            invocation,
            self.installation.executable(),
            command.argv,
            BTreeMap::new(),
            &command.stdin,
        )?;
        let expected = session.revision();
        session
            .bind_process(process.id(), invocation, Self::now())
            .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
        let session = store.update_provider_session(&session, expected)?;
        self.manager.start(store, process.id())?;
        debug_assert_eq!(session.current_process_id(), Some(process.id()));
        Ok(ProviderPoll::Pending)
    }

    fn poll_session(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
        session: ProviderSessionRecord,
    ) -> Result<ProviderPoll, CodexProviderError> {
        // See the Claude adapter: an observing poll may never start work.
        if !request.observe_only()
            && (session.status() == ProviderSessionStatus::Created
                || (session.status() == ProviderSessionStatus::Interrupted
                    && matches!(
                        request.stage_status(),
                        StageStatus::Ready | StageStatus::Running
                    )))
        {
            return self.start_invocation(store, request, session);
        }
        let Some(process_id) = session.current_process_id() else {
            if request.observe_only() {
                // Nothing was ever launched, so there is nothing to observe.
                return Ok(ProviderPoll::Pending);
            }
            return Err(CodexProviderError::Protocol(
                "provider session has no current process".to_owned(),
            ));
        };
        let inspection = self.manager.inspect(store, process_id)?;
        if matches!(
            inspection.process.status(),
            ManagedProcessStatus::Preparing | ManagedProcessStatus::Starting
        ) && !request.observe_only()
        {
            self.manager.start(store, process_id)?;
        }
        let inspection = self.manager.inspect(store, process_id)?;
        let chunk = self.read_record_chunk(store, process_id)?;
        if let Some((record, consumed)) = first_record(chunk.bytes())? {
            let consumed = u64::try_from(consumed)
                .map_err(|_| CodexProviderError::Protocol("record size overflow".to_owned()))?;
            let end = chunk
                .start_offset()
                .checked_add(consumed)
                .ok_or_else(|| CodexProviderError::Protocol("output offset overflow".to_owned()))?;
            let successful_exit = inspection.exit_evidence.as_ref().is_some_and(|evidence| {
                matches!(evidence.result(), ExitResult::ExitCode { code: 0 })
            });
            return self.map_record(
                store,
                request,
                session,
                chunk,
                end,
                record,
                inspection.process.status(),
                successful_exit,
            );
        }
        if inspection.process.status().is_active() {
            return Ok(ProviderPoll::Pending);
        }
        // Execution cannot act on half a record, so it says so. Observation
        // can: a truncated record is how this process died, not a reason to
        // refuse to stop the run. Raising here instead leaves a run whose
        // Codex process was killed mid-write permanently unstoppable.
        if !chunk.bytes().is_empty() && !request.observe_only() {
            return Err(CodexProviderError::Protocol(
                "Codex process ended with incomplete JSON record".to_owned(),
            ));
        }
        Self::map_terminal_without_result(
            store,
            request,
            session,
            chunk,
            inspection.process.status(),
        )
    }

    /// Reads unacknowledged stdout, widening the window whenever it fills
    /// without containing a newline. A record only completes at a newline, so
    /// a saturated window without one can never yield a record no matter how
    /// often the same-sized read is retried — a single Codex `item.completed`
    /// carrying a full typecheck log has exceeded one window in practice,
    /// which stalled the run for good.
    fn read_record_chunk(
        &self,
        store: &SqliteStore,
        process_id: ManagedProcessId,
    ) -> Result<OutputChunk, CodexProviderError> {
        let mut max_bytes = MAX_OUTPUT_BYTES;
        loop {
            let chunk =
                self.manager
                    .read_output(store, process_id, OutputStream::Stdout, max_bytes)?;
            let saturated = chunk.bytes().len() == max_bytes;
            if !saturated || chunk.bytes().contains(&b'\n') || max_bytes >= MAX_RECORD_BYTES {
                return Ok(chunk);
            }
            max_bytes = MAX_RECORD_BYTES.min(max_bytes.saturating_mul(2));
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "mapping needs exact raw checkpoint plus reconciled process evidence"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "one match keeps every native record's session, artifact and signal effects together"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "one match keeps every native record's session, artifact and signal effects together"
    )]
    fn map_record(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
        mut session: ProviderSessionRecord,
        chunk: OutputChunk,
        end: u64,
        record: CodexRecord,
        process_status: ManagedProcessStatus,
        successful_exit: bool,
    ) -> Result<ProviderPoll, CodexProviderError> {
        let expected = session.revision();
        let mut commit = ProviderCommit::new(chunk, end);
        let signals = match record {
            CodexRecord::ThreadStarted { thread_id } => {
                let native = ProviderSessionId::new(thread_id)
                    .map_err(|error| CodexProviderError::Protocol(error.to_string()))?;
                if let Some(previous) = session.native_session_id() {
                    if previous != &native {
                        return Err(CodexProviderError::SessionMismatch {
                            expected: previous.to_string(),
                            actual: native.to_string(),
                        });
                    }
                    if session.status() != ProviderSessionStatus::Starting {
                        return Ok(ProviderPoll::Checkpoint(commit));
                    }
                }
                session
                    .activate(native.clone(), None, Self::now())
                    .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
                commit = commit.with_session(ProviderSessionMutation::new(session, expected));
                vec![if request.signal_index() == 0 {
                    ProviderSignal::Started {
                        model_id: None,
                        session_id: Some(native),
                    }
                } else {
                    ProviderSignal::Resumed
                }]
            }
            CodexRecord::Progress(message) => {
                if session.native_session_id().is_none() {
                    return Err(CodexProviderError::Protocol(
                        "Codex progress preceded thread.started".to_owned(),
                    ));
                }
                vec![ProviderSignal::Progress(message)]
            }
            CodexRecord::TurnCompleted(usage) => {
                if session.native_session_id().is_none() {
                    return Err(CodexProviderError::Protocol(
                        "Codex completion preceded thread.started".to_owned(),
                    ));
                }
                if process_status.is_active() {
                    return Ok(ProviderPoll::Pending);
                }
                let final_path = self.final_message_path(request, &session, session.invocation());
                // A `turn.completed` line retained in stdout is not by itself
                // proof that the turn finished: the same bytes are there after
                // the CLI was killed, rebooted out from under us, or died
                // writing its result. A clean exit is the cheap proof, and
                // when there is one nothing below runs.
                //
                // When the process is gone without that proof, the
                // `--output-last-message` file decides — but only if it holds
                // the whole final message, which is what the retained stream
                // is cross-checked for. Real Codex reports its final message
                // twice, once as an `item.completed` agent message and once in
                // that file, byte for byte; requiring the two to agree proves
                // the file is complete rather than merely present, so a
                // process killed part-way through writing it leaves a prefix
                // that cannot pass. Two independent writes agreeing is also
                // strong evidence the turn genuinely produced this result, and
                // trusting it costs nothing: the artifact still goes through
                // the same write-once, fsync'd, hash-verified path as a clean
                // exit. Refusing instead is what is expensive — the same
                // retained record is re-read and re-rejected on every later
                // poll, so the stage stays Running for good and finished work
                // is reachable only by discarding it with a retry.
                //
                // Anything less — no file, a truncated one, or a stream with
                // no final message to check it against — is not trusted, and
                // this reports the neutral fact that the process is gone
                // instead. Interruption is what the adapter already says about
                // process loss after a native thread exists, and recovery
                // knows how to resume it.
                //
                // Only the deaths this reasoning covers get that treatment.
                // A death The Senate itself caused, or one its supervisor never
                // understood, is a different fact and keeps its own answer: a
                // stop is reported as the interruption it is without consulting
                // any file, and `Broken` — the runner failing rather than
                // Codex — stays the infrastructure error it has always been,
                // because a completion mapped over it would hide the failure.
                //
                // Either way exactly one record is consumed in exactly one
                // durable transaction, so a crash before the commit replays
                // this same line against the same evidence and reaches the
                // same decision.
                let trusted = match process_status {
                    ManagedProcessStatus::Exited if successful_exit => true,
                    // Exited: a non-zero code or a signal. Missing: gone with
                    // no evidence at all, such as a reboot or a lost server.
                    ManagedProcessStatus::Exited | ManagedProcessStatus::Missing => {
                        let process = store.load_managed_process(commit.output().process_id())?;
                        final_message_corroborates(&final_path, process.spec().stdout_path(), end)
                    }
                    ManagedProcessStatus::Interrupted => false,
                    _ => {
                        return Err(CodexProviderError::Protocol(format!(
                            "Codex emitted turn.completed but process ended as {process_status:?} without successful exit evidence"
                        )));
                    }
                };
                if !trusted {
                    let uncorroborated = if process_status == ManagedProcessStatus::Interrupted {
                        ""
                    } else {
                        ", and no final message file corroborates the turn"
                    };
                    session
                        .interrupt(Self::now())
                        .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
                    commit = commit.with_session(ProviderSessionMutation::new(session, expected));
                    return Ok(ProviderPoll::Emission {
                        signals: vec![
                            ProviderSignal::Progress(format!(
                                "Codex emitted turn.completed but process ended as {process_status:?} without successful exit evidence{uncorroborated}"
                            )),
                            ProviderSignal::Interrupted,
                        ],
                        commit,
                    });
                }
                // Codex's stream never names the model or the reasoning
                // effort it resolved. Its own session record does, and by the
                // time `turn.completed` arrives that record is written, so
                // this is the first moment the fact is observable at all.
                let observed = self.observe_runtime(&session);
                if let Some(model) = observed.as_ref().and_then(|it| it.model.clone()) {
                    session
                        .confirm_model(model, Self::now())
                        .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
                }
                let workspace = store.load_workspace(request.run_id())?.ok_or_else(|| {
                    CodexProviderError::Protocol("run workspace disappeared".to_owned())
                })?;
                let artifact = artifact::persist(
                    &self.artifact_root,
                    &final_path,
                    request,
                    &self.id,
                    session.model_id(),
                    workspace.base_commit(),
                    Self::now(),
                )?;
                session
                    .complete(Self::now())
                    .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
                commit = commit
                    .with_session(ProviderSessionMutation::new(session, expected))
                    .with_artifact(artifact);
                let mut signals = Vec::new();
                if let Some(observed) = observed {
                    signals.push(ProviderSignal::RuntimeObserved {
                        model_id: observed.model,
                        native_effort: observed.effort,
                    });
                }
                signals.push(ProviderSignal::Usage(usage));
                signals.push(ProviderSignal::Completed);
                signals
            }
            CodexRecord::Failed(message) => {
                if session.native_session_id().is_none() {
                    return Err(CodexProviderError::MissingThreadId(message));
                }
                session
                    .fail(Self::now())
                    .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
                commit = commit.with_session(ProviderSessionMutation::new(session, expected));
                vec![ProviderSignal::Failed(message)]
            }
            CodexRecord::Ignored => return Ok(ProviderPoll::Checkpoint(commit)),
        };
        Ok(ProviderPoll::Emission { signals, commit })
    }

    fn map_terminal_without_result(
        store: &mut SqliteStore,
        request: &ProviderRequest,
        mut session: ProviderSessionRecord,
        chunk: OutputChunk,
        status: ManagedProcessStatus,
    ) -> Result<ProviderPoll, CodexProviderError> {
        // Execution cannot continue a thread Codex never started, so it says
        // so. Observation can: a stop is asking what happened, and "the
        // invocation died before it ever started" is an answer, not a failure.
        // Raising here instead would make the run unstoppable.
        if session.native_session_id().is_none() && !request.observe_only() {
            if matches!(
                status,
                ManagedProcessStatus::Interrupted | ManagedProcessStatus::Missing
            ) {
                let expected = session.revision();
                session
                    .interrupt(Self::now())
                    .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
                store.update_provider_session(&session, expected)?;
            }
            return Err(CodexProviderError::MissingThreadId(format!(
                "process ended as {status:?} before Codex emitted thread.started for {}",
                request.stage_id()
            )));
        }
        let expected = session.revision();
        let end = chunk.end_offset();
        let signal = if matches!(
            status,
            ManagedProcessStatus::Interrupted | ManagedProcessStatus::Missing
        ) {
            session
                .interrupt(Self::now())
                .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
            ProviderSignal::Interrupted
        } else {
            session
                .fail(Self::now())
                .map_err(|error| CodexProviderError::Protocol(error.to_owned()))?;
            ProviderSignal::Failed(format!(
                "Codex process ended as {status:?} without turn completion for {}",
                request.stage_id()
            ))
        };
        Ok(ProviderPoll::Emission {
            signals: vec![signal],
            commit: ProviderCommit::new(chunk, end)
                .with_session(ProviderSessionMutation::new(session, expected)),
        })
    }
}

impl<B: ProcessBackend> Provider for CodexProvider<B> {
    fn provider_id_for(&self, _request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
        Ok(self.id.clone())
    }

    fn supports_role(&self, _role: Role) -> bool {
        true
    }

    fn keep_attached_for(&self, _request: &ProviderRequest) -> Result<bool, ProviderError> {
        Ok(true)
    }

    fn stage_continue_instruction(
        &mut self,
        _store: &mut SqliteStore,
        run_id: crate::domain::RunId,
        stage_id: &crate::domain::StageId,
        _role: Role,
        instruction: &str,
    ) -> Result<(), ProviderError> {
        crate::providers::continue_instruction::write_once(
            &self.artifact_root,
            run_id,
            stage_id,
            instruction,
        )
        .map_err(|error| ProviderError::new(error.to_string()))
    }

    fn discard_continue_instruction(
        &mut self,
        _store: &mut SqliteStore,
        run_id: crate::domain::RunId,
        stage_id: &crate::domain::StageId,
    ) -> Result<(), ProviderError> {
        crate::providers::continue_instruction::discard(&self.artifact_root, run_id, stage_id)
            .map_err(|error| ProviderError::new(error.to_string()))
    }

    fn poll(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
    ) -> Result<ProviderPoll, ProviderError> {
        self.arm_image_tool(store, request);
        let result = (|| -> Result<ProviderPoll, CodexProviderError> {
            match store.load_provider_session_for_attempt(
                request.run_id(),
                request.stage_id(),
                request.attempt(),
            )? {
                Some(session) if session.provider_id() != &self.id => {
                    Err(CodexProviderError::Protocol(
                        "persisted provider session belongs to another provider".to_owned(),
                    ))
                }
                Some(session) => self.poll_session(store, request, session),
                None => {
                    let session = ProviderSessionRecord::new(
                        ProviderSessionRecordId::new(),
                        request.run_id(),
                        request.stage_id().clone(),
                        request.attempt(),
                        self.id.clone(),
                        PROTOCOL_VERSION,
                        Some(self.installation.version().to_owned()),
                        Self::now(),
                    );
                    let session = store.insert_provider_session(&session)?;
                    self.start_invocation(store, request, session)
                }
            }
        })();
        result.map_err(|error| ProviderError::new(error.to_string()))
    }
}

/// The text of the last completed agent message in `path` before `end`.
///
/// `end` is where the `turn.completed` record being decided on ends, so this
/// only ever sees messages that preceded it. Lines are read one at a time and
/// a line too large to hold a persistable message is skipped rather than
/// buffered, which keeps a Codex log carrying a whole build's output bounded
/// here. Anything unreadable is absence of evidence, not an error: the caller
/// treats a `None` exactly as it treats a mismatch.
fn last_agent_message(path: &Path, end: u64) -> Option<String> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let mut line = Vec::new();
    let mut position = 0_u64;
    let mut last = None;
    while position < end {
        line.clear();
        let read = read_capped_line(&mut reader, &mut line).ok()?;
        if read == 0 {
            break;
        }
        position = position.saturating_add(read);
        if line.last() == Some(&b'\n')
            && let Some(text) = protocol::agent_message_text(&line)
        {
            last = Some(text);
        }
    }
    last
}

/// Reads up to [`MAX_MESSAGE_LINE_BYTES`] of one line, discarding the rest of
/// a longer one, and returns how many bytes of the stream it covered. A line
/// that came back without its newline was either over-long or is the
/// unterminated tail of the file; either way it is not a record.
fn read_capped_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<u64> {
    let mut read = u64::try_from(
        reader
            .by_ref()
            .take(MAX_MESSAGE_LINE_BYTES)
            .read_until(b'\n', line)?,
    )
    .unwrap_or(u64::MAX);
    while read > 0 && line.last() != Some(&b'\n') {
        let mut discarded = Vec::new();
        let skipped = u64::try_from(
            reader
                .by_ref()
                .take(MAX_MESSAGE_LINE_BYTES)
                .read_until(b'\n', &mut discarded)?,
        )
        .unwrap_or(u64::MAX);
        read = read.saturating_add(skipped);
        if skipped == 0 || discarded.last() == Some(&b'\n') {
            break;
        }
    }
    Ok(read)
}

/// Whether the persisted `--output-last-message` file independently
/// corroborates a `turn.completed` whose process is already gone.
///
/// The file must hold exactly the last agent message the retained stream
/// carries, which is the invariant real Codex runs hold: the CLI writes its
/// final message to that file and reports the same text in an `item.completed`
/// agent-message record, byte for byte. Requiring the whole text is what makes
/// this a proof of completeness rather than of existence — a file the CLI was
/// killed part-way through writing is a strict prefix of that record, and a
/// prefix is not equal. The one tolerated difference is a trailing newline,
/// because [`artifact::persist`] appends the missing one and both spellings
/// therefore produce the identical artifact bytes.
///
/// A stream that carries no agent message reconstructs no final message, so it
/// corroborates nothing and the completion is not trusted.
///
/// The file is read through the artifact ceiling, never past it, and measured
/// the way the artifact writer measures it — trailing newline included. A file
/// that could not be persisted as an artifact whatever it says corroborates
/// nothing, and reading the rest of it to find that out is work a runaway or
/// malformed final message would get for free.
fn final_message_corroborates(final_path: &Path, stdout_path: &Path, end: u64) -> bool {
    let Some(file) = read_bounded(final_path, artifact::MAX_ARTIFACT_BYTES) else {
        return false;
    };
    if !artifact::fits_ceiling(&file) {
        return false;
    }
    let Some(message) = last_agent_message(stdout_path, end) else {
        return false;
    };
    !file.is_empty()
        && without_trailing_newline(&file) == without_trailing_newline(message.as_bytes())
}

/// The whole file, or `None` when it cannot be read or holds more than
/// `limit` bytes. Never buffers more than one byte past the limit, which is
/// what tells "exactly at the limit" from "over it".
fn read_bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let over_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    File::open(path)
        .ok()?
        .take(over_limit)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn without_trailing_newline(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

fn create_private_parent(path: &Path) -> Result<(), CodexProviderError> {
    let parent = path.parent().ok_or_else(|| {
        CodexProviderError::Protocol("final-message path has no parent".to_owned())
    })?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    // `File` and `Read` come from the adapter's own imports through the glob
    // below; importing them again here is redundant.
    use std::io::{Seek as _, SeekFrom};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    /// One implementation stage and nothing after it. The built-in Fast
    /// graph now verifies after implementing, and that stage belongs to the
    /// `verify` provider, not to the adapter under test.
    fn implementation_only() -> WorkflowDefinition {
        WorkflowDefinition::new(
            WorkflowKind::Fast,
            vec![crate::domain::StageDefinition::new(
                crate::domain::StageId::new("implementation").unwrap(),
                StageKind::Implementation,
                Role::Implementer,
                vec![],
            )],
        )
        .unwrap()
    }

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, DomainEventKind, EventId, EventMetadata, Run, RunStatus,
        WorkflowDefinition, WorkflowKind,
    };
    use crate::engine::{EngineStatus, WorkflowEngine};
    use crate::process::{
        BackendAvailability, BackendSessionId, BackendSessionState, ExitEvidence, ManagedProcess,
        ManagedProcessId, ProcessError, TerminationSignal,
    };
    use crate::store::{ResolvedConfigSnapshot, RunInput};
    use crate::workspace::WorkspaceManager;

    /// Real Codex reports its final message twice: as the last `agent_message`
    /// item and, byte for byte, in the `--output-last-message` file. The
    /// fixture holds that invariant, because corroboration depends on it.
    const FINAL_MESSAGE: &str = "# Codex result\nFixture";

    const SUCCESS_OUTPUT: &str = concat!(
        "{\"type\":\"thread.started\",\"thread_id\":\"codex-thread-1\"}\n",
        "{\"type\":\"turn.started\"}\n",
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"m1\",\"type\":\"agent_message\",\"text\":\"# Codex result\\nFixture\"}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"cached_input_tokens\":50,\"output_tokens\":20,\"reasoning_output_tokens\":5}}\n"
    );

    /// A turn that completed without ever emitting an agent message: there is
    /// no final text in the stream to check a file against.
    const OUTPUT_WITHOUT_MESSAGE: &str = concat!(
        "{\"type\":\"thread.started\",\"thread_id\":\"codex-thread-1\"}\n",
        "{\"type\":\"turn.started\"}\n",
        "{\"type\":\"item.completed\",\"item\":{\"id\":\"c1\",\"type\":\"command_execution\",\"command\":\"cargo test\"}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"cached_input_tokens\":50,\"output_tokens\":20,\"reasoning_output_tokens\":5}}\n"
    );

    #[derive(Clone)]
    struct FixtureBackend {
        started: Arc<Mutex<HashSet<ManagedProcessId>>>,
        completed: Arc<Mutex<HashSet<ManagedProcessId>>>,
        output: Arc<String>,
        /// What the process left behind when it ended. `None` is a process
        /// that disappeared without any evidence at all — a reboot, or a lost
        /// tmux server — which reconciles to `Missing`.
        exit: Option<ExitResult>,
        /// First invocation that writes the `--output-last-message` file.
        /// `None` never writes one, which is what a CLI killed before that
        /// write leaves behind.
        final_message_from_invocation: Option<u32>,
        /// What that write leaves in the file. Anything but [`FINAL_MESSAGE`]
        /// is a file the stream does not vouch for.
        final_message: Arc<String>,
    }

    #[derive(Clone, Default)]
    struct RecoveryBackend {
        started: Arc<Mutex<HashSet<ManagedProcessId>>>,
        inspections: Arc<Mutex<HashMap<ManagedProcessId, usize>>>,
        invocations: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl FixtureBackend {
        fn new(output: &str) -> Self {
            Self {
                started: Arc::new(Mutex::new(HashSet::new())),
                completed: Arc::new(Mutex::new(HashSet::new())),
                output: Arc::new(output.to_owned()),
                exit: Some(ExitResult::ExitCode { code: 0 }),
                final_message_from_invocation: Some(1),
                final_message: Arc::new(FINAL_MESSAGE.to_owned()),
            }
        }

        fn with_exit(mut self, exit: Option<ExitResult>) -> Self {
            self.exit = exit;
            self
        }

        fn writing_final_message_from(mut self, invocation: Option<u32>) -> Self {
            self.final_message_from_invocation = invocation;
            self
        }

        fn writing_final_message(mut self, message: &str) -> Self {
            self.final_message = Arc::new(message.to_owned());
            self
        }
    }

    impl ProcessBackend for FixtureBackend {
        fn kind(&self) -> &'static str {
            "fixture"
        }

        fn session_id(&self, process_id: ManagedProcessId) -> BackendSessionId {
            BackendSessionId::for_process(process_id)
        }

        fn availability(&self) -> Result<BackendAvailability, ProcessError> {
            Ok(BackendAvailability {
                kind: self.kind(),
                version: "fixture-1".to_owned(),
            })
        }

        fn start(&self, process: &ManagedProcess, _manifest: &Path) -> Result<(), ProcessError> {
            std::fs::write(process.spec().stdout_path(), self.output.as_bytes())?;
            let args = process.spec().argv();
            let output_index = args
                .iter()
                .position(|arg| arg == "--output-last-message")
                .expect("fixture command has final-message option");
            if self
                .final_message_from_invocation
                .is_some_and(|first| process.invocation() >= first)
            {
                std::fs::write(
                    PathBuf::from(&args[output_index + 1]),
                    self.final_message.as_bytes(),
                )?;
            }
            self.started.lock().unwrap().insert(process.id());
            Ok(())
        }

        fn inspect_session(
            &self,
            process: &ManagedProcess,
        ) -> Result<BackendSessionState, ProcessError> {
            if self.started.lock().unwrap().contains(&process.id()) {
                self.completed.lock().unwrap().insert(process.id());
            }
            Ok(BackendSessionState::Absent)
        }

        fn read_output(
            &self,
            process: &ManagedProcess,
            stream: OutputStream,
            offset: u64,
            max_bytes: usize,
        ) -> Result<OutputChunk, ProcessError> {
            let path = match stream {
                OutputStream::Stdout => process.spec().stdout_path(),
                OutputStream::Stderr => process.spec().stderr_path(),
            };
            let mut file = File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::new();
            file.take(u64::try_from(max_bytes).unwrap())
                .read_to_end(&mut bytes)?;
            OutputChunk::new(
                process.id(),
                stream,
                process.cursor(stream).revision(),
                offset,
                bytes,
            )
        }

        fn output_length(
            &self,
            process: &ManagedProcess,
            stream: OutputStream,
        ) -> Result<u64, ProcessError> {
            let path = match stream {
                OutputStream::Stdout => process.spec().stdout_path(),
                OutputStream::Stderr => process.spec().stderr_path(),
            };
            Ok(std::fs::metadata(path)?.len())
        }

        fn read_exit_evidence(
            &self,
            process: &ManagedProcess,
        ) -> Result<Option<ExitEvidence>, ProcessError> {
            let Some(result) = self.exit.clone() else {
                return Ok(None);
            };
            if !self.completed.lock().unwrap().contains(&process.id()) {
                return Ok(None);
            }
            let now = CodexProvider::<Self>::now();
            Ok(Some(ExitEvidence::new(
                process.id(),
                process.command_fingerprint().to_owned(),
                result,
                false,
                now,
                now,
            )))
        }

        fn signal(
            &self,
            _process: &ManagedProcess,
            _signal: TerminationSignal,
        ) -> Result<(), ProcessError> {
            Ok(())
        }

        fn cleanup(&self, _process: &ManagedProcess) -> Result<(), ProcessError> {
            Ok(())
        }
    }

    impl ProcessBackend for RecoveryBackend {
        fn kind(&self) -> &'static str {
            "recovery_fixture"
        }

        fn session_id(&self, process_id: ManagedProcessId) -> BackendSessionId {
            BackendSessionId::for_process(process_id)
        }

        fn availability(&self) -> Result<BackendAvailability, ProcessError> {
            Ok(BackendAvailability {
                kind: self.kind(),
                version: "fixture-1".to_owned(),
            })
        }

        fn start(&self, process: &ManagedProcess, _manifest: &Path) -> Result<(), ProcessError> {
            let args = process
                .spec()
                .argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            self.invocations.lock().unwrap().push(args.clone());
            let output = if process.invocation() == 1 {
                "{\"type\":\"thread.started\",\"thread_id\":\"recovery-thread\"}\n"
            } else {
                concat!(
                    "{\"type\":\"thread.started\",\"thread_id\":\"recovery-thread\"}\n",
                    "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":9,\"output_tokens\":4}}\n"
                )
            };
            std::fs::write(process.spec().stdout_path(), output)?;
            if process.invocation() == 2 {
                let output_index = args
                    .iter()
                    .position(|arg| arg == "--output-last-message")
                    .unwrap();
                std::fs::write(&args[output_index + 1], "Recovered result\n")?;
            }
            self.started.lock().unwrap().insert(process.id());
            Ok(())
        }

        fn inspect_session(
            &self,
            process: &ManagedProcess,
        ) -> Result<BackendSessionState, ProcessError> {
            if !self.started.lock().unwrap().contains(&process.id()) {
                return Ok(BackendSessionState::Absent);
            }
            if process.invocation() != 1 {
                return Ok(BackendSessionState::Absent);
            }
            let mut inspections = self.inspections.lock().unwrap();
            let count = inspections.entry(process.id()).or_default();
            let state = if *count == 0 {
                BackendSessionState::Owned
            } else {
                BackendSessionState::Absent
            };
            *count += 1;
            Ok(state)
        }

        fn read_output(
            &self,
            process: &ManagedProcess,
            stream: OutputStream,
            offset: u64,
            max_bytes: usize,
        ) -> Result<OutputChunk, ProcessError> {
            let path = match stream {
                OutputStream::Stdout => process.spec().stdout_path(),
                OutputStream::Stderr => process.spec().stderr_path(),
            };
            let mut file = File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::new();
            file.take(u64::try_from(max_bytes).unwrap())
                .read_to_end(&mut bytes)?;
            OutputChunk::new(
                process.id(),
                stream,
                process.cursor(stream).revision(),
                offset,
                bytes,
            )
        }

        fn output_length(
            &self,
            process: &ManagedProcess,
            stream: OutputStream,
        ) -> Result<u64, ProcessError> {
            let path = match stream {
                OutputStream::Stdout => process.spec().stdout_path(),
                OutputStream::Stderr => process.spec().stderr_path(),
            };
            Ok(std::fs::metadata(path)?.len())
        }

        fn read_exit_evidence(
            &self,
            process: &ManagedProcess,
        ) -> Result<Option<ExitEvidence>, ProcessError> {
            if process.invocation() != 2 || !self.started.lock().unwrap().contains(&process.id()) {
                return Ok(None);
            }
            let now = CodexProvider::<Self>::now();
            Ok(Some(ExitEvidence::new(
                process.id(),
                process.command_fingerprint().to_owned(),
                ExitResult::ExitCode { code: 0 },
                false,
                now,
                now,
            )))
        }

        fn signal(
            &self,
            _process: &ManagedProcess,
            _signal: TerminationSignal,
        ) -> Result<(), ProcessError> {
            Ok(())
        }

        fn cleanup(&self, _process: &ManagedProcess) -> Result<(), ProcessError> {
            Ok(())
        }
    }

    /// The credential boundary at the Codex adapter: a granted stage launches
    /// with the run-scoped server registered through root overrides, the
    /// exact tool pre-approved, and an empty environment override, so nothing
    /// the adapter does can put `OPENAI_API_KEY` in front of the native CLI.
    #[test]
    fn a_granted_stage_launches_with_the_tool_and_an_empty_environment_override() {
        use crate::image::{ImageToolHost, ImageToolService};

        let (temp, _database, run_id, mut store, provider) = fixture(SUCCESS_OUTPUT);
        let host = ImageToolHost::start(
            ImageToolService::new(temp.path().join("runs"), None, vec![Role::Implementer], 4),
            run_id,
        )
        .unwrap();
        let socket = host.socket_path().to_string_lossy().into_owned();
        let provider = provider.with_image_tool(Some(host));
        let mut engine = WorkflowEngine::new(provider, "landing page with a hero image");
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Finished {
                    run_status: RunStatus::Completed,
                } => break,
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }
        let processes = store.list_managed_processes(run_id).unwrap();
        assert_eq!(processes.len(), 1);
        let spec = processes[0].spec();
        assert!(
            spec.environment().is_empty(),
            "the adapter must not add any environment entry: {:?}",
            spec.environment().keys().collect::<Vec<_>>()
        );
        let argv = spec
            .argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let overrides = argv
            .windows(2)
            .filter(|pair| pair[0] == "-c")
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>();
        assert!(
            overrides.iter().any(|value| value
                == &format!(
                    "mcp_servers.senate_image.args=[\"__image-tool\",\"--socket\",\"{socket}\"]"
                )),
            "{overrides:?}"
        );
        assert!(overrides.iter().any(|value| {
            value == "mcp_servers.senate_image.tools.image_generate.approval_mode=\"approve\""
        }));
        assert!(!overrides.iter().any(|value| value.contains(".env")));
        let joined = argv.join("\n");
        assert!(!joined.contains("OPENAI"), "{joined}");
        let stdin = std::fs::read_to_string(spec.stdin_path().unwrap()).unwrap();
        assert!(stdin.contains("# Image generation"));
        assert!(!stdin.contains("OPENAI"));
        assert!(!stdin.contains("sk-"));
    }

    /// Without a grant the launch is byte-identical to before the tool
    /// existed: no server override and no prompt note.
    #[test]
    fn an_ungranted_stage_launches_exactly_as_before() {
        let (_temp, _database, run_id, mut store, provider) = fixture(SUCCESS_OUTPUT);
        let mut engine = WorkflowEngine::new(provider, "landing page with a hero image");
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Finished {
                    run_status: RunStatus::Completed,
                } => break,
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }
        let processes = store.list_managed_processes(run_id).unwrap();
        let spec = processes[0].spec();
        let joined = spec
            .argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("mcp_servers"), "{joined}");
        assert!(spec.environment().is_empty());
        let stdin = std::fs::read_to_string(spec.stdin_path().unwrap()).unwrap();
        assert!(!stdin.contains("Image generation"));
    }

    #[test]
    fn successful_turn_with_disappeared_session_and_durable_exit_completes_once() {
        let (_temp, database, run_id, mut store, provider) = fixture(SUCCESS_OUTPUT);
        let mut engine = WorkflowEngine::new(provider, "SUPER_SECRET_TASK_MARKER");
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Finished {
                    run_status: RunStatus::Completed,
                } => break,
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }

        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::Completed);
        assert_eq!(
            session.native_session_id().unwrap().as_str(),
            "codex-thread-1"
        );
        let process = store
            .load_managed_process(session.current_process_id().unwrap())
            .unwrap();
        let argv = process
            .spec()
            .argv()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(
            !argv
                .iter()
                .any(|arg| arg.contains("SUPER_SECRET_TASK_MARKER"))
        );
        assert!(
            std::fs::read_to_string(process.spec().stdin_path().unwrap())
                .unwrap()
                .contains("SUPER_SECRET_TASK_MARKER")
        );
        let events = store.load_events(run_id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.event.kind(),
                    DomainEventKind::ProviderUsageUpdated {
                        input_units: 100,
                        output_units: 20,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.event.kind(),
                    DomainEventKind::ProviderCompleted { .. }
                ))
                .count(),
            1
        );
        let artifacts = store.list_artifacts(run_id).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            artifacts[0].metadata().provider_id().unwrap().as_str(),
            "codex"
        );

        drop(store);
        let mut store = SqliteStore::open(database).unwrap();
        assert_eq!(
            store.load_run(run_id).unwrap().run.status(),
            RunStatus::Completed
        );
        assert_eq!(store.list_artifacts(run_id).unwrap().len(), 1);
    }

    #[test]
    fn turn_completion_transaction_failure_replays_batch_without_duplicate_usage() {
        let (_temp, _database, run_id, mut store, provider) = fixture(SUCCESS_OUTPUT);
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        assert_completion_survives_a_failed_transaction(&mut store, run_id, &mut engine);
    }

    /// A completion trusted on the strength of the final-message file takes the
    /// identical transaction, so the identical crash mid-commit must cost the
    /// run nothing: one replayed record, one artifact, one usage batch.
    #[test]
    fn corroborated_completion_after_failed_exit_replays_batch_without_duplicate_usage() {
        let (_temp, _database, run_id, mut store, provider) = fixture_with(
            FixtureBackend::new(SUCCESS_OUTPUT).with_exit(Some(ExitResult::ExitCode { code: 1 })),
        );
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        assert_completion_survives_a_failed_transaction(&mut store, run_id, &mut engine);
    }

    fn assert_completion_survives_a_failed_transaction(
        store: &mut SqliteStore,
        run_id: crate::domain::RunId,
        engine: &mut WorkflowEngine<CodexProvider<FixtureBackend>>,
    ) {
        loop {
            let before = store.load_events(run_id).unwrap();
            let next_is_completion = before.iter().any(|event| {
                matches!(event.event.kind(), DomainEventKind::ProviderProgress { .. })
            });
            if next_is_completion {
                break;
            }
            let _ = engine.tick(store, run_id).unwrap();
        }
        let session_before = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        let process_id = session_before.current_process_id().unwrap();
        let cursor_before = store
            .load_managed_process(process_id)
            .unwrap()
            .cursor(OutputStream::Stdout);
        let events_before = store.load_events(run_id).unwrap();
        store.install_event_insert_failure();
        assert!(engine.tick(store, run_id).is_err());
        store.remove_event_insert_failure();

        assert_eq!(store.load_events(run_id).unwrap(), events_before);
        assert_eq!(
            store.list_provider_sessions(run_id).unwrap().pop().unwrap(),
            session_before
        );
        assert_eq!(
            store
                .load_managed_process(process_id)
                .unwrap()
                .cursor(OutputStream::Stdout),
            cursor_before
        );
        assert!(store.list_artifacts(run_id).unwrap().is_empty());

        assert!(matches!(
            engine.tick(store, run_id).unwrap(),
            EngineStatus::Advanced { .. }
        ));
        let events = store.load_events(run_id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.event.kind(),
                    DomainEventKind::ProviderUsageUpdated { .. }
                ))
                .count(),
            1
        );
        assert_eq!(store.list_artifacts(run_id).unwrap().len(), 1);
    }

    /// Codex finished the turn and then the process died: a non-zero exit
    /// here, a machine rebooted out from under tmux there. The work itself is
    /// intact — the file holds the whole message the stream reported — so the
    /// completion stands. Refusing it strands finished work behind a protocol
    /// error that every later poll raises again.
    #[test]
    fn a_dead_process_completes_when_the_final_message_corroborates_the_turn() {
        for (exit, message) in [
            // Non-zero exit: the turn was done, the CLI's own teardown was not.
            (Some(ExitResult::ExitCode { code: 1 }), FINAL_MESSAGE),
            // No evidence at all — reboot, or a lost tmux server: `Missing`.
            (None, FINAL_MESSAGE),
            // A file that differs only by the newline the artifact writer
            // adds anyway is the same message, and yields the same artifact.
            (None, "# Codex result\nFixture\n"),
        ] {
            let (_temp, database, run_id, mut store, provider) = fixture_with(
                FixtureBackend::new(SUCCESS_OUTPUT)
                    .with_exit(exit)
                    .writing_final_message(message),
            );
            let mut engine = WorkflowEngine::new(provider, "fixture task");
            loop {
                match engine.drive(&mut store, run_id).unwrap() {
                    EngineStatus::Finished {
                        run_status: RunStatus::Completed,
                    } => break,
                    EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                    status => panic!("unexpected status: {status:?}"),
                }
            }

            let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
            assert_eq!(session.status(), ProviderSessionStatus::Completed);
            let artifacts = store.list_artifacts(run_id).unwrap();
            assert_eq!(artifacts.len(), 1);
            assert_eq!(
                artifacts[0].metadata().provider_id().unwrap().as_str(),
                "codex"
            );
            // The artifact is the corroborating file itself, through the same
            // write-once path a clean exit takes.
            assert_eq!(
                std::fs::read_to_string(artifacts[0].path()).unwrap(),
                "# Codex result\nFixture\n"
            );
            assert_eq!(
                store
                    .load_events(run_id)
                    .unwrap()
                    .iter()
                    .filter(|event| matches!(
                        event.event.kind(),
                        DomainEventKind::ProviderCompleted { .. }
                    ))
                    .count(),
                1
            );

            drop(store);
            let mut store = SqliteStore::open(database).unwrap();
            assert_eq!(
                store.load_run(run_id).unwrap().run.status(),
                RunStatus::Completed
            );
            assert_eq!(store.list_artifacts(run_id).unwrap().len(), 1);
        }
    }

    /// The scan behind that corroboration reads a real Codex log, where one
    /// `item.completed` can carry a whole build's output. It must step over a
    /// line too large to hold instead of wedging on it, and must never look
    /// past the record it is deciding on.
    #[test]
    fn the_final_message_scan_skips_an_oversized_line_and_stops_at_the_record() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("stdout.log");
        let oversized = "build output "
            .repeat(usize::try_from(MAX_MESSAGE_LINE_BYTES).unwrap() / "build output ".len() + 1);
        let later = "{\"type\":\"item.completed\",\"item\":{\"id\":\"m2\",\"type\":\"agent_message\",\"text\":\"a later turn\"}}\n";
        let stream = format!(
            concat!(
                "{{\"type\":\"item.completed\",\"item\":{{\"id\":\"c1\",\"type\":\"command_execution\",",
                "\"command\":\"yarn build\",\"aggregated_output\":\"{}\"}}}}\n",
                "{{\"type\":\"item.completed\",\"item\":{{\"id\":\"m1\",\"type\":\"agent_message\",",
                "\"text\":\"# Codex result\\nFixture\"}}}}\n",
                "{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}\n",
                "{}"
            ),
            oversized, later
        );
        std::fs::write(&path, &stream).unwrap();

        let end = u64::try_from(stream.len() - later.len()).unwrap();
        assert_eq!(
            last_agent_message(&path, end).as_deref(),
            Some(FINAL_MESSAGE)
        );
        // A message that arrives after this record is not this turn's.
        assert_eq!(last_agent_message(&path, 0), None);
    }

    /// A file the stream does not vouch for is not evidence of anything. The
    /// dangerous one is the truncation: a CLI killed part-way through writing
    /// its final message leaves a readable, non-empty prefix, and promoting
    /// that to a hash-verified artifact would feed downstream stages half a
    /// result that looks whole. Each of these must interrupt instead.
    #[test]
    fn a_final_message_the_stream_does_not_vouch_for_never_completes_the_turn() {
        let cases = [
            // Killed mid-write: a strict prefix of the real message.
            (SUCCESS_OUTPUT, "# Codex resu"),
            // Killed mid-write one byte short of the end.
            (SUCCESS_OUTPUT, "# Codex result\nFixtur"),
            // Not the message the stream reported at all.
            (SUCCESS_OUTPUT, "# Codex result\nSomething else entirely"),
            // Nothing in the stream to reconstruct a final message from.
            (OUTPUT_WITHOUT_MESSAGE, FINAL_MESSAGE),
        ];
        for (output, message) in cases {
            let (_temp, _database, run_id, mut store, provider) = fixture_with(
                FixtureBackend::new(output)
                    .with_exit(None)
                    .writing_final_message(message),
            );
            let mut engine = WorkflowEngine::new(provider, "fixture task");
            loop {
                match engine.drive(&mut store, run_id).unwrap() {
                    EngineStatus::Interrupted { stages } => {
                        assert_eq!(
                            stages,
                            vec![crate::domain::StageId::new("implementation").unwrap()]
                        );
                        break;
                    }
                    EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                    status => panic!("{message:?} must not complete the turn: {status:?}"),
                }
            }
            let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
            assert_eq!(session.status(), ProviderSessionStatus::Interrupted);
            assert!(
                store.list_artifacts(run_id).unwrap().is_empty(),
                "{message:?} became an artifact"
            );
        }
    }

    /// `Broken` is the supervisor saying it could not run or account for the
    /// process at all, not Codex saying anything. Corroborating a completion
    /// over it would dress an infrastructure failure up as finished work, or
    /// as an ordinary interruption to resume; it keeps raising instead, which
    /// is what it did before dead-process completions were trusted.
    #[test]
    fn a_broken_supervisor_keeps_failing_however_good_the_final_message_looks() {
        let (_temp, _database, run_id, mut store, provider) = fixture_with(
            FixtureBackend::new(SUCCESS_OUTPUT).with_exit(Some(ExitResult::RunnerError {
                message: "managed runner could not wait for the child".to_owned(),
            })),
        );
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        let failure = loop {
            match engine.drive(&mut store, run_id) {
                Ok(EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. }) => {}
                Ok(status) => panic!("a broken supervisor must not settle: {status:?}"),
                Err(error) => break error,
            }
        };
        assert!(
            failure.to_string().contains("Broken")
                && failure.to_string().contains("turn.completed"),
            "the infrastructure failure must still be reported: {failure}"
        );

        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::Active);
        assert!(store.list_artifacts(run_id).unwrap().is_empty());
        assert!(
            !store
                .load_events(run_id)
                .unwrap()
                .iter()
                .any(|event| matches!(
                    event.event.kind(),
                    DomainEventKind::ProviderCompleted { .. }
                        | DomainEventKind::ProviderInterrupted { .. }
                ))
        );
        // Still a failure on the next pass: nothing about it was consumed away.
        assert!(engine.drive(&mut store, run_id).is_err());
    }

    /// A final message too large to become an artifact is refused before it is
    /// read whole, and the ceiling is the one the artifact writer enforces —
    /// counting the newline it appends. A file of exactly the ceiling without
    /// that newline would be persisted one byte over it, so it must not pass;
    /// the same size with the newline already there fits exactly and must.
    #[test]
    fn the_final_message_ceiling_is_measured_as_the_artifact_writer_measures_it() {
        // At the ceiling and already terminated: persisted verbatim, exactly
        // the ceiling. Over it, and at it but needing the appended newline:
        // both would be persisted past the ceiling.
        let at_ceiling = format!("{}\n", "x".repeat(artifact::MAX_ARTIFACT_BYTES - 1));
        let unterminated = "x".repeat(artifact::MAX_ARTIFACT_BYTES);
        let over_ceiling = "x".repeat(artifact::MAX_ARTIFACT_BYTES + 1);

        // The stream reports the same bytes as the file, so only the ceiling
        // ever decides these.
        let stream = |message: &str| {
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"thread.started","thread_id":"codex-thread-1"}),
                json!({"type":"item.completed","item":{"id":"m1","type":"agent_message","text":message}}),
                json!({"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":20}}),
            )
        };

        for (message, completes) in [
            (at_ceiling.as_str(), true),
            (unterminated.as_str(), false),
            (over_ceiling.as_str(), false),
        ] {
            let output = stream(message);
            let (_temp, _database, run_id, mut store, provider) = fixture_with(
                FixtureBackend::new(&output)
                    .with_exit(None)
                    .writing_final_message(message),
            );
            let mut engine = WorkflowEngine::new(provider, "fixture task");
            let outcome = loop {
                match engine.drive(&mut store, run_id).unwrap() {
                    EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                    status => break status,
                }
            };
            let artifacts = store.list_artifacts(run_id).unwrap();
            if completes {
                assert!(
                    matches!(
                        outcome,
                        EngineStatus::Finished {
                            run_status: RunStatus::Completed
                        }
                    ),
                    "a message that fits the ceiling must complete: {outcome:?}"
                );
                assert_eq!(artifacts.len(), 1);
                assert_eq!(
                    artifacts[0].content_size(),
                    u64::try_from(artifact::MAX_ARTIFACT_BYTES).unwrap(),
                    "the persisted artifact must be exactly the ceiling"
                );
            } else {
                assert!(
                    matches!(outcome, EngineStatus::Interrupted { .. }),
                    "a message past the ceiling must not complete: {outcome:?}"
                );
                assert!(artifacts.is_empty());
            }
        }

        // The same ceiling on the path that never corroborates anything: a
        // clean exit whose file needs the appended newline to reach one byte
        // past the ceiling is refused by the artifact writer, not written.
        let output = stream(&unterminated);
        let (_temp, _database, run_id, mut store, provider) =
            fixture_with(FixtureBackend::new(&output).writing_final_message(&unterminated));
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        let failure = loop {
            match engine.drive(&mut store, run_id) {
                Ok(EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. }) => {}
                Ok(status) => panic!("a clean exit past the ceiling must not settle: {status:?}"),
                Err(error) => break error,
            }
        };
        assert!(
            failure.to_string().contains("exceeds"),
            "the artifact ceiling must be reported: {failure}"
        );
        assert!(store.list_artifacts(run_id).unwrap().is_empty());
    }

    /// The same death with nothing to corroborate it: `turn.completed` is in
    /// the output but the CLI never left a final message, so the turn is not
    /// trusted. It must still consume the record and report the neutral fact
    /// that the process is gone — an error here re-reads and re-rejects the
    /// same line on every poll, and the stage never leaves Running.
    #[test]
    fn a_dead_process_without_a_final_message_interrupts_and_stays_recoverable() {
        let stage_id = crate::domain::StageId::new("implementation").unwrap();
        let (_temp, _database, run_id, mut store, provider) = fixture_with(
            FixtureBackend::new(SUCCESS_OUTPUT)
                .with_exit(None)
                .writing_final_message_from(Some(2)),
        );
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Interrupted { stages } => {
                    assert_eq!(stages, vec![stage_id.clone()]);
                    break;
                }
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }

        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::Interrupted);
        assert!(store.list_artifacts(run_id).unwrap().is_empty());
        assert!(
            store
                .load_events(run_id)
                .unwrap()
                .iter()
                .any(|event| matches!(
                    event.event.kind(),
                    DomainEventKind::ProviderInterrupted { .. }
                ))
        );
        // The record was consumed: there is nothing left for a later poll to
        // read again and reject again.
        assert_eq!(
            store
                .load_managed_process(session.current_process_id().unwrap())
                .unwrap()
                .cursor(OutputStream::Stdout)
                .offset(),
            u64::try_from(SUCCESS_OUTPUT.len()).unwrap()
        );

        // Ordinary recovery, the same one process loss has always used.
        engine.recover_stage(&mut store, run_id, &stage_id).unwrap();
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Finished {
                    run_status: RunStatus::Completed,
                } => break,
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::Completed);
        assert_eq!(session.invocation(), 2);
        assert_eq!(store.list_artifacts(run_id).unwrap().len(), 1);
    }

    /// A Codex process killed mid-write leaves half a record behind. Execution
    /// cannot act on it and says so; a stop must still be able to stop the
    /// run, or the truncation makes it unstoppable for good.
    #[test]
    fn a_truncated_record_is_reported_by_execution_and_survived_by_a_stop() {
        let truncated = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-A\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"m1\",\"typ"
        );
        let (_temp, _database, run_id, mut store, provider) = fixture(truncated);
        let mut engine = WorkflowEngine::new(provider, "fixture task");

        let failure = loop {
            match engine.drive(&mut store, run_id) {
                Ok(EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. }) => {}
                Ok(status) => panic!("execution must not settle on half a record: {status:?}"),
                Err(error) => break error,
            }
        };
        assert!(
            failure
                .to_string()
                .contains("ended with incomplete JSON record"),
            "execution reports what it cannot parse: {failure}"
        );

        engine
            .drive_observing(&mut store, run_id)
            .expect("observing a process that died mid-record must not fail the stop");
    }

    /// One record can outgrow a whole read window — Codex has emitted an
    /// `item.completed` carrying a full typecheck log past the 1 MiB window
    /// in practice. The read must widen until the record's newline fits, or
    /// the run stalls on a poll that can never see a complete record.
    #[test]
    fn a_record_larger_than_one_read_window_still_completes_the_run() {
        let log = "line of build output\\n".repeat(MAX_OUTPUT_BYTES / 16);
        let output = format!(
            concat!(
                "{{\"type\":\"thread.started\",\"thread_id\":\"codex-thread-1\"}}\n",
                "{{\"type\":\"turn.started\"}}\n",
                "{{\"type\":\"item.completed\",\"item\":{{\"id\":\"c1\",",
                "\"type\":\"command_execution\",\"command\":\"yarn typecheck\",",
                "\"aggregated_output\":\"{}\"}}}}\n",
                "{{\"type\":\"item.completed\",\"item\":{{\"id\":\"m1\",",
                "\"type\":\"agent_message\",\"text\":\"Fixture progress\"}}}}\n",
                "{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":100,",
                "\"cached_input_tokens\":50,\"output_tokens\":20,",
                "\"reasoning_output_tokens\":5}}}}\n"
            ),
            log
        );
        assert!(output.lines().any(|line| line.len() > MAX_OUTPUT_BYTES));
        let (_temp, _database, run_id, mut store, provider) = fixture(&output);
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Finished {
                    run_status: RunStatus::Completed,
                } => break,
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::Completed);
    }

    #[test]
    fn conflicting_thread_identity_fails_without_acknowledging_record() {
        let output = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-A\"}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-B\"}\n"
        );
        let (_temp, _database, run_id, mut store, provider) = fixture(output);
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        for _ in 0..4 {
            let _ = engine.tick(&mut store, run_id).unwrap();
        }
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        let cursor = store
            .load_managed_process(session.current_process_id().unwrap())
            .unwrap()
            .cursor(OutputStream::Stdout);
        let error = engine.tick(&mut store, run_id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected thread-A, received thread-B")
        );
        let session_after = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(
            session_after.native_session_id().unwrap().as_str(),
            "thread-A"
        );
        assert_eq!(
            store
                .load_managed_process(session.current_process_id().unwrap())
                .unwrap()
                .cursor(OutputStream::Stdout),
            cursor
        );
    }

    #[test]
    fn unknown_record_checkpoints_but_invalid_json_keeps_cursor() {
        let unknown = concat!(
            "{\"type\":\"future.codex.event\",\"something\":123}\n",
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-A\"}\n"
        );
        let (_temp, _database, run_id, mut store, provider) = fixture(unknown);
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        for _ in 0..3 {
            let _ = engine.tick(&mut store, run_id).unwrap();
        }
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        let process_id = session.current_process_id().unwrap();
        assert!(matches!(
            engine.tick(&mut store, run_id).unwrap(),
            EngineStatus::Advanced { .. }
        ));
        let checkpoint = store
            .load_managed_process(process_id)
            .unwrap()
            .cursor(OutputStream::Stdout);
        assert_eq!(
            checkpoint.offset(),
            u64::try_from(unknown.lines().next().unwrap().len() + 1).unwrap()
        );
        assert!(!store.load_events(run_id).unwrap().iter().any(|event| {
            matches!(event.event.kind(), DomainEventKind::ProviderStarted { .. })
        }));

        let (_temp, _database, run_id, mut store, provider) = fixture("{\"type\":\"broken\"\n");
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        for _ in 0..3 {
            let _ = engine.tick(&mut store, run_id).unwrap();
        }
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        let process_id = session.current_process_id().unwrap();
        assert!(engine.tick(&mut store, run_id).is_err());
        assert_eq!(
            store
                .load_managed_process(process_id)
                .unwrap()
                .cursor(OutputStream::Stdout)
                .offset(),
            0
        );
        assert_eq!(
            store.list_provider_sessions(run_id).unwrap().pop().unwrap(),
            session
        );
    }

    #[test]
    fn lost_process_recovers_exact_thread_with_new_invocation() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        init_repository(&source);
        let database = temp.path().join("senate.db");
        let process_root = temp.path().join("runs");
        let run_id = crate::domain::RunId::new();
        let created_at = CodexProvider::<RecoveryBackend>::now();
        let config_id = ConfigSnapshotId::new(format!("m8-{run_id}")).unwrap();
        let run = Run::new(run_id, implementation_only(), config_id.clone(), created_at);
        let input = RunInput::new(run_id, "recover task", created_at).unwrap();
        let config = codex_config(config_id, created_at);
        let created = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&database).unwrap();
        store
            .create_run_with_input(&run, &input, &config, &[created])
            .unwrap();
        WorkspaceManager::new(temp.path().join("worktrees"))
            .prepare_run_workspace(&mut store, run_id, &source)
            .unwrap();
        let backend = RecoveryBackend::default();
        let invocations = Arc::clone(&backend.invocations);
        let provider = CodexProvider {
            id: ProviderId::new("codex").unwrap(),
            installation: CodexInstallation::fixture(PathBuf::from("/bin/true")),
            model: None,
            effort: EffortSetting::NativeDefault,
            manager: ProcessManager::new(&process_root, backend),
            artifact_root: process_root,
            // Inside the test's own temp directory: the lookup runs for real
            // and finds nothing, and no test can reach a real Codex home.
            codex_home: Some(temp.path().join("codex-home")),
            image_tool: None,
        };
        let mut engine = WorkflowEngine::new(provider, "recover task");
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Interrupted { stages } => {
                    assert_eq!(
                        stages,
                        vec![crate::domain::StageId::new("implementation").unwrap()]
                    );
                    break;
                }
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }
        engine
            .recover_stage(
                &mut store,
                run_id,
                &crate::domain::StageId::new("implementation").unwrap(),
            )
            .unwrap();
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Finished {
                    run_status: RunStatus::Completed,
                } => break,
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.invocation(), 2);
        assert_eq!(
            session.native_session_id().unwrap().as_str(),
            "recovery-thread"
        );
        assert_eq!(store.list_managed_processes(run_id).unwrap().len(), 2);
        let invocations = invocations.lock().unwrap();
        assert!(!invocations[0].iter().any(|arg| arg == "resume"));
        assert!(
            invocations[1]
                .windows(2)
                .any(|pair| pair == ["resume", "recovery-thread"])
        );
        assert!(!invocations[1].iter().any(|arg| arg == "--last"));
    }

    fn fixture(
        output: &str,
    ) -> (
        TempDir,
        PathBuf,
        crate::domain::RunId,
        SqliteStore,
        CodexProvider<FixtureBackend>,
    ) {
        fixture_with(FixtureBackend::new(output))
    }

    fn fixture_with(
        backend: FixtureBackend,
    ) -> (
        TempDir,
        PathBuf,
        crate::domain::RunId,
        SqliteStore,
        CodexProvider<FixtureBackend>,
    ) {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        init_repository(&source);
        let database = temp.path().join("senate.db");
        let process_root = temp.path().join("runs");
        let run_id = crate::domain::RunId::new();
        let created_at = CodexProvider::<FixtureBackend>::now();
        let config_id = ConfigSnapshotId::new(format!("m8-{run_id}")).unwrap();
        let run = Run::new(run_id, implementation_only(), config_id.clone(), created_at);
        let input = RunInput::new(run_id, "fixture task", created_at).unwrap();
        let config = codex_config(config_id, created_at);
        let created = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&database).unwrap();
        store
            .create_run_with_input(&run, &input, &config, &[created])
            .unwrap();
        WorkspaceManager::new(temp.path().join("worktrees"))
            .prepare_run_workspace(&mut store, run_id, &source)
            .unwrap();
        let provider = CodexProvider {
            id: ProviderId::new("codex").unwrap(),
            installation: CodexInstallation::fixture(PathBuf::from("/bin/true")),
            model: None,
            effort: EffortSetting::NativeDefault,
            manager: ProcessManager::new(&process_root, backend),
            artifact_root: process_root,
            // Inside the test's own temp directory: the lookup runs for real
            // and finds nothing unless the test writes a rollout there, and
            // no test can reach a real Codex home.
            codex_home: Some(temp.path().join("codex-home")),
            image_tool: None,
        };
        (temp, database, run_id, store, provider)
    }

    #[test]
    fn provider_accepts_specialized_reviewer_roles() {
        let (_temp, _database, _run_id, _store, provider) = fixture("");
        assert!(provider.supports_role(Role::CodeQualityReviewer));
        assert!(provider.supports_role(Role::SpecReviewer));
    }

    fn codex_config(
        config_id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> ResolvedConfigSnapshot {
        ResolvedConfigSnapshot::new(
            config_id,
            1,
            json!({
                "schema_version":1,
                "profile":"native_codex",
                "provider":"codex",
                "model":null,
                "provider_options":{
                    "execution_protocol":"exec_json_v1",
                    "sandbox_policy":"stage_kind_v1",
                    "approval_policy":"never"
                }
            }),
            created_at,
        )
        .unwrap()
    }

    fn init_repository(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        command(path, &["init"]);
        command(path, &["config", "user.email", "senate@example.invalid"]);
        command(path, &["config", "user.name", "The Senate Test"]);
        std::fs::write(path.join("README.md"), "fixture\n").unwrap();
        command(path, &["add", "README.md"]);
        command(path, &["commit", "-m", "fixture"]);
    }

    fn command(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
