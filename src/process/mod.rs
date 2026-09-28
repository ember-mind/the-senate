//! Crash-reconcilable external-process infrastructure below provider adapters.

mod backend;
mod environment;
mod error;
mod ids;
mod manager;
mod model;
mod runner;
mod tmux;

pub use backend::{BackendAvailability, BackendSessionState, ProcessBackend, TerminationSignal};
pub use error::ProcessError;
pub use ids::{BackendSessionId, ManagedProcessId};
pub use manager::ProcessManager;
pub use model::{
    ExitEvidence, ExitResult, ManagedProcess, ManagedProcessStatus, OutputChunk, OutputCursor,
    OutputStream, ProcessInspection, ProcessRevision, ProcessSpec,
};
pub use runner::{exec_managed_process, run_managed_process};
pub use tmux::TmuxBackend;
