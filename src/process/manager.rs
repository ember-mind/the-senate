use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::domain::{RunId, StageId};
use crate::store::{SqliteStore, process_root};

use super::{
    BackendSessionState, ExitResult, ManagedProcess, ManagedProcessId, ManagedProcessStatus,
    OutputChunk, OutputCursor, OutputStream, ProcessBackend, ProcessError, ProcessInspection,
    ProcessSpec, TerminationSignal, TmuxBackend,
};

const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Five seconds for Ctrl-C: long enough for an agent to finish the turn it is
/// writing and record its own exit, which is the outcome worth waiting for.
const INTERRUPT_POLLS: usize = 100;
/// Three seconds for SIGTERM. Whatever is still alive here already ignored
/// Ctrl-C, so this is a courtesy to children that handle only the conventional
/// signal, not a second full wait.
const TERMINATE_POLLS: usize = 60;
/// One second after SIGKILL, which the kernel does not negotiate. The wait is
/// for the reaping to land in the store, not for the process to decide.
const KILL_POLLS: usize = 20;

pub struct ProcessManager<B> {
    root: PathBuf,
    backend: B,
    /// How long one poll of the termination ladder waits. Only the pace is
    /// adjustable, never how many rungs there are: a test that skipped a rung
    /// would stop proving the ladder is climbed in full.
    poll_interval: Duration,
}

