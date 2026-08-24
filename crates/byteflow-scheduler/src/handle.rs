use crate::oneshot;
use crate::process::{ProcessId, ProcessOutcome};

/// A reference to a spawned process, returned by
/// [`crate::runtime::Runtime::spawn`].
///
/// Holding a `ProcessHandle` does not keep the process alive or pin it to
/// any worker — it is purely a way to (a) read off its [`ProcessId`] for
/// addressing it with `Send`, and (b) block the *calling native thread*
/// until it finishes, via [`ProcessHandle::join`]. Dropping a handle
/// without joining is fine; the process runs to completion regardless
/// (fire-and-forget is the common case for actor-style workers).
pub struct ProcessHandle {
    pub(crate) id: ProcessId,
    pub(crate) receiver: oneshot::Receiver<ProcessOutcome>,
}

impl ProcessHandle {
    pub fn id(&self) -> ProcessId {
        self.id
    }

    /// Block the current (native) thread until the process terminates.
    /// **Never call this from inside a worker thread / from bytecode** —
    /// see [`crate::oneshot::Receiver::join`]'s warning. It is meant for an
    /// embedder's `main` waiting on a top-level computation, mirroring
    /// `std::thread::JoinHandle::join`.
    pub fn join(self) -> ProcessOutcome {
        self.receiver.join()
    }
}
