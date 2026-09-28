//! Native opencode CLI adapter. Drives the user's own local `opencode`
//! installation and its native per-vendor authentication
//! (`~/.local/share/opencode/auth.json`); no vendor API key is ever read,
//! copied, or passed through this adapter. Any `opencode models` entry can
//! serve a stage — `opencode-go/deepseek-v4-pro`, `google/gemini-...`,
//! `opencode-go/kimi-k3`, and so on — because the model id itself carries the
//! vendor; The Senate never special-cases one.

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
/// Ceiling for one record. A single line larger than this fails the poll
/// rather than growing the read without bound.
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
/// Ceiling for one line held in memory while reconstructing the final
/// answer. Generous against JSON escaping.
const MAX_MESSAGE_LINE_BYTES: u64 = 8 * 1024 * 1024;
/// Ceiling on the retained stdout scanned for a denied `bash` call when a
/// clean exit never reached a terminal `stop` step. Mirrors Claude's own
/// denial-recovery scan: an oversized log yields no evidence rather than
/// being pulled fully into memory, which asks the operator instead of
/// guessing.
const MAX_DENIAL_SCAN_BYTES: u64 = 8 * 1024 * 1024;
/// Ceiling on one persisted operator response to a permission attention.
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
    /// Builds native adapter using discovered opencode CLI, tmux, and The
    /// Senate data root.
    ///
    /// # Errors
    /// Returns missing/auth/unknown-model/process-path failures before
    /// execution starts.
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
    /// Sets the requested effort translated onto the native `--variant`
    /// flag. `NativeDefault` keeps invocations byte-identical to pre-effort
    /// policy.
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

    /// A follow-up stage's operator instruction, persisted by
    /// [`crate::app::RunService::request_continue`] before this stage's
    /// initial invocation ever runs. `None` for every other stage kind.
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

    /// The repository's standing bash allowlist translated onto opencode's
    /// bash-permission patterns, read from the run's own worktree and then
    /// from the repository it was cut from. A workspace not ready yet grants
    /// nothing rather than failing: `prepare_with_input` is the check that a
    /// stage cannot run without a worktree, and it runs a few lines later
    /// with a better error than this one would give.
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

    /// Writes this invocation's permission config, the shape stage kind alone
    /// decides (see [`command::OpencodeSandbox`]), to a run-private path this
    /// invocation's `OPENCODE_CONFIG` points at for the record. `OPENCODE_CONFIG`
    /// is not the enforcement authority — a repository-controlled
    /// `opencode.json` can override it — so the same `config["permission"]`
    /// value is also carried as `OPENCODE_PERMISSION`, built by the caller.
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

    /// The exact denied `bash` command a pending permission attention points
    /// at, re-read from the retained stdout range [`PendingProviderAttention`]
    /// bounds — the same seek-and-parse-one-record pattern Claude's own
    /// denial recovery uses.
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

    /// Persists the operator's decline instruction once. Absence of this file
    /// at resume time is itself meaningful: it is how the adapter tells an
    /// approval (no response ever staged) from a decline (one was).
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
        // A pending permission attention resolves into this invocation's own
        // command and config: an approval widens the allowlist by exactly the
        // denied command's own top-level parts (opencode's own permission
        // granularity — a compound command is checked part by part) and asks
        // the agent to retry it; a decline grants nothing and carries the
        // operator's instruction instead. Read before `bind_process` below,
        // which clears it.
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
            &[],
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
            // A permission halt (`NeedsUser`) and an interruption both resume
            // by launching a fresh invocation once the stage is running
            // again; a stage that never started keeps `Ready` while the
            // session sits interrupted over a dead process, and that needs
            // the same fresh launch.
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

    /// Reads unacknowledged stdout, widening the window whenever it fills
    /// without containing a newline. Same reasoning as the Codex adapter's
    /// equivalent: a record only completes at a newline, so a saturated
    /// window without one can never yield a record no matter how often the
    /// same-sized read is retried.
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

        // opencode carries the session id on every event, unlike Codex's
        // dedicated `thread.started`; whichever record happens to be first
        // for this invocation binds (or re-confirms, on resume) native
        // session identity uniformly, regardless of its own kind.
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
                    // Nothing below has been persisted yet, so returning here
                    // discards nothing durable; the next poll re-derives the
                    // identical decision once the process has actually ended.
                    return Ok(ProviderPoll::Pending);
                } else {
                    // opencode writes no separate corroborating file the way
                    // Codex's `--output-last-message` does, so — unlike
                    // Codex, which can trust an unclean exit when a second
                    // independent write agrees with the retained stream —
                    // opencode trusts a terminal `stop` step only on a clean
                    // exit. Anything else is reported as the same recoverable
                    // interruption Codex falls back to for an uncorroborated
                    // dead-process completion: the finished work stays
                    // reachable through `senate resume`, not stranded behind
                    // a hard failure that `retry` would throw away.
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
        // opencode ends the whole invocation — a clean `exit 0` — the moment a
        // headless `ask` permission is auto-rejected, never reaching a
        // terminal `stop` step (observed by hand, both in the spike fixtures
        // and in a real end-to-end run: `impl.jsonl`/`impl.stderr.txt` and a
        // process whose retained stream ends on `step_finish` reason
        // `tool-calls`). That is a permission halt, not a crash: recover the
        // exact denied command from the retained stream and raise typed
        // attention, the same continuation shape Claude's own permission
        // denials use. A clean exit with no `stop` step and no denial
        // evidence at all is a distinct, unexplained ending and gets its own
        // specific failure rather than the generic one below.
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

    /// Stages the operator's decision on one permission halt before the
    /// domain commits the resolution: `response: None` is "omit to approve"
    /// (see `senate resolve`), anything else is a decline carrying that text
    /// as the continuation instruction. Approval is validated as an exact,
    /// non-widening grant *before* anything commits — the same
    /// prove-it-first discipline Claude's own permission continuation uses —
    /// so an ungrantable command (glob syntax in the denied line itself) is
    /// refused to the operator instead of committing a resolution every
    /// later drive would fail to build a command for.
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

/// The verbatim text of every `text` part under `message_id` retained in
/// `path` before `end`, concatenated in stream order — the protocol's own
/// definition of the final answer: "the last text part(s) of the last step
/// with reason `stop`". `end` is where the deciding `step_finish` record
/// ends, so this only ever sees parts that preceded it. `None` when nothing
/// matched, which the caller treats as absence of evidence rather than an
/// empty answer.
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

/// Reads up to [`MAX_MESSAGE_LINE_BYTES`] of one line, discarding the rest of
/// a longer one, and returns how many bytes of the stream it covered.
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

/// The last denied `bash` call retained in `path`, with its own exact byte
/// range, or `None` when no denial is evidenced there.
///
/// Read-only and bounded: an oversized log yields no evidence rather than
/// being pulled fully into memory, the same "unreadable/oversized log asks
/// the operator" rule Claude's own denial-recovery scan already uses. The
/// range is the *last* denial specifically, mirroring what actually happens:
/// once opencode auto-rejects one `ask` request it stops the whole
/// invocation, so at most one such record exists per invocation in practice,
/// and taking the last one is simply the most defensive reading of that.
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
mod tests {
    use std::collections::HashSet;
    use std::io::Seek as _;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, DomainEventKind, EventId, EventMetadata, Run, RunStatus, StageDefinition,
        StageId, WorkflowDefinition, WorkflowKind,
    };
    use crate::engine::{EngineStatus, WorkflowEngine};
    use crate::process::{
        BackendAvailability, BackendSessionId, BackendSessionState, ExitEvidence, ExitResult,
        ManagedProcess, ProcessError, TerminationSignal,
    };
    use crate::store::{ResolvedConfigSnapshot, RunInput};
    use crate::workspace::WorkspaceManager;

    fn implementation_only() -> WorkflowDefinition {
        WorkflowDefinition::new(
            WorkflowKind::Fast,
            vec![StageDefinition::new(
                StageId::new("implementation").unwrap(),
                StageKind::Implementation,
                Role::Implementer,
                vec![],
            )],
        )
        .unwrap()
    }

    /// Shape copied from the real `ro.jsonl` fixture (a single-turn
    /// read-only run): `step_start`, a completed `read` tool, a `text` part
    /// under the winning `step_finish`'s messageID, then `step_finish` with
    /// `reason: "stop"`.
    const SUCCESS_OUTPUT: &str = concat!(
        "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\"}}\n",
        "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"read\",\"state\":{\"status\":\"completed\"}}}\n",
        "{\"type\":\"text\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"text\":\"# opencode result\\nFixture\"}}\n",
        "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"reason\":\"stop\",\"tokens\":{\"total\":167,\"input\":100,\"output\":47,\"reasoning\":0,\"cache\":{\"write\":0,\"read\":20}}}}\n"
    );

    /// Shape copied from a real end-to-end run (`stdout.log`/`stderr.log`,
    /// path-sanitized): opencode auto-rejects a `bash` call the headless
    /// session cannot interactively ask about and ends the *whole
    /// invocation* right there — a clean `exit 0` whose last record is
    /// `step_finish` reason `"tool-calls"`, never `"stop"`. The spike's own
    /// `impl.jsonl`/`impl.stderr.txt` and `resume.jsonl` show the same shape:
    /// the denial ends one invocation, and a later `-s <session>` resume is
    /// what continues it to `"stop"`.
    const PERMISSION_HALT_OUTPUT: &str = concat!(
        "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\"}}\n",
        "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_1\",\"state\":{\"status\":\"error\",\"input\":{\"command\":\"python3 -c \\\"from calc import add; assert add(2,3)==5\\\"\"},\"error\":\"The user rejected permission to use this specific tool call.\"}}}\n",
        "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"reason\":\"tool-calls\",\"tokens\":{\"total\":50,\"input\":40,\"output\":10}}}\n"
    );

    /// What a `-s ses_A` resume looks like once the operator approves the
    /// denied command: the agent retries it and this time reaches `"stop"`.
    const RESUME_AFTER_APPROVAL_OUTPUT: &str = concat!(
        "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\"}}\n",
        "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_2\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"python3 -c \\\"from calc import add; assert add(2,3)==5\\\"\"},\"output\":\"\"}}}\n",
        "{\"type\":\"text\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"text\":\"# opencode result\\nVerified.\"}}\n",
        "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"reason\":\"stop\",\"tokens\":{\"total\":30,\"input\":20,\"output\":10}}}\n"
    );

    /// Shape copied from a real end-to-end run: opencode splits a compound
    /// command into top-level parts and checks each one, so a denial names
    /// every part (`bash (pwd, ls -la); auto-rejecting`, real stderr) even
    /// though only `pwd` was actually ungranted.
    const COMPOUND_PERMISSION_HALT_OUTPUT: &str = concat!(
        "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\"}}\n",
        "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_1\",\"state\":{\"status\":\"error\",\"input\":{\"command\":\"pwd && ls -la\"},\"error\":\"The user rejected permission to use this specific tool call.\"}}}\n",
        "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_1\",\"reason\":\"tool-calls\",\"tokens\":{\"total\":50,\"input\":40,\"output\":10}}}\n"
    );

    const COMPOUND_RESUME_AFTER_APPROVAL_OUTPUT: &str = concat!(
        "{\"type\":\"step_start\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\"}}\n",
        "{\"type\":\"tool_use\",\"sessionID\":\"ses_A\",\"part\":{\"tool\":\"bash\",\"callID\":\"call_2\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"pwd && ls -la\"},\"output\":\"\"}}}\n",
        "{\"type\":\"text\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"text\":\"# opencode result\\nDone.\"}}\n",
        "{\"type\":\"step_finish\",\"sessionID\":\"ses_A\",\"part\":{\"messageID\":\"msg_2\",\"reason\":\"stop\",\"tokens\":{\"total\":30,\"input\":20,\"output\":10}}}\n"
    );

    /// Shape copied from the real `err-402.jsonl` fixture.
    const INSUFFICIENT_BALANCE_OUTPUT: &str = "{\"type\":\"error\",\"sessionID\":\"ses_A\",\"error\":{\"name\":\"APIError\",\"data\":{\"message\":\"Insufficient Balance\",\"statusCode\":402}}}\n";

    #[derive(Clone)]
    struct FixtureBackend {
        started: Arc<Mutex<HashSet<crate::process::ManagedProcessId>>>,
        completed: Arc<Mutex<HashSet<crate::process::ManagedProcessId>>>,
        output: Arc<String>,
        exit: Option<ExitResult>,
    }

    impl FixtureBackend {
        fn new(output: &str) -> Self {
            Self {
                started: Arc::new(Mutex::new(HashSet::new())),
                completed: Arc::new(Mutex::new(HashSet::new())),
                output: Arc::new(output.to_owned()),
                exit: Some(ExitResult::ExitCode { code: 0 }),
            }
        }

        fn with_exit(mut self, exit: Option<ExitResult>) -> Self {
            self.exit = exit;
            self
        }
    }

    impl ProcessBackend for FixtureBackend {
        fn kind(&self) -> &'static str {
            "fixture"
        }

        fn session_id(&self, process_id: crate::process::ManagedProcessId) -> BackendSessionId {
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
            file.seek(std::io::SeekFrom::Start(offset))?;
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
            let now = OpencodeProvider::<Self>::now();
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

    /// One recorded invocation's number and exact argv.
    type RecordedInvocation = (u32, Vec<String>);

    /// Invocation 1 replays a real permission halt; invocation 2+ replays
    /// what a `-s ses_A` resume looks like once approved. Records every
    /// invocation's exact argv, so a test can check the resume actually
    /// carries `--session ses_A`.
    #[derive(Clone)]
    struct HaltThenResumeBackend {
        halt_output: &'static str,
        resume_output: &'static str,
        started: Arc<Mutex<HashSet<crate::process::ManagedProcessId>>>,
        completed: Arc<Mutex<HashSet<crate::process::ManagedProcessId>>>,
        invocations: Arc<Mutex<Vec<RecordedInvocation>>>,
    }

    impl Default for HaltThenResumeBackend {
        fn default() -> Self {
            Self::new(PERMISSION_HALT_OUTPUT, RESUME_AFTER_APPROVAL_OUTPUT)
        }
    }

    impl HaltThenResumeBackend {
        fn new(halt_output: &'static str, resume_output: &'static str) -> Self {
            Self {
                halt_output,
                resume_output,
                started: Arc::new(Mutex::new(HashSet::new())),
                completed: Arc::new(Mutex::new(HashSet::new())),
                invocations: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl ProcessBackend for HaltThenResumeBackend {
        fn kind(&self) -> &'static str {
            "halt_then_resume"
        }

        fn session_id(&self, process_id: crate::process::ManagedProcessId) -> BackendSessionId {
            BackendSessionId::for_process(process_id)
        }

        fn availability(&self) -> Result<BackendAvailability, ProcessError> {
            Ok(BackendAvailability {
                kind: self.kind(),
                version: "fixture-1".to_owned(),
            })
        }

        fn start(&self, process: &ManagedProcess, _manifest: &Path) -> Result<(), ProcessError> {
            let argv = process
                .spec()
                .argv()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            self.invocations
                .lock()
                .unwrap()
                .push((process.invocation(), argv));
            let output = if process.invocation() == 1 {
                self.halt_output
            } else {
                self.resume_output
            };
            std::fs::write(process.spec().stdout_path(), output)?;
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
            file.seek(std::io::SeekFrom::Start(offset))?;
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
            if !self.completed.lock().unwrap().contains(&process.id()) {
                return Ok(None);
            }
            let now = OpencodeProvider::<Self>::now();
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

    fn fixture(
        output: &str,
    ) -> (
        TempDir,
        PathBuf,
        crate::domain::RunId,
        SqliteStore,
        OpencodeProvider<FixtureBackend>,
    ) {
        fixture_with(FixtureBackend::new(output))
    }

    fn fixture_with<B: ProcessBackend>(
        backend: B,
    ) -> (
        TempDir,
        PathBuf,
        crate::domain::RunId,
        SqliteStore,
        OpencodeProvider<B>,
    ) {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        init_repository(&source);
        let database = temp.path().join("senate.db");
        let process_root = temp.path().join("runs");
        let run_id = crate::domain::RunId::new();
        let created_at = OpencodeProvider::<B>::now();
        let config_id = ConfigSnapshotId::new(format!("opencode-{run_id}")).unwrap();
        let run = Run::new(run_id, implementation_only(), config_id.clone(), created_at);
        let input = RunInput::new(run_id, "fixture task", created_at).unwrap();
        let config = opencode_config(config_id, created_at);
        let created = run.created_event(EventMetadata::new(EventId::new(), created_at));
        let mut store = SqliteStore::open(&database).unwrap();
        store
            .create_run_with_input(&run, &input, &config, &[created])
            .unwrap();
        WorkspaceManager::new(temp.path().join("worktrees"))
            .prepare_run_workspace(&mut store, run_id, &source)
            .unwrap();
        let provider = OpencodeProvider {
            id: ProviderId::new("opencode").unwrap(),
            installation: OpencodeInstallation::fixture(PathBuf::from("/bin/true")),
            model: None,
            effort: EffortSetting::NativeDefault,
            manager: ProcessManager::new(&process_root, backend),
            artifact_root: process_root,
        };
        (temp, database, run_id, store, provider)
    }

    fn opencode_config(
        config_id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> ResolvedConfigSnapshot {
        ResolvedConfigSnapshot::new(
            config_id,
            1,
            json!({
                "schema_version":1,
                "profile":"native_opencode",
                "provider":"opencode",
                "model":null,
                "provider_options":{
                    "execution_protocol":"run_json_v1",
                    "permission_policy":"stage_kind_v1"
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

    fn drive_to_completion<B: ProcessBackend>(
        engine: &mut WorkflowEngine<OpencodeProvider<B>>,
        store: &mut SqliteStore,
        run_id: crate::domain::RunId,
    ) -> RunStatus {
        loop {
            match engine.drive(store, run_id).unwrap() {
                EngineStatus::Finished { run_status } => return run_status,
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        }
    }

    #[test]
    fn successful_turn_persists_artifact_usage_and_completes() {
        let (_temp, database, run_id, mut store, provider) = fixture(SUCCESS_OUTPUT);
        let mut engine = WorkflowEngine::new(provider, "SUPER_SECRET_TASK_MARKER");
        assert_eq!(
            drive_to_completion(&mut engine, &mut store, run_id),
            RunStatus::Completed
        );

        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::Completed);
        assert_eq!(session.native_session_id().unwrap().as_str(), "ses_A");

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
            argv.iter()
                .any(|arg| arg.contains("SUPER_SECRET_TASK_MARKER")),
            "the prompt travels as argv, the only transport opencode's CLI offers"
        );
        assert!(
            process
                .spec()
                .environment()
                .contains_key(&std::ffi::OsString::from("OPENCODE_CONFIG"))
        );
        // A repository-controlled opencode.json overriding OPENCODE_CONFIG is
        // a verified real vulnerability; every real managed process spec must
        // carry both independently-verified defenses, and --pure, end to end.
        assert_eq!(
            process
                .spec()
                .environment()
                .get(&std::ffi::OsString::from("OPENCODE_DISABLE_PROJECT_CONFIG")),
            Some(&std::ffi::OsString::from("1"))
        );
        assert!(
            process
                .spec()
                .environment()
                .contains_key(&std::ffi::OsString::from("OPENCODE_PERMISSION")),
        );
        assert!(argv.iter().any(|arg| arg == "--pure"), "{argv:?}");

        let events = store.load_events(run_id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.event.kind(),
                    DomainEventKind::ProviderUsageUpdated {
                        input_units: 100,
                        output_units: 47,
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
            "opencode"
        );
        let content = std::fs::read_to_string(artifacts[0].path()).unwrap();
        assert!(content.contains("# opencode result"));

        drop(store);
        let mut store = SqliteStore::open(database).unwrap();
        assert_eq!(
            store.load_run(run_id).unwrap().run.status(),
            RunStatus::Completed
        );
    }

    /// Headless opencode auto-rejects a permission it cannot ask about and
    /// ends the whole invocation right there (verified against a real
    /// end-to-end run) — a permission halt, not silent progress. It raises
    /// the same typed continuable attention Claude's own permission denials
    /// do; approving it resumes the same native session with the exact
    /// command now allowed, and the run completes.
    #[test]
    fn a_permission_halt_raises_attention_and_approval_resumes_and_completes() {
        let backend = HaltThenResumeBackend::default();
        let inspector = backend.clone();
        let (temp, _database, run_id, mut store, provider) = fixture_with(backend);
        let mut engine = WorkflowEngine::new(provider, "fix the bug");
        let request_id = loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::NeedsUser { requests } => break requests[0],
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        };

        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::NeedsUser);
        assert_eq!(session.native_session_id().unwrap().as_str(), "ses_A");

        let loaded = store.load_run(run_id).unwrap();
        let attention = loaded
            .run
            .attention_requests()
            .iter()
            .find(|request| request.id() == request_id)
            .unwrap();
        assert_eq!(attention.kind(), crate::domain::AttentionKind::Permission);
        assert!(
            attention.summary().contains("python3 -c"),
            "{}",
            attention.summary()
        );

        // Approve: omit a response, exactly like `senate resolve <run> <id>`.
        engine
            .resolve_attention_with_response(&mut store, run_id, request_id, None)
            .unwrap();
        assert_eq!(
            drive_to_completion(&mut engine, &mut store, run_id),
            RunStatus::Completed
        );

        let invocations = inspector.invocations.lock().unwrap().clone();
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[0].0, 1);
        assert_eq!(invocations[1].0, 2);
        assert!(
            invocations[1]
                .1
                .windows(2)
                .any(|pair| pair[0] == "--session" && pair[1] == "ses_A"),
            "{:?}",
            invocations[1].1
        );

        let config_path = temp
            .path()
            .join("runs")
            .join(run_id.to_string())
            .join("provider-output")
            .join("opencode")
            .join(session.id().to_string())
            .join("invocation-2.config.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(
            config["permission"]["bash"]["python3 -c \"from calc import add; assert add(2,3)==5\""],
            serde_json::json!("allow")
        );
    }

    /// Regression for a real end-to-end failure: approving a compound denial
    /// (`pwd && ls -la`) used to grant the whole joined string as one
    /// pattern, which matched neither of opencode's own split parts and the
    /// same denial repeated forever. Approval must grant every split part.
    #[test]
    fn approving_a_compound_denial_grants_every_split_part() {
        let backend = HaltThenResumeBackend::new(
            COMPOUND_PERMISSION_HALT_OUTPUT,
            COMPOUND_RESUME_AFTER_APPROVAL_OUTPUT,
        );
        let (temp, _database, run_id, mut store, provider) = fixture_with(backend);
        let mut engine = WorkflowEngine::new(provider, "fix the bug");
        let request_id = loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::NeedsUser { requests } => break requests[0],
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        };
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();

        engine
            .resolve_attention_with_response(&mut store, run_id, request_id, None)
            .unwrap();
        assert_eq!(
            drive_to_completion(&mut engine, &mut store, run_id),
            RunStatus::Completed
        );

        let config_path = temp
            .path()
            .join("runs")
            .join(run_id.to_string())
            .join("provider-output")
            .join("opencode")
            .join(session.id().to_string())
            .join("invocation-2.config.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(
            config["permission"]["bash"]["pwd"],
            serde_json::json!("allow")
        );
        assert_eq!(
            config["permission"]["bash"]["ls -la"],
            serde_json::json!("allow")
        );
    }

    /// A decline carries the operator's text into the continuation instead
    /// of granting anything, and the resumed config never allows the denied
    /// command.
    #[test]
    fn a_declined_permission_halt_resumes_without_granting_anything() {
        let backend = HaltThenResumeBackend::default();
        let (temp, _database, run_id, mut store, provider) = fixture_with(backend);
        let mut engine = WorkflowEngine::new(provider, "fix the bug");
        let request_id = loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::NeedsUser { requests } => break requests[0],
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                status => panic!("unexpected status: {status:?}"),
            }
        };
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();

        engine
            .resolve_attention_with_response(
                &mut store,
                run_id,
                request_id,
                Some("Continue without running it."),
            )
            .unwrap();
        // Drive far enough to observe the resumed invocation's own config;
        // the fixture backend always replays a successful shape for
        // invocation 2+, which is fine here since only the config matters.
        drive_to_completion(&mut engine, &mut store, run_id);

        let config_path = temp
            .path()
            .join("runs")
            .join(run_id.to_string())
            .join("provider-output")
            .join("opencode")
            .join(session.id().to_string())
            .join("invocation-2.config.json");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(
            config["permission"]["bash"]
                .get("python3 -c \"from calc import add; assert add(2,3)==5\""),
            None,
            "a decline must never widen the allowlist"
        );
    }

    /// Shape copied from the real `err-402.jsonl`/`nomodel.jsonl` fixtures: a
    /// top-level `error` record fails the stage with a scrubbed message.
    #[test]
    fn a_vendor_error_fails_the_stage_with_a_scrubbed_message() {
        let (_temp, _database, run_id, mut store, provider) = fixture(INSUFFICIENT_BALANCE_OUTPUT);
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        assert_eq!(
            drive_to_completion(&mut engine, &mut store, run_id),
            RunStatus::Failed
        );
        let events = store.load_events(run_id).unwrap();
        assert!(events.iter().any(|event| matches!(
            event.event.kind(),
            DomainEventKind::ProviderFailed { reason: Some(reason), .. }
                if reason.contains("APIError") && reason.contains("Insufficient Balance")
        )));
    }

    /// opencode writes no corroborating final-message file the way Codex
    /// does, so a terminal `stop` step whose process then died any way other
    /// than a clean exit is never trusted as completion — it is reported as
    /// a recoverable interruption instead, exactly what `senate resume`
    /// exists for.
    #[test]
    fn a_stop_step_after_an_unclean_exit_is_a_recoverable_interruption_not_a_completion() {
        let (_temp, _database, run_id, mut store, provider) = fixture_with(
            FixtureBackend::new(SUCCESS_OUTPUT).with_exit(Some(ExitResult::ExitCode { code: 1 })),
        );
        let mut engine = WorkflowEngine::new(provider, "fixture task");
        loop {
            match engine.drive(&mut store, run_id).unwrap() {
                EngineStatus::Interrupted { .. } => break,
                EngineStatus::Finished { run_status } => {
                    panic!("unexpected terminal status: {run_status:?}")
                }
                EngineStatus::Advanced { .. } | EngineStatus::WaitingForProvider { .. } => {}
                other => panic!("unexpected status: {other:?}"),
            }
        }
        assert!(store.list_artifacts(run_id).unwrap().is_empty());
        let session = store.list_provider_sessions(run_id).unwrap().pop().unwrap();
        assert_eq!(session.status(), ProviderSessionStatus::Interrupted);
    }

    /// Session binding is generic over first-invocation vs. resumed-invocation:
    /// whichever record arrives first for an invocation binds (or re-confirms)
    /// native identity, and the emitted signal is `Started` only for signal
    /// index zero — every later invocation, including a resume, reports
    /// `Resumed` instead, exercised here directly against `map_record` rather
    /// than through a full recovery drive, which is shared engine machinery
    /// Codex's own suite already exercises.
    #[test]
    fn a_resumed_invocations_first_record_reports_resumed_not_started() {
        let (_temp, _database, run_id, mut store, mut provider) = fixture(SUCCESS_OUTPUT);
        let workspace = store.load_workspace(run_id).unwrap().unwrap();
        let mut session = ProviderSessionRecord::new(
            ProviderSessionRecordId::new(),
            run_id,
            StageId::new("implementation").unwrap(),
            1,
            ProviderId::new("opencode").unwrap(),
            PROTOCOL_VERSION,
            None,
            OpencodeProvider::<FixtureBackend>::now(),
        );
        session
            .activate(
                ProviderSessionId::new("ses_A").unwrap(),
                None,
                OpencodeProvider::<FixtureBackend>::now(),
            )
            .unwrap();
        // A resume rebinds a process, which returns the session to
        // `Starting` until the next record confirms it again.
        let process = provider
            .manager
            .prepare_with_input(
                &mut store,
                run_id,
                StageId::new("implementation").unwrap(),
                1,
                2,
                Path::new("/bin/true"),
                vec![],
                BTreeMap::new(),
                &[],
            )
            .unwrap();
        session
            .bind_process(process.id(), 2, OpencodeProvider::<FixtureBackend>::now())
            .unwrap();

        let request = ProviderRequest::new(
            run_id,
            StageId::new("implementation").unwrap(),
            StageKind::Implementation,
            StageStatus::Running,
            Role::Implementer,
            "fixture task".to_owned(),
            workspace.worktree_path().to_path_buf(),
            1,
            1,
            Some(ProviderSessionId::new("ses_A").unwrap()),
            vec![],
        );
        let event = OpencodeEvent {
            session_id: "ses_A".to_owned(),
            kind: OpencodeKind::StepStart,
        };
        let chunk = OutputChunk::new(process.id(), OutputStream::Stdout, 0, 0, Vec::new()).unwrap();
        let poll = provider
            .map_record(
                &mut store,
                &request,
                session,
                chunk,
                0,
                event,
                ManagedProcessStatus::Preparing,
                false,
            )
            .unwrap();
        match poll {
            ProviderPoll::Emission { signals, .. } => {
                assert_eq!(signals, vec![ProviderSignal::Resumed]);
            }
            other => panic!("expected an Emission carrying Resumed, got {other:?}"),
        }
    }
}
