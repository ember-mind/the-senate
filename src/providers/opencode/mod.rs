//! Native opencode CLI adapter. Drives the user's own local installation
//! and native per-vendor authentication, never a vendor SDK. Prompts use
//! immutable stdin; discovery and every launch audit native permissions.

mod artifact;
mod command;
mod detection;
mod error;
mod prompt;
mod protocol;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::domain::{
    AttentionKind, AttentionRequestId, EffortSetting, ModelId, ProviderId, ProviderSessionId, Role,
    StageKind, StageStatus,
};
use crate::engine::{
    Provider, ProviderAttentionContext, ProviderError, ProviderPoll, ProviderRequest,
    ProviderSignal,
};
use crate::process::{
    ManagedProcessId, ManagedProcessStatus, OutputChunk, OutputStream, ProcessBackend,
    ProcessManager, TmuxBackend,
};
use crate::providers::{
    PendingProviderAttention, ProviderCommit, ProviderSessionMutation, ProviderSessionRecord,
    ProviderSessionRecordId, ProviderSessionStatus, change_handoff,
};
use crate::store::{SqliteStore, process_root};

pub use detection::{OpencodeInstallation, suspicious_opencode_environment};
pub use error::OpencodeProviderError;
use protocol::{OpencodeEvent, OpencodeKind, first_record};

const PROTOCOL_VERSION: u32 = 1;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const MAX_MESSAGE_LINE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DENIAL_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub struct OpencodeProvider<B = TmuxBackend> {
    id: ProviderId,
    installation: OpencodeInstallation,
    model: Option<ModelId>,
    effort: EffortSetting,
    manager: ProcessManager<B>,
    artifact_root: PathBuf,
}

impl OpencodeProvider<TmuxBackend> {
    /// Builds the native adapter using opencode, tmux and The Senate data root.
    ///
    /// # Errors
    /// Returns discovery, permission, model or process-path failures.
    pub fn from_environment(model: Option<ModelId>) -> Result<Self, OpencodeProviderError> {
        let installation = OpencodeInstallation::discover()?;
        installation.require_authenticated()?;
        if let Some(model) = &model {
            installation.validate_model(model)?;
        }
        let root = process_root()?;
        Ok(Self {
            id: ProviderId::new("opencode")
                .map_err(|error| OpencodeProviderError::Protocol(error.to_string()))?,
            installation,
            model,
            effort: EffortSetting::NativeDefault,
            manager: ProcessManager::from_environment()?,
            artifact_root: root,
        })
    }

    pub(crate) fn from_runtime(
        model: Option<ModelId>,
        root: PathBuf,
        runner_executable: PathBuf,
    ) -> Result<Self, OpencodeProviderError> {
        let installation = OpencodeInstallation::discover()?;
        installation.require_authenticated()?;
        if let Some(model) = &model {
            installation.validate_model(model)?;
        }
        Ok(Self {
            id: ProviderId::new("opencode")
                .map_err(|error| OpencodeProviderError::Protocol(error.to_string()))?,
            installation,
            model,
            effort: EffortSetting::NativeDefault,
            manager: ProcessManager::new(&root, TmuxBackend::new(runner_executable)),
            artifact_root: root,
        })
    }
}

impl<B> OpencodeProvider<B> {
    /// Sets requested native effort; native default omits the variant flag.
    #[must_use]
    pub fn with_effort(mut self, effort: EffortSetting) -> Self {
        self.effort = effort;
        self
    }
}

impl<B: ProcessBackend> OpencodeProvider<B> {
    #[must_use]
    pub const fn installation(&self) -> &OpencodeInstallation {
        &self.installation
    }

    fn now() -> DateTime<Utc> {
        std::time::SystemTime::now().into()
    }

    fn continue_instruction(
        &self,
        request: &ProviderRequest,
    ) -> Result<Option<String>, OpencodeProviderError> {
        if !matches!(request.stage_kind(), StageKind::FollowUp | StageKind::Lead) {
            return Ok(None);
        }
        Ok(crate::providers::continue_instruction::read(
            &self.artifact_root,
            request.run_id(),
            request.stage_id(),
        )?)
    }