impl<B: ProcessBackend> ProcessManager<B> {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, backend: B) -> Self {
        Self {
            root: root.into(),
            backend,
            poll_interval: INTERRUPT_POLL_INTERVAL,
        }
    }

    /// Runs the termination ladder at a different pace, for tests that must
    /// climb all of it without waiting out the real grace periods.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    #[must_use]
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Persists one launch intent before creating files or backend resources.
    ///
    /// # Errors
    /// Rejects invalid run/stage/workspace/apply state, duplicate attempts,
    /// malformed commands, or filesystem/persistence failures.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        executable: impl Into<PathBuf>,
        argv: Vec<OsString>,
        environment: BTreeMap<OsString, OsString>,
    ) -> Result<ManagedProcess, ProcessError> {
        self.prepare_internal(
            store,
            run_id,
            stage_id,
            attempt,
            1,
            executable,
            argv,
            environment,
            None,
        )
    }

    /// Persists one exact invocation with immutable stdin bytes.
    ///
    /// # Errors
    /// Rejects invalid identity, execution guards, input conflicts, or I/O failures.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_with_input(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        invocation: u32,
        executable: impl Into<PathBuf>,
        argv: Vec<OsString>,
        environment: BTreeMap<OsString, OsString>,
        input: &[u8],
    ) -> Result<ManagedProcess, ProcessError> {
        self.prepare_internal(
            store,
            run_id,
            stage_id,
            attempt,
            invocation,
            executable,
            argv,
            environment,
            Some(input),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_internal(
        &self,
        store: &mut SqliteStore,
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        invocation: u32,
        executable: impl Into<PathBuf>,
        argv: Vec<OsString>,
        environment: BTreeMap<OsString, OsString>,
        input: Option<&[u8]>,
    ) -> Result<ManagedProcess, ProcessError> {
        let workspace = store.load_workspace(run_id)?.ok_or(
            crate::store::StoreError::ExecutionWorkspaceNotReady {
                run_id,
                status: None,
            },
        )?;
        let process_id = ManagedProcessId::new();
        let directory = self.process_directory(run_id, process_id);
        let mut spec = ProcessSpec::new(
            executable,
            argv,
            workspace.worktree_path(),
            environment,
            directory.join("stdout.log"),
            directory.join("stderr.log"),
        )?;
        if let Some(input) = input {
            let input_path = directory.join("stdin.jsonl");
            std::fs::create_dir_all(&directory)?;
            write_immutable_file(&input_path, input)?;
            spec = spec.with_stdin(input_path, sha256(input))?;
        }
        let process = ManagedProcess::preparing(
            process_id,
            run_id,
            stage_id,
            attempt,
            invocation,
            self.backend.kind().to_owned(),
            self.backend.session_id(process_id),
            spec,
            now(),
        )?;
        let persisted = store.insert_managed_process_intent(&process)?;
        Self::materialize(&persisted)?;
        Ok(persisted)
    }

    /// Claims and starts one prepared process exactly once per observed revision.
    ///
    /// # Errors
    /// Returns concurrency, ownership, backend, or reconciliation failures.
    pub fn start(
        &self,
        store: &mut SqliteStore,
        process_id: ManagedProcessId,
    ) -> Result<ProcessInspection, ProcessError> {
        let inspection = self.reconcile(store, process_id)?;
        let process = inspection.process;
        if process.backend_kind() != self.backend.kind() {
            return Err(ProcessError::InvalidStoredProcess("backend kind mismatch"));
        }
        if process.status() != ManagedProcessStatus::Preparing {
            return self.inspection(
                process,
                inspection.backend_session,
                inspection.exit_evidence,
            );
        }
        Self::materialize(&process)?;
        let expected = process.revision();
        let mut starting = process;
        starting.transition(ManagedProcessStatus::Starting, now(), None, None)?;
        let starting = store.update_managed_process(&starting, expected, true)?;
        if let Err(error) = self
            .backend
            .start(&starting, &Self::manifest_path(&starting)?)
        {
            Self::persist_broken(store, &starting, "backend start failed")?;
            return Err(error);
        }
        self.reconcile(store, process_id)
    }

    /// Returns reconciled process and infrastructure evidence.
    ///
    /// # Errors
    /// Returns persistence, ownership, backend, evidence, or filesystem failures.
    pub fn inspect(
        &self,
        store: &mut SqliteStore,
        process_id: ManagedProcessId,
    ) -> Result<ProcessInspection, ProcessError> {
        self.reconcile(store, process_id)
    }

    /// Reconciles persisted intent against exit file and owned backend session.
    ///
    /// # Errors
    /// Persists safe broken state for corrupt/foreign evidence, then returns typed failure.
    pub fn reconcile(
        &self,
        store: &mut SqliteStore,
        process_id: ManagedProcessId,
    ) -> Result<ProcessInspection, ProcessError> {
        let mut process = store.load_managed_process(process_id)?;
        if process.backend_kind() != self.backend.kind() {
            return Err(ProcessError::InvalidStoredProcess("backend kind mismatch"));
        }
        if process.status() == ManagedProcessStatus::Preparing
            && let Err(error) = Self::materialize(&process)
        {
            Self::persist_broken(store, &process, "process files conflict")?;
            return Err(error);
        }

        // Observe supervisor state before exit evidence. Managed runner durably writes exit.json
        // and syncs its directory before returning; normal tmux disappearance happens afterward.
        // Therefore evidence read after Absent cannot predate that Absent observation.
        let backend_session = match self.backend.inspect_session(&process) {
            Ok(session) => session,
            Err(error) => {
                Self::persist_broken(store, &process, "backend ownership mismatch")?;
                return Err(error);
            }
        };
        let exit = match self.backend.read_exit_evidence(&process) {
            Ok(exit) => exit,
            Err(error) => {
                Self::persist_broken(store, &process, "invalid exit evidence")?;
                return Err(error);
            }
        };

        if let Some(evidence) = exit.as_ref() {
            let target = match evidence.result() {
                ExitResult::RunnerError { message } => {
                    if can_mark_broken(process.status()) {
                        let expected = process.revision();
                        process.transition(
                            ManagedProcessStatus::Broken,
                            now(),
                            None,
                            Some(message.clone()),
                        )?;
                        process = store.update_managed_process(&process, expected, false)?;
                    }
                    ManagedProcessStatus::Broken
                }
                ExitResult::ExitCode { .. } | ExitResult::Signal { .. } => {
                    if process.interrupt_requested() {
                        ManagedProcessStatus::Interrupted
                    } else {
                        ManagedProcessStatus::Exited
                    }
                }
            };
            if target != ManagedProcessStatus::Broken
                && !matches!(
                    process.status(),
                    ManagedProcessStatus::Exited
                        | ManagedProcessStatus::Interrupted
                        | ManagedProcessStatus::Cleaned
                )
            {
                let expected = process.revision();
                process.transition(target, now(), Some(evidence), None)?;
                process = store.update_managed_process(&process, expected, false)?;
            }
        } else {
            match (process.status(), backend_session) {
                (
                    ManagedProcessStatus::Preparing | ManagedProcessStatus::Starting,
                    BackendSessionState::Owned,
                ) => {
                    let expected = process.revision();
                    process.transition(ManagedProcessStatus::Running, now(), None, None)?;
                    process = store.update_managed_process(&process, expected, false)?;
                }
                (ManagedProcessStatus::Missing, BackendSessionState::Owned) => {
                    let expected = process.revision();
                    let target = if process.interrupt_requested() {
                        ManagedProcessStatus::Interrupting
                    } else {
                        ManagedProcessStatus::Running
                    };
                    process.transition(target, now(), None, None)?;
                    process = store.update_managed_process(&process, expected, false)?;
                }
                (
                    ManagedProcessStatus::Starting
                    | ManagedProcessStatus::Running
                    | ManagedProcessStatus::Interrupting,
                    BackendSessionState::Absent,
                ) => {
                    let expected = process.revision();
                    process.transition(
                        ManagedProcessStatus::Missing,
                        now(),
                        None,
                        Some("owned tmux session absent without exit evidence".to_owned()),
                    )?;
                    process = store.update_managed_process(&process, expected, false)?;
                }
                _ => {}
            }
        }
        self.inspection(process, backend_session, exit)
    }

    /// Reads raw unacknowledged bytes. Durable cursor is unchanged.
    ///
    /// # Errors
    /// Rejects invalid sizes, truncation, or I/O failures.
    pub fn read_output(
        &self,
        store: &SqliteStore,
        process_id: ManagedProcessId,
        stream: OutputStream,
        max_bytes: usize,
    ) -> Result<OutputChunk, ProcessError> {
        let process = store.load_managed_process(process_id)?;
        let cursor = process.cursor(stream);
        self.backend
            .read_output(&process, stream, cursor.offset(), max_bytes)
    }

    /// Reads a bounded tail without advancing durable provider-consumption cursor.
    ///
    /// # Errors
    /// Returns process lookup, size, truncation, or backend I/O failures.
    pub fn read_output_tail(
        &self,
        store: &SqliteStore,
        process_id: ManagedProcessId,
        stream: OutputStream,
        max_bytes: usize,
    ) -> Result<(OutputChunk, u64, bool), ProcessError> {
        if max_bytes == 0 {
            return Err(ProcessError::InvalidReadSize(0));
        }
        let process = store.load_managed_process(process_id)?;
        let total_bytes = self.backend.output_length(&process, stream)?;
        let max_bytes_u64 =
            u64::try_from(max_bytes).map_err(|_| ProcessError::InvalidReadSize(max_bytes))?;
        let offset = total_bytes.saturating_sub(max_bytes_u64);
        let requested = usize::try_from(total_bytes.saturating_sub(offset))
            .map_err(|_| ProcessError::InvalidReadSize(max_bytes))?;
        let chunk = if requested == 0 {
            OutputChunk::new(
                process.id(),
                stream,
                process.cursor(stream).revision(),
                offset,
                Vec::new(),
            )?
        } else {
            self.backend
                .read_output(&process, stream, offset, requested)?
        };
        Ok((chunk, total_bytes, offset > 0))
    }

    /// Advances one stream cursor after consumer durably accepts bytes.
    ///
    /// # Errors
    /// Rejects stale cursors, out-of-chunk offsets, output truncation, or `SQLite` failures.
    pub fn acknowledge_output(
        &self,
        store: &mut SqliteStore,
        chunk: &OutputChunk,
        acknowledged_end: u64,
    ) -> Result<OutputCursor, ProcessError> {
        let process = store.load_managed_process(chunk.process_id())?;
        let length = self.backend.output_length(&process, chunk.stream())?;
        if length < acknowledged_end {
            return Err(ProcessError::OutputTruncated(process.id()));
        }
        store.acknowledge_process_output(chunk, acknowledged_end)
    }

    /// Persists interrupt intent, then climbs the termination ladder until the
    /// process group is gone or the ladder runs out.
    ///
    /// Each rung is sent to the group and given its own bounded grace period.
    /// Ctrl-C comes first and is given the longest wait, because it is the only
    /// rung an agent can shut itself down cleanly under; the rungs below it
    /// exist for what the agent leaves behind. A stop that gave up after Ctrl-C
    /// left worker pools running for days, so giving up now means the kernel
    /// itself refused, not that a child declined to listen.
    ///
    /// # Errors
    /// Rejects invalid status, foreign ownership, persistence failures, or a
    /// process that survived every rung.
    pub fn interrupt(
        &self,
        store: &mut SqliteStore,
        process_id: ManagedProcessId,
    ) -> Result<ProcessInspection, ProcessError> {
        let mut inspection = self.reconcile(store, process_id)?;
        if matches!(
            inspection.process.status(),
            ManagedProcessStatus::Exited
                | ManagedProcessStatus::Interrupted
                | ManagedProcessStatus::Missing
                | ManagedProcessStatus::Broken
                | ManagedProcessStatus::Cleaned
        ) {
            return Ok(inspection);
        }
        // Intent is persisted before the signal, so a failure to signal leaves
        // the process in Interrupting with nothing having been sent. Asking
        // again has to be allowed to finish that job: the runtime evidence a
        // signal needs may simply not have been written yet when the first
        // attempt ran, and the caller retrying is the thing that resolves it.
        // Rejecting the second attempt as an invalid transition would strand
        // the process — marked as being interrupted, never actually signalled.
        let process = match inspection.process.status() {
            ManagedProcessStatus::Running => {
                let expected = inspection.process.revision();
                inspection.process.transition(
                    ManagedProcessStatus::Interrupting,
                    now(),
                    None,
                    None,
                )?;
                store.update_managed_process(&inspection.process, expected, false)?
            }
            ManagedProcessStatus::Interrupting => inspection.process.clone(),
            status => {
                return Err(ProcessError::InvalidTransition {
                    from: status,
                    to: ManagedProcessStatus::Interrupting,
                });
            }
        };
        for signal in TerminationSignal::LADDER {
            self.backend.signal(&process, signal)?;
            for _ in 0..Self::grace_polls(signal) {
                inspection = self.reconcile(store, process_id)?;
                if settled(&inspection) {
                    return Ok(inspection);
                }
                std::thread::sleep(self.poll_interval);
            }
            inspection = self.reconcile(store, process_id)?;
            if settled(&inspection) || inspection.process.status() == ManagedProcessStatus::Missing
            {
                return Ok(inspection);
            }
        }
        Err(ProcessError::InterruptTimeout(process_id))
    }

    /// How long one rung of the ladder is given before the next one is sent.
    const fn grace_polls(signal: TerminationSignal) -> usize {
        match signal {
            TerminationSignal::Interrupt => INTERRUPT_POLLS,
            TerminationSignal::Terminate => TERMINATE_POLLS,
            TerminationSignal::Kill => KILL_POLLS,
        }
    }

    /// Removes owned backend session while preserving manifest and output history.
    ///
    /// # Errors
    /// Rejects active processes, foreign sessions, or persistence/backend failures.
    pub fn cleanup(
        &self,
        store: &mut SqliteStore,
        process_id: ManagedProcessId,
    ) -> Result<ProcessInspection, ProcessError> {
        let inspection = self.reconcile(store, process_id)?;
        if inspection.process.status() == ManagedProcessStatus::Cleaned {
            return Ok(inspection);
        }
        if inspection.process.status().is_active() {
            return Err(ProcessError::InvalidTransition {
                from: inspection.process.status(),
                to: ManagedProcessStatus::Cleaned,
            });
        }
        self.backend.cleanup(&inspection.process)?;
        let expected = inspection.process.revision();
        let mut process = inspection.process;
        process.transition(ManagedProcessStatus::Cleaned, now(), None, None)?;
        let process = store.update_managed_process(&process, expected, false)?;
        self.inspection(
            process,
            BackendSessionState::Absent,
            inspection.exit_evidence,
        )
    }

    fn process_directory(&self, run_id: RunId, process_id: ManagedProcessId) -> PathBuf {
        self.root
            .join(run_id.to_string())
            .join("processes")
            .join(process_id.to_string())
    }

    fn manifest_path(process: &ManagedProcess) -> Result<PathBuf, ProcessError> {
        process
            .spec()
            .stdout_path()
            .parent()
            .map(|directory| directory.join("spec.json"))
            .ok_or(ProcessError::InvalidSpec("stdout path has no parent"))
    }

    fn materialize(process: &ManagedProcess) -> Result<(), ProcessError> {
        let manifest_path = Self::manifest_path(process)?;
        let directory = manifest_path
            .parent()
            .ok_or(ProcessError::InvalidSpec("manifest path has no parent"))?;
        std::fs::create_dir_all(directory)?;
        let manifest = process.manifest_json()?;
        validate_input(process.spec())?;
        write_immutable_file(&manifest_path, manifest.as_bytes())?;
        touch_append_only(process.spec().stdout_path())?;
        touch_append_only(process.spec().stderr_path())?;
        Ok(())
    }

    fn inspection(
        &self,
        process: ManagedProcess,
        backend_session: BackendSessionState,
        exit_evidence: Option<super::ExitEvidence>,
    ) -> Result<ProcessInspection, ProcessError> {
        let stdout_length = self.backend.output_length(&process, OutputStream::Stdout)?;
        let stderr_length = self.backend.output_length(&process, OutputStream::Stderr)?;
        Ok(ProcessInspection {
            process,
            backend_session,
            stdout_length,
            stderr_length,
            exit_evidence,
        })
    }

    fn persist_broken(
        store: &mut SqliteStore,
        process: &ManagedProcess,
        reason: &str,
    ) -> Result<(), ProcessError> {
        if !can_mark_broken(process.status()) {
            return Ok(());
        }
        let expected = process.revision();
        let mut broken = process.clone();
        broken.transition(
            ManagedProcessStatus::Broken,
            now(),
            None,
            Some(reason.to_owned()),
        )?;
        store.update_managed_process(&broken, expected, false)?;
        Ok(())
    }
}

fn validate_input(spec: &ProcessSpec) -> Result<(), ProcessError> {
    let (Some(path), Some(expected)) = (spec.stdin_path(), spec.stdin_sha256()) else {
        return Ok(());
    };
    let bytes = std::fs::read(path)?;
    if sha256(&bytes) != expected {
        return Err(ProcessError::InvalidSpec("stdin content hash mismatch"));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

impl ProcessManager<TmuxBackend> {
    /// Builds default tmux manager from The Senate data path and current executable.
    ///
    /// # Errors
    /// Returns data-path or current-executable failures.
    pub fn from_environment() -> Result<Self, ProcessError> {
        Ok(Self::new(
            process_root()?,
            TmuxBackend::new(std::env::current_exe()?),
        ))
    }
}

fn write_immutable_file(path: &Path, bytes: &[u8]) -> Result<(), ProcessError> {
    if path.exists() {
        if std::fs::read(path)? == bytes {
            return Ok(());
        }
        return Err(ProcessError::InvalidSpec(
            "existing manifest differs from persisted intent",
        ));
    }
    let directory = path
        .parent()
        .ok_or(ProcessError::InvalidSpec("manifest path has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(file) => file.sync_all()?,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(path)? != bytes {
                return Err(ProcessError::InvalidSpec(
                    "existing manifest differs from persisted intent",
                ));
            }
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(())
}

fn touch_append_only(path: &Path) -> Result<(), ProcessError> {
    OpenOptions::new().create(true).append(true).open(path)?;
    Ok(())
}

/// Whether a stop has finished: the process is done and its backend session is
/// no longer holding anything open.
///
/// `Interrupted` with an owned session still counts as running, because the
/// session outliving the signal is what a child that ignored it looks like —
/// exactly the case the next rung of the ladder exists for.
fn settled(inspection: &ProcessInspection) -> bool {
    matches!(
        inspection.process.status(),
        ManagedProcessStatus::Interrupted
            | ManagedProcessStatus::Exited
            | ManagedProcessStatus::Broken
    ) && !(inspection.process.status() == ManagedProcessStatus::Interrupted
        && inspection.backend_session == BackendSessionState::Owned)
}

fn can_mark_broken(status: ManagedProcessStatus) -> bool {
    matches!(
        status,
        ManagedProcessStatus::Preparing
            | ManagedProcessStatus::Starting
            | ManagedProcessStatus::Running
            | ManagedProcessStatus::Interrupting
            | ManagedProcessStatus::Missing
    )
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, EventId, EventMetadata, Run, WorkflowDefinition, WorkflowKind,
    };
    use crate::process::{BackendAvailability, BackendSessionId, ExitEvidence};
    use crate::store::{ResolvedConfigSnapshot, RunInput};
    use crate::workspace::WorkspaceManager;

    #[derive(Default)]
    struct BackendState {
        started: bool,
        absent: bool,
        evidence: Option<ExitResult>,
        complete_on_next_inspection: Option<ExitResult>,
        calls: Vec<&'static str>,
        /// Every signal the manager sent, in order, so a test can read back
        /// which rungs of the ladder were actually climbed.
        signals: Vec<TerminationSignal>,
        /// The gentlest signal this fixture will die to. `Interrupt` models a
        /// well-behaved agent; `Kill` models the worker pool that ignores
        /// everything catchable.
        dies_to: Option<TerminationSignal>,
    }

    #[derive(Clone, Default)]
    struct InterleavingBackend {
        state: Arc<Mutex<BackendState>>,
    }

    impl InterleavingBackend {
        fn complete_during_next_inspection(&self, result: ExitResult) {
            let mut state = self.state.lock().unwrap();
            state.complete_on_next_inspection = Some(result);
            state.calls.clear();
        }

        fn lose_session_without_evidence(&self) {
            let mut state = self.state.lock().unwrap();
            state.absent = true;
            state.evidence = None;
            state.calls.clear();
        }

        fn finish(&self, result: ExitResult) {
            let mut state = self.state.lock().unwrap();
            state.absent = true;
            state.evidence = Some(result);
            state.calls.clear();
        }

        fn calls(&self) -> Vec<&'static str> {
            self.state.lock().unwrap().calls.clone()
        }

        /// Models a child that only dies to `signal` and ignores every gentler
        /// one, the way a Jest worker pool ignores Ctrl-C.
        fn survives_until(&self, signal: TerminationSignal) {
            let mut state = self.state.lock().unwrap();
            state.dies_to = Some(signal);
            state.calls.clear();
        }

        fn signals(&self) -> Vec<TerminationSignal> {
            self.state.lock().unwrap().signals.clone()
        }
    }

    impl ProcessBackend for InterleavingBackend {
        fn kind(&self) -> &'static str {
            "interleaving_fixture"
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

        fn start(&self, _process: &ManagedProcess, _manifest: &Path) -> Result<(), ProcessError> {
            let mut state = self.state.lock().unwrap();
            state.started = true;
            state.absent = false;
            Ok(())
        }

        fn inspect_session(
            &self,
            _process: &ManagedProcess,
        ) -> Result<BackendSessionState, ProcessError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("inspect_session");
            if let Some(result) = state.complete_on_next_inspection.take() {
                state.evidence = Some(result);
                state.absent = true;
            }
            Ok(if state.started && !state.absent {
                BackendSessionState::Owned
            } else {
                BackendSessionState::Absent
            })
        }

        fn read_output(
            &self,
            process: &ManagedProcess,
            stream: OutputStream,
            offset: u64,
            _max_bytes: usize,
        ) -> Result<OutputChunk, ProcessError> {
            OutputChunk::new(
                process.id(),
                stream,
                process.cursor(stream).revision(),
                offset,
                Vec::new(),
            )
        }

        fn output_length(
            &self,
            _process: &ManagedProcess,
            _stream: OutputStream,
        ) -> Result<u64, ProcessError> {
            Ok(0)
        }

        fn read_exit_evidence(
            &self,
            process: &ManagedProcess,
        ) -> Result<Option<ExitEvidence>, ProcessError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push("read_exit_evidence");
            Ok(state.evidence.clone().map(|result| {
                let at = now();
                ExitEvidence::new(
                    process.id(),
                    process.command_fingerprint().to_owned(),
                    result,
                    false,
                    at,
                    at,
                )
            }))
        }

        fn signal(
            &self,
            _process: &ManagedProcess,
            signal: TerminationSignal,
        ) -> Result<(), ProcessError> {
            let dies_to = {
                let mut state = self.state.lock().unwrap();
                state.signals.push(signal);
                state.dies_to.unwrap_or(TerminationSignal::Interrupt)
            };
            if signal >= dies_to {
                self.finish(ExitResult::Signal { signal: 2 });
            }
            Ok(())
        }

        fn cleanup(&self, _process: &ManagedProcess) -> Result<(), ProcessError> {
            Ok(())
        }
    }

    struct Fixture {
        _temp: TempDir,
        store: SqliteStore,
        manager: ProcessManager<InterleavingBackend>,
        backend: InterleavingBackend,
        process_id: ManagedProcessId,
    }

    impl Fixture {
        fn running() -> Self {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            initialize_repository(&source);
            let run_id = RunId::new();
            let stage_id = StageId::new("implementation").unwrap();
            let created_at = now();
            let config_id = ConfigSnapshotId::new(format!("race-{run_id}")).unwrap();
            let run = Run::new(
                run_id,
                WorkflowDefinition::built_in(WorkflowKind::Fast),
                config_id.clone(),
                created_at,
            );
            let input = RunInput::new(run_id, "process race fixture", created_at).unwrap();
            let config = ResolvedConfigSnapshot::new(
                config_id,
                1,
                json!({"provider":"fixture"}),
                created_at,
            )
            .unwrap();
            let event = run.created_event(EventMetadata::new(EventId::new(), created_at));
            let mut store = SqliteStore::open(temp.path().join("senate.db")).unwrap();
            store
                .create_run_with_input(&run, &input, &config, &[event])
                .unwrap();
            WorkspaceManager::new(temp.path().join("worktrees"))
                .prepare_run_workspace(&mut store, run_id, &source)
                .unwrap();
            let backend = InterleavingBackend::default();
            // The ladder's real grace periods are seconds long by design; the
            // rung count is what these tests are about, not the waiting.
            let manager = ProcessManager::new(temp.path().join("runs"), backend.clone())
                .with_poll_interval(Duration::ZERO);
            let process = manager
                .prepare(
                    &mut store,
                    run_id,
                    stage_id,
                    0,
                    "/bin/true",
                    Vec::new(),
                    BTreeMap::new(),
                )
                .unwrap();
            let started = manager.start(&mut store, process.id()).unwrap();
            assert_eq!(started.process.status(), ManagedProcessStatus::Running);
            Self {
                _temp: temp,
                store,
                manager,
                backend,
                process_id: process.id(),
            }
        }
    }

    /// The property the old stop lacked: a child that ignores Ctrl-C is still
    /// stopped. Before escalation existed, this run left its process group
    /// alive and returned `InterruptTimeout`, which is how worker pools
    /// survived their run by days.
    #[test]
    fn a_process_that_ignores_ctrl_c_is_escalated_until_it_dies() {
        let mut fixture = Fixture::running();
        fixture.backend.survives_until(TerminationSignal::Kill);

        let inspection = fixture
            .manager
            .interrupt(&mut fixture.store, fixture.process_id)
            .expect("a killed group settles the stop");

        assert!(
            settled(&inspection),
            "the process is stopped, not left running"
        );
        assert_eq!(
            fixture.backend.signals(),
            vec![
                TerminationSignal::Interrupt,
                TerminationSignal::Terminate,
                TerminationSignal::Kill,
            ],
            "the ladder is climbed in order, gentlest first"
        );
    }

    /// The other half of the same property: escalation is a fallback, not the
    /// normal path. An agent that honours Ctrl-C is never signalled again, so
    /// it keeps the chance to write its own exit evidence.
    #[test]
    fn a_process_that_honours_ctrl_c_is_never_escalated() {
        let mut fixture = Fixture::running();
        fixture.backend.survives_until(TerminationSignal::Interrupt);

        fixture
            .manager
            .interrupt(&mut fixture.store, fixture.process_id)
            .expect("an interrupted process settles the stop");

        assert_eq!(
            fixture.backend.signals(),
            vec![TerminationSignal::Interrupt],
            "nothing harsher than Ctrl-C is sent to a process that stopped"
        );
    }

    #[test]
    fn absent_session_uses_exit_evidence_observed_after_absence() {
        let mut fixture = Fixture::running();
        fixture
            .backend
            .complete_during_next_inspection(ExitResult::ExitCode { code: 0 });

        let inspection = fixture
            .manager
            .reconcile(&mut fixture.store, fixture.process_id)
            .unwrap();

        assert_eq!(
            fixture.backend.calls(),
            vec!["inspect_session", "read_exit_evidence"]
        );
        assert_eq!(inspection.backend_session, BackendSessionState::Absent);
        assert_eq!(inspection.process.status(), ManagedProcessStatus::Exited);
        assert_eq!(
            inspection.process.exit_result(),
            Some(&ExitResult::ExitCode { code: 0 })
        );
    }

    #[test]
    fn absent_session_without_exit_evidence_remains_missing() {
        let mut fixture = Fixture::running();
        fixture.backend.lose_session_without_evidence();

        let inspection = fixture
            .manager
            .reconcile(&mut fixture.store, fixture.process_id)
            .unwrap();

        assert_eq!(inspection.process.status(), ManagedProcessStatus::Missing);
        assert!(inspection.exit_evidence.is_none());
    }

    #[test]
    fn exit_results_preserve_failure_signal_runner_and_interrupt_semantics() {
        let cases = [
            (
                ExitResult::ExitCode { code: 42 },
                ManagedProcessStatus::Exited,
            ),
            (
                ExitResult::Signal { signal: 9 },
                ManagedProcessStatus::Exited,
            ),
            (
                ExitResult::RunnerError {
                    message: "fixture runner failed".to_owned(),
                },
                ManagedProcessStatus::Broken,
            ),
        ];
        for (result, expected) in cases {
            let mut fixture = Fixture::running();
            fixture.backend.finish(result);
            let inspection = fixture
                .manager
                .reconcile(&mut fixture.store, fixture.process_id)
                .unwrap();
            assert_eq!(inspection.process.status(), expected);
        }

        let mut interrupted = Fixture::running();
        let inspection = interrupted
            .manager
            .interrupt(&mut interrupted.store, interrupted.process_id)
            .unwrap();
        assert_eq!(
            inspection.process.status(),
            ManagedProcessStatus::Interrupted
        );
        assert_eq!(
            inspection.process.exit_result(),
            Some(&ExitResult::Signal { signal: 2 })
        );
    }

    fn initialize_repository(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "senate@example.invalid"][..],
            &["config", "user.name", "The Senate Test"][..],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(path.join("README.md"), "fixture\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "README.md"])
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-qm", "fixture"])
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
    }
}
