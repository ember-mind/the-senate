use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::{RunId, StageId};

use super::{BackendSessionId, ManagedProcessId, ProcessError};

pub(crate) const PROCESS_SPEC_SCHEMA_VERSION: u32 = 2;
const LEGACY_PROCESS_SPEC_SCHEMA_VERSION: u32 = 1;
pub(crate) const EXIT_EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub(crate) const RUNTIME_EVIDENCE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    executable: PathBuf,
    argv: Vec<OsString>,
    working_directory: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    stdin_path: Option<PathBuf>,
    stdin_sha256: Option<String>,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl fmt::Debug for ProcessSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessSpec")
            .field("executable", &self.executable)
            .field("argv_count", &self.argv.len())
            .field("working_directory", &self.working_directory)
            .field("environment_override_count", &self.environment.len())
            .field("has_stdin", &self.stdin_path.is_some())
            .field("stdin_sha256", &self.stdin_sha256)
            .field("stdout_path", &self.stdout_path)
            .field("stderr_path", &self.stderr_path)
            .finish()
    }
}

impl ProcessSpec {
    /// Creates one immutable exact command specification.
    ///
    /// Environment inherits from runner and applies only supplied overrides.
    ///
    /// # Errors
    /// Rejects relative paths, invalid environment keys, NUL bytes, and shared output paths.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        executable: impl Into<PathBuf>,
        argv: Vec<OsString>,
        working_directory: impl Into<PathBuf>,
        environment: BTreeMap<OsString, OsString>,
        stdout_path: impl Into<PathBuf>,
        stderr_path: impl Into<PathBuf>,
    ) -> Result<Self, ProcessError> {
        if !cfg!(unix) {
            return Err(ProcessError::UnsupportedPlatform);
        }
        let spec = Self {
            executable: executable.into(),
            argv,
            working_directory: working_directory.into(),
            environment,
            stdin_path: None,
            stdin_sha256: None,
            stdout_path: stdout_path.into(),
            stderr_path: stderr_path.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Binds immutable stdin bytes by path and SHA-256 digest.
    ///
    /// # Errors
    /// Rejects relative paths, malformed hashes, or conflicting paths.
    pub(crate) fn with_stdin(
        mut self,
        path: impl Into<PathBuf>,
        sha256: impl Into<String>,
    ) -> Result<Self, ProcessError> {
        self.stdin_path = Some(path.into());
        self.stdin_sha256 = Some(sha256.into());
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), ProcessError> {
        if self.executable.as_os_str().is_empty() {
            return Err(ProcessError::InvalidSpec("executable must not be empty"));
        }
        if !self.executable.is_absolute()
            || !self.working_directory.is_absolute()
            || !self.stdout_path.is_absolute()
            || !self.stderr_path.is_absolute()
            || self
                .stdin_path
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
        {
            return Err(ProcessError::InvalidSpec("all paths must be absolute"));
        }
        if self.stdout_path == self.stderr_path {
            return Err(ProcessError::InvalidSpec(
                "stdout and stderr paths must differ",
            ));
        }
        if self
            .stdin_path
            .as_ref()
            .is_some_and(|path| path == &self.stdout_path || path == &self.stderr_path)
        {
            return Err(ProcessError::InvalidSpec(
                "stdin and output paths must differ",
            ));
        }
        match (&self.stdin_path, &self.stdin_sha256) {
            (Some(_), Some(hash))
                if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) => {}
            (None, None) => {}
            _ => return Err(ProcessError::InvalidSpec("invalid stdin binding")),
        }
        for value in self.argv.iter().chain(self.environment.values()) {
            if os_bytes(value)?.contains(&0) {
                return Err(ProcessError::InvalidSpec("OS values must not contain NUL"));
            }
        }
        for value in [
            self.executable.as_os_str(),
            self.working_directory.as_os_str(),
            self.stdout_path.as_os_str(),
            self.stderr_path.as_os_str(),
        ] {
            if os_bytes(value)?.contains(&0) {
                return Err(ProcessError::InvalidSpec("OS values must not contain NUL"));
            }
        }
        if let Some(path) = &self.stdin_path
            && os_bytes(path.as_os_str())?.contains(&0)
        {
            return Err(ProcessError::InvalidSpec("OS values must not contain NUL"));
        }
        for key in self.environment.keys() {
            let key = os_bytes(key)?;
            if key.is_empty() || key.contains(&b'=') || key.contains(&0) {
                return Err(ProcessError::InvalidSpec("invalid environment key"));
            }
            if sensitive_environment_name(key) {
                return Err(ProcessError::InvalidSpec(
                    "sensitive environment overrides must not be persisted",
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    #[must_use]
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    #[must_use]
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    #[must_use]
    pub fn stdin_path(&self) -> Option<&Path> {
        self.stdin_path.as_deref()
    }

    #[must_use]
    pub fn stdin_sha256(&self) -> Option<&str> {
        self.stdin_sha256.as_deref()
    }

    #[must_use]
    pub fn stdout_path(&self) -> &Path {
        &self.stdout_path
    }

    #[must_use]
    pub fn stderr_path(&self) -> &Path {
        &self.stderr_path
    }
}

fn sensitive_environment_name(name: &[u8]) -> bool {
    let upper = name.iter().map(u8::to_ascii_uppercase).collect::<Vec<_>>();
    [
        b"TOKEN".as_slice(),
        b"SECRET".as_slice(),
        b"PASSWORD".as_slice(),
        b"API_KEY".as_slice(),
        b"AUTH".as_slice(),
        b"COOKIE".as_slice(),
        b"CREDENTIAL".as_slice(),
        b"PRIVATE_KEY".as_slice(),
    ]
    .iter()
    .any(|needle| upper.windows(needle.len()).any(|window| window == *needle))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedProcessStatus {
    Preparing,
    Starting,
    Running,
    Interrupting,
    Exited,
    Interrupted,
    Missing,
    Broken,
    Cleaned,
}

impl ManagedProcessStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Interrupting => "interrupting",
            Self::Exited => "exited",
            Self::Interrupted => "interrupted",
            Self::Missing => "missing",
            Self::Broken => "broken",
            Self::Cleaned => "cleaned",
        }
    }

    pub(crate) fn from_str(value: &str) -> Result<Self, ProcessError> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "interrupting" => Ok(Self::Interrupting),
            "exited" => Ok(Self::Exited),
            "interrupted" => Ok(Self::Interrupted),
            "missing" => Ok(Self::Missing),
            "broken" => Ok(Self::Broken),
            "cleaned" => Ok(Self::Cleaned),
            _ => Err(ProcessError::InvalidStoredProcess("unknown status")),
        }
    }

    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Interrupting)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessRevision(u64);

impl ProcessRevision {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputCursor {
    offset: u64,
    revision: u64,
}

impl OutputCursor {
    pub(crate) const fn new(offset: u64, revision: u64) -> Self {
        Self { offset, revision }
    }

    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputChunk {
    process_id: ManagedProcessId,
    stream: OutputStream,
    cursor_revision: u64,
    start_offset: u64,
    end_offset: u64,
    bytes: Vec<u8>,
}

impl OutputChunk {
    pub(crate) fn new(
        process_id: ManagedProcessId,
        stream: OutputStream,
        cursor_revision: u64,
        start_offset: u64,
        bytes: Vec<u8>,
    ) -> Result<Self, ProcessError> {
        let length = u64::try_from(bytes.len())
            .map_err(|_| ProcessError::InvalidSpec("output chunk is too large"))?;
        let end_offset = start_offset
            .checked_add(length)
            .ok_or(ProcessError::InvalidSpec("output offset overflow"))?;
        Ok(Self {
            process_id,
            stream,
            cursor_revision,
            start_offset,
            end_offset,
            bytes,
        })
    }

    #[must_use]
    pub const fn process_id(&self) -> ManagedProcessId {
        self.process_id
    }

    #[must_use]
    pub const fn stream(&self) -> OutputStream {
        self.stream
    }

    #[must_use]
    pub const fn cursor_revision(&self) -> u64 {
        self.cursor_revision
    }

    #[must_use]
    pub const fn start_offset(&self) -> u64 {
        self.start_offset
    }

    #[must_use]
    pub const fn end_offset(&self) -> u64 {
        self.end_offset
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExitResult {
    ExitCode { code: i32 },
    Signal { signal: i32 },
    RunnerError { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitEvidence {
    schema_version: u32,
    process_id: ManagedProcessId,
    command_fingerprint: String,
    result: ExitResult,
    interrupt_observed: bool,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeEvidence {
    schema_version: u32,
    process_id: ManagedProcessId,
    command_fingerprint: String,
    runner_pid: u32,
    child_pid: u32,
    process_group_id: u32,
}

impl RuntimeEvidence {
    pub(crate) fn new(
        process_id: ManagedProcessId,
        command_fingerprint: String,
        runner_pid: u32,
        child_pid: u32,
    ) -> Self {
        Self {
            schema_version: RUNTIME_EVIDENCE_SCHEMA_VERSION,
            process_id,
            command_fingerprint,
            runner_pid,
            child_pid,
            process_group_id: child_pid,
        }
    }

    pub(crate) fn validate_for(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        if self.schema_version != RUNTIME_EVIDENCE_SCHEMA_VERSION
            || self.process_id != process.id()
            || self.command_fingerprint != process.command_fingerprint()
            || self.runner_pid == 0
            || self.child_pid == 0
            || self.process_group_id != self.child_pid
        {
            return Err(ProcessError::MissingRuntimeEvidence(process.id()));
        }
        Ok(())
    }

    pub(crate) const fn runner_pid(&self) -> u32 {
        self.runner_pid
    }

    pub(crate) const fn process_group_id(&self) -> u32 {
        self.process_group_id
    }
}

impl ExitEvidence {
    pub(crate) fn new(
        process_id: ManagedProcessId,
        command_fingerprint: String,
        result: ExitResult,
        interrupt_observed: bool,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
    ) -> Self {
        Self {
            schema_version: EXIT_EVIDENCE_SCHEMA_VERSION,
            process_id,
            command_fingerprint,
            result,
            interrupt_observed,
            started_at,
            finished_at: finished_at.max(started_at),
        }
    }

    pub(crate) fn validate_for(&self, process: &ManagedProcess) -> Result<(), ProcessError> {
        if self.schema_version != EXIT_EVIDENCE_SCHEMA_VERSION {
            return Err(ProcessError::InvalidExitEvidence {
                process_id: process.id(),
                reason: "unsupported schema version",
            });
        }
        if self.process_id != process.id()
            || self.command_fingerprint != process.command_fingerprint()
        {
            return Err(ProcessError::InvalidExitEvidence {
                process_id: process.id(),
                reason: "identity or fingerprint mismatch",
            });
        }
        if self.finished_at < self.started_at {
            return Err(ProcessError::InvalidExitEvidence {
                process_id: process.id(),
                reason: "timestamp regression",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn process_id(&self) -> ManagedProcessId {
        self.process_id
    }

    #[must_use]
    pub fn command_fingerprint(&self) -> &str {
        &self.command_fingerprint
    }

    #[must_use]
    pub const fn result(&self) -> &ExitResult {
        &self.result
    }

    #[must_use]
    pub const fn interrupt_observed(&self) -> bool {
        self.interrupt_observed
    }

    #[must_use]
    pub const fn started_at(&self) -> &DateTime<Utc> {
        &self.started_at
    }

    #[must_use]
    pub const fn finished_at(&self) -> &DateTime<Utc> {
        &self.finished_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedProcess {
    id: ManagedProcessId,
    run_id: RunId,
    stage_id: StageId,
    attempt: u32,
    invocation: u32,
    backend_kind: String,
    backend_session_id: BackendSessionId,
    status: ManagedProcessStatus,
    spec_schema_version: u32,
    spec: ProcessSpec,
    command_fingerprint: String,
    stdout_cursor: OutputCursor,
    stderr_cursor: OutputCursor,
    exit_result: Option<ExitResult>,
    interrupt_requested: bool,
    last_error: Option<String>,
    revision: ProcessRevision,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl ManagedProcess {
    #[allow(
        clippy::too_many_arguments,
        reason = "constructor captures complete immutable process identity"
    )]
    pub(crate) fn preparing(
        id: ManagedProcessId,
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        invocation: u32,
        backend_kind: String,
        backend_session_id: BackendSessionId,
        spec: ProcessSpec,
        now: DateTime<Utc>,
    ) -> Result<Self, ProcessError> {
        if invocation == 0 {
            return Err(ProcessError::InvalidSpec("invocation must be positive"));
        }
        let command_fingerprint = fingerprint(
            PROCESS_SPEC_SCHEMA_VERSION,
            run_id,
            &stage_id,
            attempt,
            invocation,
            &backend_kind,
            &spec,
        )?;
        let process = Self {
            id,
            run_id,
            stage_id,
            attempt,
            invocation,
            backend_kind,
            backend_session_id,
            status: ManagedProcessStatus::Preparing,
            spec_schema_version: PROCESS_SPEC_SCHEMA_VERSION,
            spec,
            command_fingerprint,
            stdout_cursor: OutputCursor::default(),
            stderr_cursor: OutputCursor::default(),
            exit_result: None,
            interrupt_requested: false,
            last_error: None,
            revision: ProcessRevision::default(),
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        };
        process.validate()?;
        Ok(process)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored(
        id: ManagedProcessId,
        run_id: RunId,
        stage_id: StageId,
        attempt: u32,
        invocation: u32,
        backend_kind: String,
        backend_session_id: BackendSessionId,
        status: ManagedProcessStatus,
        spec_schema_version: u32,
        manifest_json: &str,
        command_fingerprint: String,
        stdout_cursor: OutputCursor,
        stderr_cursor: OutputCursor,
        exit_result: Option<ExitResult>,
        interrupt_requested: bool,
        last_error: Option<String>,
        revision: ProcessRevision,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        started_at: Option<DateTime<Utc>>,
        finished_at: Option<DateTime<Utc>>,
    ) -> Result<Self, ProcessError> {
        let manifest: LaunchManifestV1 = serde_json::from_str(manifest_json)?;
        if manifest.schema_version != spec_schema_version
            || manifest.process_id != id
            || manifest.run_id != run_id
            || manifest.stage_id != stage_id
            || manifest.attempt != attempt
            || manifest.invocation() != invocation
            || manifest.backend_kind != backend_kind
            || manifest.backend_session_id != backend_session_id
            || manifest.command_fingerprint != command_fingerprint
        {
            return Err(ProcessError::InvalidStoredProcess(
                "manifest projection mismatch",
            ));
        }
        let spec = manifest.spec.decode()?;
        let process = Self {
            id,
            run_id,
            stage_id,
            attempt,
            invocation,
            backend_kind,
            backend_session_id,
            status,
            spec_schema_version,
            spec,
            command_fingerprint,
            stdout_cursor,
            stderr_cursor,
            exit_result,
            interrupt_requested,
            last_error,
            revision,
            created_at,
            updated_at,
            started_at,
            finished_at,
        };
        process.validate()?;
        Ok(process)
    }

    fn validate(&self) -> Result<(), ProcessError> {
        self.spec.validate()?;
        if self.backend_kind.is_empty()
            || !self
                .backend_kind
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        {
            return Err(ProcessError::InvalidStoredProcess("invalid backend kind"));
        }
        let expected = fingerprint(
            self.spec_schema_version,
            self.run_id,
            &self.stage_id,
            self.attempt,
            self.invocation,
            &self.backend_kind,
            &self.spec,
        )?;
        if expected != self.command_fingerprint {
            return Err(ProcessError::InvalidStoredProcess(
                "command fingerprint mismatch",
            ));
        }
        if self.updated_at < self.created_at
            || self.started_at.is_some_and(|at| at < self.created_at)
            || self.finished_at.is_some_and(|at| {
                at < self.created_at || self.started_at.is_some_and(|started| at < started)
            })
        {
            return Err(ProcessError::InvalidStoredProcess(
                "process timestamp regression",
            ));
        }
        let requires_exit = matches!(
            self.status,
            ManagedProcessStatus::Exited | ManagedProcessStatus::Interrupted
        );
        let permits_exit = requires_exit || self.status == ManagedProcessStatus::Cleaned;
        if (requires_exit && (self.exit_result.is_none() || self.finished_at.is_none()))
            || (!permits_exit && (self.exit_result.is_some() || self.finished_at.is_some()))
            || (self.exit_result.is_some() != self.finished_at.is_some())
        {
            return Err(ProcessError::InvalidStoredProcess(
                "terminal status and exit summary disagree",
            ));
        }
        if matches!(
            self.status,
            ManagedProcessStatus::Running | ManagedProcessStatus::Interrupting
        ) && self.started_at.is_none()
        {
            return Err(ProcessError::InvalidStoredProcess(
                "active process has no start time",
            ));
        }
        Ok(())
    }

    pub(crate) fn transition(
        &mut self,
        to: ManagedProcessStatus,
        now: DateTime<Utc>,
        evidence: Option<&ExitEvidence>,
        error: Option<String>,
    ) -> Result<(), ProcessError> {
        use ManagedProcessStatus::{
            Broken, Cleaned, Exited, Interrupted, Interrupting, Missing, Preparing, Running,
            Starting,
        };

        if self.status == to {
            return Ok(());
        }
        let allowed = matches!(
            (self.status, to),
            (Preparing, Starting | Running | Exited | Broken | Cleaned)
                | (Starting | Missing, Running)
                | (Starting | Running | Missing, Exited)
                | (Starting | Running | Interrupting, Missing)
                | (Starting | Running | Interrupting | Missing, Broken)
                | (Running | Missing, Interrupting)
                | (Interrupting | Missing, Interrupted)
                | (Exited | Interrupted | Missing | Broken, Cleaned)
        );
        if !allowed {
            return Err(ProcessError::InvalidTransition {
                from: self.status,
                to,
            });
        }

        if matches!(
            to,
            ManagedProcessStatus::Exited | ManagedProcessStatus::Interrupted
        ) {
            let evidence = evidence.ok_or(ProcessError::InvalidStoredProcess(
                "exit transition requires evidence",
            ))?;
            evidence.validate_for(self)?;
            self.exit_result = Some(evidence.result.clone());
            self.started_at = Some(*evidence.started_at());
            self.finished_at = Some(*evidence.finished_at());
        } else if to == ManagedProcessStatus::Running && self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if to == ManagedProcessStatus::Interrupting {
            self.interrupt_requested = true;
        }
        self.status = to;
        if to != ManagedProcessStatus::Cleaned || error.is_some() {
            self.last_error = error;
        }
        self.updated_at = self
            .finished_at
            .map_or(now.max(self.updated_at), |finished| {
                now.max(self.updated_at).max(finished)
            });
        self.validate()
    }

    pub(crate) fn manifest_json(&self) -> Result<String, ProcessError> {
        Ok(serde_json::to_string(&LaunchManifestV1::from(self))?)
    }

    #[must_use]
    pub const fn id(&self) -> ManagedProcessId {
        self.id
    }

    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    #[must_use]
    pub fn stage_id(&self) -> &StageId {
        &self.stage_id
    }

    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    #[must_use]
    pub const fn invocation(&self) -> u32 {
        self.invocation
    }

    pub(crate) const fn spec_schema_version(&self) -> u32 {
        self.spec_schema_version
    }

    #[must_use]
    pub fn backend_kind(&self) -> &str {
        &self.backend_kind
    }

    #[must_use]
    pub fn backend_session_id(&self) -> &BackendSessionId {
        &self.backend_session_id
    }

    #[must_use]
    pub const fn status(&self) -> ManagedProcessStatus {
        self.status
    }

    #[must_use]
    pub const fn spec(&self) -> &ProcessSpec {
        &self.spec
    }

    #[must_use]
    pub fn command_fingerprint(&self) -> &str {
        &self.command_fingerprint
    }

    #[must_use]
    pub const fn cursor(&self, stream: OutputStream) -> OutputCursor {
        match stream {
            OutputStream::Stdout => self.stdout_cursor,
            OutputStream::Stderr => self.stderr_cursor,
        }
    }

    #[must_use]
    pub const fn exit_result(&self) -> Option<&ExitResult> {
        self.exit_result.as_ref()
    }

    #[must_use]
    pub const fn interrupt_requested(&self) -> bool {
        self.interrupt_requested
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    #[must_use]
    pub const fn revision(&self) -> ProcessRevision {
        self.revision
    }

    #[must_use]
    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }

    #[must_use]
    pub const fn started_at(&self) -> Option<&DateTime<Utc>> {
        self.started_at.as_ref()
    }

    #[must_use]
    pub const fn finished_at(&self) -> Option<&DateTime<Utc>> {
        self.finished_at.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInspection {
    pub process: ManagedProcess,
    pub backend_session: super::BackendSessionState,
    pub stdout_length: u64,
    pub stderr_length: u64,
    pub exit_evidence: Option<ExitEvidence>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LaunchManifestV1 {
    schema_version: u32,
    process_id: ManagedProcessId,
    run_id: RunId,
    stage_id: StageId,
    attempt: u32,
    #[serde(default = "default_invocation")]
    invocation: u32,
    backend_kind: String,
    backend_session_id: BackendSessionId,
    command_fingerprint: String,
    spec: ProcessSpecV1,
}

impl LaunchManifestV1 {
    pub(crate) fn decode(json: &str) -> Result<Self, ProcessError> {
        let manifest: Self = serde_json::from_str(json)?;
        if !matches!(
            manifest.schema_version,
            LEGACY_PROCESS_SPEC_SCHEMA_VERSION | PROCESS_SPEC_SCHEMA_VERSION
        ) {
            return Err(ProcessError::InvalidSpec(
                "unsupported process manifest schema",
            ));
        }
        Ok(manifest)
    }

    const fn invocation(&self) -> u32 {
        self.invocation
    }

    pub(crate) fn process_id(&self) -> ManagedProcessId {
        self.process_id
    }

    pub(crate) fn command_fingerprint(&self) -> &str {
        &self.command_fingerprint
    }

    #[cfg(test)]
    pub(crate) fn spec(&self) -> Result<ProcessSpec, ProcessError> {
        self.spec.decode()
    }

    pub(crate) fn validated_spec(&self) -> Result<ProcessSpec, ProcessError> {
        let spec = self.spec.decode()?;
        let expected = fingerprint(
            self.schema_version,
            self.run_id,
            &self.stage_id,
            self.attempt,
            self.invocation,
            &self.backend_kind,
            &spec,
        )?;
        if expected != self.command_fingerprint {
            return Err(ProcessError::InvalidSpec("command fingerprint mismatch"));
        }
        Ok(spec)
    }
}

impl From<&ManagedProcess> for LaunchManifestV1 {
    fn from(process: &ManagedProcess) -> Self {
        Self {
            schema_version: PROCESS_SPEC_SCHEMA_VERSION,
            process_id: process.id,
            run_id: process.run_id,
            stage_id: process.stage_id.clone(),
            attempt: process.attempt,
            invocation: process.invocation,
            backend_kind: process.backend_kind.clone(),
            backend_session_id: process.backend_session_id.clone(),
            command_fingerprint: process.command_fingerprint.clone(),
            spec: ProcessSpecV1::from(&process.spec),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ProcessSpecV1 {
    executable: EncodedOsValue,
    argv: Vec<EncodedOsValue>,
    working_directory: EncodedOsValue,
    environment: Vec<EnvironmentEntryV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdin_path: Option<EncodedOsValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdin_sha256: Option<String>,
    stdout_path: EncodedOsValue,
    stderr_path: EncodedOsValue,
}

impl ProcessSpecV1 {
    fn decode(&self) -> Result<ProcessSpec, ProcessError> {
        let mut environment = BTreeMap::new();
        for entry in &self.environment {
            let key = entry.key.decode()?;
            if environment.insert(key, entry.value.decode()?).is_some() {
                return Err(ProcessError::InvalidSpec("duplicate environment override"));
            }
        }
        let spec = ProcessSpec::new(
            PathBuf::from(self.executable.decode()?),
            self.argv
                .iter()
                .map(EncodedOsValue::decode)
                .collect::<Result<Vec<_>, _>>()?,
            PathBuf::from(self.working_directory.decode()?),
            environment,
            PathBuf::from(self.stdout_path.decode()?),
            PathBuf::from(self.stderr_path.decode()?),
        )?;
        match (&self.stdin_path, &self.stdin_sha256) {
            (Some(path), Some(hash)) => {
                spec.with_stdin(PathBuf::from(path.decode()?), hash.clone())
            }
            (None, None) => Ok(spec),
            _ => Err(ProcessError::InvalidSpec("invalid stdin binding")),
        }
    }
}

impl From<&ProcessSpec> for ProcessSpecV1 {
    fn from(spec: &ProcessSpec) -> Self {
        Self {
            executable: EncodedOsValue::from(spec.executable.as_os_str()),
            argv: spec
                .argv
                .iter()
                .map(|value| EncodedOsValue::from(value.as_os_str()))
                .collect(),
            working_directory: EncodedOsValue::from(spec.working_directory.as_os_str()),
            environment: spec
                .environment
                .iter()
                .map(|(key, value)| EnvironmentEntryV1 {
                    key: EncodedOsValue::from(key.as_os_str()),
                    value: EncodedOsValue::from(value.as_os_str()),
                })
                .collect(),
            stdin_path: spec
                .stdin_path
                .as_ref()
                .map(|path| EncodedOsValue::from(path.as_os_str())),
            stdin_sha256: spec.stdin_sha256.clone(),
            stdout_path: EncodedOsValue::from(spec.stdout_path.as_os_str()),
            stderr_path: EncodedOsValue::from(spec.stderr_path.as_os_str()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct EnvironmentEntryV1 {
    key: EncodedOsValue,
    value: EncodedOsValue,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "encoding", content = "value", rename_all = "snake_case")]
enum EncodedOsValue {
    Utf8(String),
    UnixHex(String),
}

impl EncodedOsValue {
    fn decode(&self) -> Result<OsString, ProcessError> {
        match self {
            Self::Utf8(value) => Ok(OsString::from(value)),
            Self::UnixHex(value) => decode_unix_hex(value),
        }
    }
}

impl From<&OsStr> for EncodedOsValue {
    fn from(value: &OsStr) -> Self {
        value.to_str().map_or_else(
            || Self::UnixHex(encode_hex(&os_bytes_infallible(value))),
            |value| Self::Utf8(value.to_owned()),
        )
    }
}

fn fingerprint(
    schema_version: u32,
    run_id: RunId,
    stage_id: &StageId,
    attempt: u32,
    invocation: u32,
    backend_kind: &str,
    spec: &ProcessSpec,
) -> Result<String, ProcessError> {
    let mut hash = Sha256::new();
    match schema_version {
        LEGACY_PROCESS_SPEC_SCHEMA_VERSION => hash.update(b"senate-managed-process/v1\0"),
        PROCESS_SPEC_SCHEMA_VERSION => hash.update(b"senate-managed-process/v2\0"),
        _ => {
            return Err(ProcessError::InvalidSpec(
                "unsupported process manifest schema",
            ));
        }
    }
    hash_part(&mut hash, run_id.to_string().as_bytes())?;
    hash_part(&mut hash, stage_id.as_str().as_bytes())?;
    hash_part(&mut hash, &attempt.to_be_bytes())?;
    if schema_version >= PROCESS_SPEC_SCHEMA_VERSION {
        hash_part(&mut hash, &invocation.to_be_bytes())?;
    }
    hash_part(&mut hash, backend_kind.as_bytes())?;
    hash_os(&mut hash, spec.executable.as_os_str())?;
    for argument in &spec.argv {
        hash_os(&mut hash, argument)?;
    }
    hash.update([0xff]);
    hash_os(&mut hash, spec.working_directory.as_os_str())?;
    for (key, value) in &spec.environment {
        hash_os(&mut hash, key)?;
        hash_os(&mut hash, value)?;
    }
    hash.update([0xfe]);
    if schema_version >= PROCESS_SPEC_SCHEMA_VERSION {
        match (&spec.stdin_path, &spec.stdin_sha256) {
            (Some(path), Some(digest)) => {
                hash.update([1]);
                hash_os(&mut hash, path.as_os_str())?;
                hash_part(&mut hash, digest.as_bytes())?;
            }
            (None, None) => hash.update([0]),
            _ => return Err(ProcessError::InvalidSpec("invalid stdin binding")),
        }
    }
    hash_os(&mut hash, spec.stdout_path.as_os_str())?;
    hash_os(&mut hash, spec.stderr_path.as_os_str())?;
    let digest = hash.finalize();
    Ok(encode_hex(digest.as_ref()))
}

const fn default_invocation() -> u32 {
    1
}

fn hash_os(hash: &mut Sha256, value: &OsStr) -> Result<(), ProcessError> {
    hash_part(hash, os_bytes(value)?)
}

fn hash_part(hash: &mut Sha256, value: &[u8]) -> Result<(), ProcessError> {
    let length = u64::try_from(value.len())
        .map_err(|_| ProcessError::InvalidSpec("fingerprint input is too large"))?;
    hash.update(length.to_be_bytes());
    hash.update(value);
    Ok(())
}

#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "shared call sites return UnsupportedPlatform on non-Unix"
)]
fn os_bytes(value: &OsStr) -> Result<&[u8], ProcessError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(value.as_bytes())
}

#[cfg(not(unix))]
fn os_bytes(_value: &OsStr) -> Result<&[u8], ProcessError> {
    Err(ProcessError::UnsupportedPlatform)
}

#[cfg(unix)]
fn os_bytes_infallible(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes_infallible(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(unix)]
fn decode_unix_hex(value: &str) -> Result<OsString, ProcessError> {
    use std::os::unix::ffi::OsStringExt;
    if !value.len().is_multiple_of(2) {
        return Err(ProcessError::InvalidSpec("invalid Unix byte encoding"));
    }
    let bytes = value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[high, low]| {
            let high = hex_digit(high)?;
            let low = hex_digit(low)?;
            Ok((high << 4) | low)
        })
        .collect::<Result<Vec<_>, ProcessError>>()?;
    Ok(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn decode_unix_hex(_value: &str) -> Result<OsString, ProcessError> {
    Err(ProcessError::UnsupportedPlatform)
}

fn hex_digit(value: u8) -> Result<u8, ProcessError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(ProcessError::InvalidSpec("invalid Unix byte encoding")),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use chrono::TimeZone;

    use super::*;

    fn process() -> ManagedProcess {
        let root = std::env::temp_dir().join("senate process model");
        let spec = ProcessSpec::new(
            PathBuf::from("/usr/bin/printf"),
            vec![OsString::from("%s"), OsString::from("hello; touch /tmp/no")],
            &root,
            BTreeMap::from([(OsString::from("MODE"), OsString::from("test"))]),
            root.join("stdout.log"),
            root.join("stderr.log"),
        )
        .unwrap();
        ManagedProcess::preparing(
            ManagedProcessId::from_u128(1),
            RunId::from_u128(2),
            StageId::new("implementation").unwrap(),
            0,
            1,
            "tmux".to_owned(),
            BackendSessionId::for_process(ManagedProcessId::from_u128(1)),
            spec,
            Utc.with_ymd_and_hms(2026, 8, 14, 10, 0, 0)
                .single()
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn manifest_round_trip_preserves_exact_command_without_debugging_arguments() {
        let process = process();
        let json = process.manifest_json().unwrap();
        let restored = ManagedProcess::from_stored(
            process.id(),
            process.run_id(),
            process.stage_id().clone(),
            process.attempt(),
            process.invocation(),
            process.backend_kind().to_owned(),
            process.backend_session_id().clone(),
            process.status(),
            process.spec_schema_version(),
            &json,
            process.command_fingerprint().to_owned(),
            process.cursor(OutputStream::Stdout),
            process.cursor(OutputStream::Stderr),
            None,
            false,
            None,
            process.revision(),
            *process.created_at(),
            *process.updated_at(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(restored, process);
        assert!(!format!("{:?}", process.spec()).contains("touch /tmp/no"));
    }

    #[cfg(unix)]
    #[test]
    fn manifest_round_trip_preserves_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;

        let mut process = process();
        process
            .spec
            .argv
            .push(OsString::from_vec(vec![b'a', 0xff, b'b']));
        process.command_fingerprint = fingerprint(
            process.spec_schema_version,
            process.run_id,
            &process.stage_id,
            process.attempt,
            process.invocation,
            &process.backend_kind,
            &process.spec,
        )
        .unwrap();
        let manifest = LaunchManifestV1::decode(&process.manifest_json().unwrap()).unwrap();
        assert_eq!(manifest.spec().unwrap(), process.spec);
    }
}
