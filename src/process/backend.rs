use std::path::Path;

use super::{
    BackendSessionId, ExitEvidence, ManagedProcess, ManagedProcessId, OutputChunk, OutputStream,
    ProcessError,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendAvailability {
    pub kind: &'static str,
    pub version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendSessionState {
    Absent,
    Owned,
}

/// One rung of the termination ladder, in the order a stop climbs it.
///
/// Interrupt alone is not a stop. An agent CLI forwards Ctrl-C to whatever it
/// launched, and a build tool that spawns a worker pool — Jest, Vitest, Cargo —
/// routinely leaves that pool running: the workers either ignore SIGINT or take
/// longer to honour it than a caller can be asked to wait. A process only ever
/// asked politely is a process that outlives its run, holding its share of the
/// machine until someone notices, so the ask escalates until the operating
/// system stops asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TerminationSignal {
    /// What Ctrl-C sends. The only rung that lets an agent shut itself down
    /// cleanly and write its own exit evidence.
    Interrupt,
    /// The conventional "stop now" that a well-behaved child still handles.
    Terminate,
    /// Uncatchable, and so the rung that is guaranteed to end the process.
    Kill,
}

impl TerminationSignal {
    /// The ladder, gentlest first. A stop climbs it in this order.
    pub const LADDER: [Self; 3] = [Self::Interrupt, Self::Terminate, Self::Kill];

    /// The `kill(1)` flag naming this signal.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Interrupt => "-INT",
            Self::Terminate => "-TERM",
            Self::Kill => "-KILL",
        }
    }

    /// Whether surviving this rung leaves nothing further to escalate to.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Kill)
    }
}

/// Synchronous process-supervisor boundary used below future provider adapters.
pub trait ProcessBackend {
    fn kind(&self) -> &'static str;

    fn session_id(&self, process_id: ManagedProcessId) -> BackendSessionId;

    /// Checks backend executable availability without creating a session.
    ///
    /// # Errors
    /// Returns typed missing-backend or command failures.
    fn availability(&self) -> Result<BackendAvailability, ProcessError>;

    /// Starts hidden runner from immutable manifest using backend-native ownership.
    ///
    /// # Errors
    /// Rejects foreign collisions and backend command failures.
    fn start(&self, process: &ManagedProcess, manifest: &Path) -> Result<(), ProcessError>;

    /// Inspects only concrete supervisor evidence, validating ownership.
    ///
    /// # Errors
    /// Rejects foreign sessions and backend command failures.
    fn inspect_session(
        &self,
        process: &ManagedProcess,
    ) -> Result<BackendSessionState, ProcessError>;

    /// Reads raw bytes at explicit offset without advancing durable cursor.
    ///
    /// # Errors
    /// Rejects truncated files and I/O failures.
    fn read_output(
        &self,
        process: &ManagedProcess,
        stream: OutputStream,
        offset: u64,
        max_bytes: usize,
    ) -> Result<OutputChunk, ProcessError>;

    /// Returns current byte length for one retained output stream.
    ///
    /// # Errors
    /// Returns I/O failures.
    fn output_length(
        &self,
        process: &ManagedProcess,
        stream: OutputStream,
    ) -> Result<u64, ProcessError>;

    /// Reads and validates atomic runner exit evidence.
    ///
    /// # Errors
    /// Rejects corrupt or foreign evidence.
    fn read_exit_evidence(
        &self,
        process: &ManagedProcess,
    ) -> Result<Option<ExitEvidence>, ProcessError>;

    /// Sends one signal to the process group, validating session ownership.
    ///
    /// A group that has already gone is not a failure. The signal asked for
    /// something that has already happened, and reporting that as an error
    /// would turn a won race into a stop that never completes.
    ///
    /// # Errors
    /// Rejects foreign sessions and backend command failures.
    fn signal(
        &self,
        process: &ManagedProcess,
        signal: TerminationSignal,
    ) -> Result<(), ProcessError>;

    /// Removes only proven-owned supervisor resources. Output files stay retained.
    ///
    /// # Errors
    /// Rejects foreign sessions and backend command failures.
    fn cleanup(&self, process: &ManagedProcess) -> Result<(), ProcessError>;
}