    fn bash_allow(
        store: &SqliteStore,
        request: &ProviderRequest,
    ) -> Result<BTreeMap<String, String>, OpencodeProviderError> {
        let Some(workspace) = store.load_workspace(request.run_id())? else {
            return Ok(BTreeMap::new());
        };
        command::bash_allow_patterns(
            workspace.worktree_path(),
            Some(workspace.source_repo_path()),
        )
        .map_err(|error| OpencodeProviderError::PermissionsConfig(error.to_string()))
    }

    fn config_path(
        &self,
        request: &ProviderRequest,
        session: &ProviderSessionRecord,
        invocation: u32,
    ) -> PathBuf {
        self.artifact_root
            .join(request.run_id().to_string())
            .join("provider-output")
            .join("opencode")
            .join(session.id().to_string())
            .join(format!("invocation-{invocation}.config.json"))
    }

    fn write_config(path: &Path, config: &Value) -> Result<(), OpencodeProviderError> {
        create_private_parent(path)?;
        std::fs::write(path, serde_json::to_vec_pretty(config)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn read_pending_denial(
        store: &SqliteStore,
        pending: &PendingProviderAttention,
    ) -> Result<String, OpencodeProviderError> {
        let process = store.load_managed_process(pending.process_id())?;
        let mut file = File::open(process.spec().stdout_path())?;
        file.seek(SeekFrom::Start(pending.record_start()))?;
        let length = pending
            .record_end()
            .checked_sub(pending.record_start())
            .ok_or_else(|| {
                OpencodeProviderError::Protocol("attention range regression".to_owned())
            })?;
        let mut bytes = vec![
            0_u8;
            usize::try_from(length).map_err(
                |_| OpencodeProviderError::Protocol("attention record too large".to_owned())
            )?
        ];
        file.read_exact(&mut bytes)?;
        protocol::denied_bash(&bytes).ok_or_else(|| {
            OpencodeProviderError::Protocol(
                "pending attention record is not a denied bash call".to_owned(),
            )
        })
    }

    fn response_path(
        &self,
        session_id: ProviderSessionRecordId,
        attention_id: AttentionRequestId,
    ) -> PathBuf {
        self.artifact_root
            .join("provider-responses")
            .join(session_id.to_string())
            .join(format!("{attention_id}.txt"))
    }

    fn read_response(
        &self,
        session_id: ProviderSessionRecordId,
        attention_id: AttentionRequestId,
    ) -> Result<Option<String>, OpencodeProviderError> {
        match std::fs::read_to_string(self.response_path(session_id, attention_id)) {
            Ok(response) => Ok(Some(response)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Absence means approval; an immutable response carries a decline.
    fn write_response_once(
        &self,
        session_id: ProviderSessionRecordId,
        attention_id: AttentionRequestId,
        response: &str,
    ) -> Result<(), OpencodeProviderError> {
        use std::io::Write as _;

        let bytes = response.as_bytes();
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(OpencodeProviderError::Protocol(format!(
                "attention response exceeds {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        if response.trim().is_empty() {
            return Err(OpencodeProviderError::EmptyAttentionResponse);
        }
        let path = self.response_path(session_id, attention_id);
        let directory = path.parent().ok_or_else(|| {
            OpencodeProviderError::Protocol("response path has no parent".to_owned())
        })?;
        std::fs::create_dir_all(directory)?;
        if path.exists() {
            return if std::fs::read(&path)? == bytes {
                Ok(())
            } else {
                Err(OpencodeProviderError::ArtifactConflict(path))
            };
        }
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&path) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::read(&path)? != bytes {
                    return Err(OpencodeProviderError::ArtifactConflict(path));
                }
            }
            Err(error) => return Err(error.error.into()),
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one function keeps orphan reuse, attention resolution, and command building together"
    )]
    fn start_invocation(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
        mut session: ProviderSessionRecord,
    ) -> Result<ProviderPoll, OpencodeProviderError> {
        // Native configuration can change while a run waits for attention.
        // Audit again before both fresh launches and orphan recovery.
        self.installation.validate_permissions()?;
        let invocation = session
            .invocation()
            .checked_add(1)
            .ok_or_else(|| OpencodeProviderError::Protocol("invocation overflow".to_owned()))?;
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
            if orphan.status() != crate::process::ManagedProcessStatus::Preparing {
                return Err(OpencodeProviderError::Protocol(
                    "unbound provider invocation is not safely restartable".to_owned(),
                ));
            }
            let expected = session.revision();
            session
                .bind_process(orphan.id(), invocation, Self::now())
                .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
            store.update_provider_session(&session, expected)?;
            self.manager.start(store, orphan.id())?;
            return Ok(ProviderPoll::Pending);
        }

        let config_path = self.config_path(request, &session, invocation);
        let mut bash_allow = Self::bash_allow(store, request)?;
        // Read the operator's decision before bind_process clears attention.
        let attention_note = if let Some(pending) = session.pending_attention() {
            let command = Self::read_pending_denial(store, pending)?;
            match self.read_response(session.id(), pending.attention_id())? {
                None => {
                    let patterns = command::exact_bash_patterns(&command)
                        .map_err(OpencodeProviderError::UnsafePermission)?;
                    for pattern in patterns {
                        bash_allow.insert(pattern, "allow".to_owned());
                    }
                    Some(format!(
                        "The operator approved the command you were denied permission to run: `{command}`. It is now allowed; retry it, then continue the task."
                    ))
                }
                Some(text) => Some(format!(
                    "The operator declined the command you were denied permission to run: `{command}`. {text}"
                )),
            }
        } else {
            None
        };
        let config = command::OpencodeSandbox::for_stage(request.stage_kind())
            .permission_config(&bash_allow);
        Self::write_config(&config_path, &config)?;
        let permission = &config["permission"];

        let command = if let Some(native) = session.native_session_id() {
            let mut prompt = prompt::continuation(request);
            if let Some(note) = &attention_note {
                prompt.push_str("\n\n");
                prompt.push_str(note);
            }
            command::resume(
                native,
                &prompt,
                request.stage_kind(),
                self.model.as_ref(),
                self.effort,
                request.workspace_path(),
                &config_path,
                permission,
            )
        } else {
            let artifacts = store.list_artifacts(request.run_id())?;
            let handoff = change_handoff::for_request(store, request)?;
            let continue_instruction = self.continue_instruction(request)?;
            let prompt = prompt::compose(
                request,
                &artifacts,
                handoff.as_ref(),
                continue_instruction.as_deref(),
            )?;
            command::initial(
                &prompt,
                request.stage_kind(),
                self.model.as_ref(),
                self.effort,
                request.workspace_path(),
                &config_path,
                permission,
            )
        };
        let process = self.manager.prepare_with_input(
            store,
            request.run_id(),
            request.stage_id().clone(),
            request.attempt(),
            invocation,
            self.installation.executable(),
            command.argv,
            command.environment,
            &command.stdin,
        )?;
        let expected = session.revision();
        session
            .bind_process(process.id(), invocation, Self::now())
            .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
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
    ) -> Result<ProviderPoll, OpencodeProviderError> {
        if !request.observe_only() {
            if session.status() == ProviderSessionStatus::Created {
                return self.start_invocation(store, request, session);
            }
            if matches!(
                session.status(),
                ProviderSessionStatus::NeedsUser | ProviderSessionStatus::Interrupted
            ) && request.stage_status() == StageStatus::Running
            {
                return self.start_invocation(store, request, session);
            }
            if session.status() == ProviderSessionStatus::Interrupted
                && request.stage_status() == StageStatus::Ready
            {
                return self.start_invocation(store, request, session);
            }
        }
        let Some(process_id) = session.current_process_id() else {
            if request.observe_only() {
                return Ok(ProviderPoll::Pending);
            }
            return Err(OpencodeProviderError::Protocol(
                "provider session has no current process".to_owned(),
            ));
        };
        let inspection = self.manager.inspect(store, process_id)?;
        if matches!(
            inspection.process.status(),
            ManagedProcessStatus::Preparing | ManagedProcessStatus::Starting
        ) && !request.observe_only()
        {
            self.installation.validate_permissions()?;
            self.manager.start(store, process_id)?;
        }
        let inspection = self.manager.inspect(store, process_id)?;
        let successful_exit = inspection.exit_evidence.as_ref().is_some_and(|evidence| {
            matches!(
                evidence.result(),
                crate::process::ExitResult::ExitCode { code: 0 }
            )
        });
        let chunk = self.read_record_chunk(store, process_id)?;
        if let Some((event, consumed)) = first_record(chunk.bytes())? {
            let consumed = u64::try_from(consumed)
                .map_err(|_| OpencodeProviderError::Protocol("record size overflow".to_owned()))?;
            let end = chunk.start_offset().checked_add(consumed).ok_or_else(|| {
                OpencodeProviderError::Protocol("output offset overflow".to_owned())
            })?;
            return self.map_record(
                store,
                request,
                session,
                chunk,
                end,
                event,
                inspection.process.status(),
                successful_exit,
            );
        }
        if inspection.process.status().is_active() {
            return Ok(ProviderPoll::Pending);
        }
        if !chunk.bytes().is_empty() && !request.observe_only() {
            return Err(OpencodeProviderError::Protocol(
                "opencode process ended with incomplete JSON record".to_owned(),
            ));
        }
        Self::map_terminal_without_result(
            store,
            request,
            session,
            chunk,
            inspection.process.status(),
            successful_exit,
        )
    }

    /// Widen saturated reads until a whole JSONL record can be decoded.
    fn read_record_chunk(
        &self,
        store: &SqliteStore,
        process_id: ManagedProcessId,
    ) -> Result<OutputChunk, OpencodeProviderError> {
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
        reason = "one match keeps every native record's session and signal effects together"
    )]
    fn map_record(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
        mut session: ProviderSessionRecord,
        chunk: OutputChunk,
        end: u64,
        event: OpencodeEvent,
        process_status: ManagedProcessStatus,
        successful_exit: bool,
    ) -> Result<ProviderPoll, OpencodeProviderError> {
        let expected = session.revision();
        let mut commit = ProviderCommit::new(chunk, end);
        let mut signals = Vec::new();
        let mut session_changed = false;

        // Every event carries identity; the first record binds this invocation.
        if session.status() == ProviderSessionStatus::Starting {
            let native = ProviderSessionId::new(event.session_id.clone())
                .map_err(|error| OpencodeProviderError::Protocol(error.to_string()))?;
            session
                .activate(native.clone(), None, Self::now())
                .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
            session_changed = true;
            signals.push(if request.signal_index() == 0 {
                ProviderSignal::Started {
                    model_id: None,
                    session_id: Some(native),
                }
            } else {
                ProviderSignal::Resumed
            });
        } else if let Some(existing) = session.native_session_id()
            && existing.as_str() != event.session_id
        {
            return Err(OpencodeProviderError::SessionMismatch {
                expected: existing.to_string(),
                actual: event.session_id,
            });
        }

        match event.kind {
            OpencodeKind::StepStart | OpencodeKind::Ignored => {}
            OpencodeKind::ToolUse { progress } => signals.push(ProviderSignal::Progress(progress)),
            OpencodeKind::Text { text, .. } => {
                if !text.trim().is_empty() {
                    signals.push(ProviderSignal::Progress(text));
                }
            }
            OpencodeKind::StepFinish {
                message_id,
                stop,
                usage,
            } => {
                if !stop {
                    signals.push(ProviderSignal::Usage(usage));
                } else if process_status.is_active() {
                    // Nothing has been committed; re-derive after process exit.
                    return Ok(ProviderPoll::Pending);
                } else {
                    // No independently written final-message file exists for
                    // corroboration. Only a clean exit can prove completion.
                    let trusted =
                        matches!(process_status, ManagedProcessStatus::Exited) && successful_exit;
                    if trusted {
                        let process = store.load_managed_process(commit.output().process_id())?;
                        let text = final_answer(process.spec().stdout_path(), &message_id, end)
                            .ok_or_else(|| {
                                OpencodeProviderError::Protocol(
                                    "opencode reported a final step but no matching text part was retained".to_owned(),
                                )
                            })?;
                        let workspace =
                            store.load_workspace(request.run_id())?.ok_or_else(|| {
                                OpencodeProviderError::Protocol(
                                    "run workspace disappeared".to_owned(),
                                )
                            })?;
                        let artifact = artifact::persist(
                            &self.artifact_root,
                            &text,
                            request,
                            &self.id,
                            session.model_id(),
                            workspace.base_commit(),
                            Self::now(),
                        )?;
                        session
                            .complete(Self::now())
                            .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
                        session_changed = true;
                        commit = commit.with_artifact(artifact);
                        signals.push(ProviderSignal::Usage(usage));
                        signals.push(ProviderSignal::Completed);
                    } else {
                        session
                            .interrupt(Self::now())
                            .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
                        session_changed = true;
                        signals.push(ProviderSignal::Progress(format!(
                            "opencode reported a final step but the process ended as {process_status:?} without a clean exit; opencode has no corroborating final-message file the way Codex does, so this is treated as a recoverable interruption rather than a trusted completion"
                        )));
                        signals.push(ProviderSignal::Interrupted);
                    }
                }
            }
            OpencodeKind::Error { message } => {
                session
                    .fail(Self::now())
                    .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
                session_changed = true;
                signals.push(ProviderSignal::Failed(message));
            }
        }

        if session_changed {
            commit = commit.with_session(ProviderSessionMutation::new(session, expected));
        }
        if signals.is_empty() {
            Ok(ProviderPoll::Checkpoint(commit))
        } else {
            Ok(ProviderPoll::Emission { signals, commit })
        }
    }

    fn map_terminal_without_result(
        store: &mut SqliteStore,
        request: &ProviderRequest,
        mut session: ProviderSessionRecord,
        chunk: OutputChunk,
        status: ManagedProcessStatus,
        successful_exit: bool,
    ) -> Result<ProviderPoll, OpencodeProviderError> {
        if session.native_session_id().is_none() && !request.observe_only() {
            if matches!(
                status,
                ManagedProcessStatus::Interrupted | ManagedProcessStatus::Missing
            ) {
                let expected = session.revision();
                session
                    .interrupt(Self::now())
                    .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
                store.update_provider_session(&session, expected)?;
            }
            return Err(OpencodeProviderError::MissingSessionId(format!(
                "process ended as {status:?} before opencode emitted any event for {}",
                request.stage_id()
            )));
        }
        let expected = session.revision();
        let end = chunk.end_offset();
        // Headless ask rejection ends the invocation with exit 0 and no stop.
        // Recover the exact denied command from retained evidence, not stderr.
        if status == ManagedProcessStatus::Exited && successful_exit {
            if let Some(process_id) = session.current_process_id() {
                let process = store.load_managed_process(process_id)?;
                if let Some((command, start, denial_end)) =
                    last_denied_bash(process.spec().stdout_path())
                {
                    let attention_id = AttentionRequestId::new();
                    let pending =
                        PendingProviderAttention::new(attention_id, process_id, start, denial_end)
                            .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
                    session
                        .need_user(pending, Self::now())
                        .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
                    return Ok(ProviderPoll::Emission {
                        signals: vec![ProviderSignal::NeedsUser {
                            kind: AttentionKind::Permission,
                            summary: format!(
                                "opencode was denied permission to run `{command}` and stopped the turn; approve to allow it and retry, or decline to continue without it"
                            ),
                            request_id: Some(attention_id),
                        }],
                        commit: ProviderCommit::new(chunk, end)
                            .with_session(ProviderSessionMutation::new(session, expected)),
                    });
                }
            }
            session
                .fail(Self::now())
                .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
            return Ok(ProviderPoll::Emission {
                signals: vec![ProviderSignal::Failed(format!(
                    "opencode exited cleanly without reaching a final step and without any observed permission denial for {}",
                    request.stage_id()
                ))],
                commit: ProviderCommit::new(chunk, end)
                    .with_session(ProviderSessionMutation::new(session, expected)),
            });
        }
        let signal = if matches!(
            status,
            ManagedProcessStatus::Interrupted | ManagedProcessStatus::Missing
        ) {
            session
                .interrupt(Self::now())
                .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
            ProviderSignal::Interrupted
        } else {
            session
                .fail(Self::now())
                .map_err(|error| OpencodeProviderError::Protocol(error.to_owned()))?;
            ProviderSignal::Failed(format!(
                "opencode process ended as {status:?} without completing the stage for {}",
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

impl<B: ProcessBackend> Provider for OpencodeProvider<B> {
    fn provider_id_for(&self, _request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
        Ok(self.id.clone())
    }

    fn supports_role(&self, _role: Role) -> bool {
        true
    }

    fn keep_attached_for(&self, _request: &ProviderRequest) -> Result<bool, ProviderError> {
        Ok(true)
    }

    /// Validate an exact approval before the domain commits the resolution.
    fn stage_attention_response(
        &mut self,
        store: &mut SqliteStore,
        context: &ProviderAttentionContext,
        response: Option<&str>,
    ) -> Result<(), ProviderError> {
        let result = (|| -> Result<(), OpencodeProviderError> {
            let session = store
                .list_provider_sessions(context.run_id())?
                .into_iter()
                .find(|session| {
                    session.stage_id() == context.stage_id()
                        && session.provider_id() == &self.id
                        && session
                            .pending_attention()
                            .is_some_and(|pending| pending.attention_id() == context.request_id())
                })
                .ok_or_else(|| {
                    OpencodeProviderError::Protocol(
                        "attention has no matching opencode provider session".to_owned(),
                    )
                })?;
            let pending = session
                .pending_attention()
                .expect("matched pending attention");
            let command = Self::read_pending_denial(store, pending)?;
            match response {
                None => {
                    command::exact_bash_patterns(&command)
                        .map_err(OpencodeProviderError::UnsafePermission)?;
                }
                Some(text) if !text.trim().is_empty() => {
                    self.write_response_once(session.id(), context.request_id(), text)?;
                }
                Some(_) => return Err(OpencodeProviderError::EmptyAttentionResponse),
            }
            Ok(())
        })();
        result.map_err(|error| ProviderError::new(error.to_string()))
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
        let result = (|| -> Result<ProviderPoll, OpencodeProviderError> {
            match store.load_provider_session_for_attempt(
                request.run_id(),
                request.stage_id(),
                request.attempt(),
            )? {
                Some(session) if session.provider_id() != &self.id => {
                    Err(OpencodeProviderError::Protocol(
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

/// Concatenate the winning message's retained text parts in stream order.
fn final_answer(path: &Path, message_id: &str, end: u64) -> Option<String> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let mut line = Vec::new();
    let mut position = 0_u64;
    let mut text = String::new();
    while position < end {
        line.clear();
        let read = read_capped_line(&mut reader, &mut line).ok()?;
        if read == 0 {
            break;
        }
        position = position.saturating_add(read);
        if line.last() == Some(&b'\n')
            && let Some((id, part_text)) = protocol::text_part(&line)
            && id == message_id
        {
            text.push_str(&part_text);
        }
    }
    (!text.is_empty()).then_some(text)
}

/// Retain only a bounded line, while counting all discarded bytes as well.
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

/// Recover the last denied bash command and its exact retained byte range.
fn last_denied_bash(path: &Path) -> Option<(String, u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_DENIAL_SCAN_BYTES {
        return None;
    }
    let mut reader = BufReader::new(File::open(path).ok()?);
    let mut line = Vec::new();
    let mut position = 0_u64;
    let mut last = None;
    loop {
        line.clear();
        let start = position;
        let read = read_capped_line(&mut reader, &mut line).ok()?;
        if read == 0 {
            break;
        }
        position = position.saturating_add(read);
        if line.last() == Some(&b'\n')
            && let Some(command) = protocol::denied_bash(&line)
        {
            last = Some((command, start, position));
        }
    }
    last
}

fn create_private_parent(path: &Path) -> Result<(), OpencodeProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| OpencodeProviderError::Protocol("config path has no parent".to_owned()))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
