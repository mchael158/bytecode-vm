use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use byteflow_vm::Vm;

use crate::mailbox::Mailbox;
use crate::oneshot;

/// A process identifier.
///
/// Backed by a single global, wait-free `AtomicU64` counter
/// (`fetch_add(1, Relaxed)`) rather than anything derived from memory
/// addresses or slot indices: identifiers must stay valid and unique for
/// the lifetime of the whole runtime (a `Send` can be issued long after the
/// sender last held a reference to the target), and must never be reused,
/// or a stale `Pid` in someone's registers could end up addressing a
/// *different*, later process — a classic ABA bug in actor systems.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId(pub(crate) u64);

impl ProcessId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid#{}", self.0)
    }
}

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

pub fn next_process_id() -> ProcessId {
    ProcessId(NEXT_PID.fetch_add(1, Ordering::Relaxed))
}

/// Where a process currently sits in its lifecycle (design notes §4).
///
/// This is metrics/introspection state (what `byteflow debug` or
/// `runtime.metrics()` would report, design notes §26-27) — the scheduler's
/// actual control flow is driven by *where the `Process` object physically
/// lives* (a worker's local deque, the global injector, the timer wheel, or
/// parked inside its own mailbox), not by this enum. Keeping the two in
/// sync is the worker loop's job (`byteflow-scheduler::worker`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState {
    Ready,
    Running,
    Waiting,
    Sleeping,
    Terminated,
    Failed,
}

/// Restart policy consulted by a [`crate::supervisor::Supervisor`] when one
/// of its children terminates abnormally (design notes §15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

/// Live counters for one process, updated only by the worker thread
/// currently executing it (no contention: a process runs on one worker at a
/// time by construction) and read by anyone holding a clone of the `Arc`
/// for `runtime.metrics()` / a debugger attach (design notes §26-27).
#[derive(Debug, Default)]
pub struct ProcessMetrics {
    pub instructions: AtomicU64,
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub reschedules: AtomicU64,
}

/// A single virtual process: its interpreter state, mailbox, and
/// bookkeeping. This is the unit of work moved around by the scheduler —
/// pushed onto worker-local deques, stolen, parked inside a `Mailbox`, or
/// held by the timer wheel while sleeping.
pub struct Process {
    pub id: ProcessId,
    pub vm: Vm,
    pub mailbox: Arc<Mailbox>,
    pub metrics: Arc<ProcessMetrics>,
    pub restart_policy: RestartPolicy,
    /// Completion channel consumed by `ProcessHandle::join`.
    pub(crate) completion: oneshot::Sender<ProcessOutcome>,
    /// Set by [`crate::mailbox::Mailbox::park`] when a message wins a race
    /// against this process trying to park on `Receive` — see that
    /// function's doc comment for the race it closes. The worker loop
    /// checks this immediately after a failed `park` instead of looping
    /// back into the mailbox.
    pub pending_message: Option<byteflow_bytecode::Value>,
    /// The destination register of the most recent `Receive`/
    /// `ReceiveTimeout` this process issued. Recorded the moment we decide
    /// to park (see `worker::park_on_mailbox`) because by the time a
    /// `Send` or timeout hands the value back, the original [`VmResult`]
    /// that carried this register is long gone — the process object itself
    /// is the only place left to remember it.
    pub last_receive_dest: Option<u8>,
    /// Set when this process was started by a [`crate::supervisor::Supervisor`].
    /// The worker delivers the terminal outcome here so the supervisor can
    /// apply [`RestartPolicy`] without joining on a worker thread.
    pub(crate) supervisor: Option<crate::supervisor::SupervisorLink>,
}

/// Terminal outcome of a process, delivered to whoever holds its
/// [`crate::handle::ProcessHandle`].
#[derive(Clone, Debug)]
pub enum ProcessOutcome {
    Completed(byteflow_bytecode::Value),
    Failed(String),
}

impl Process {
    pub fn new(
        id: ProcessId,
        vm: Vm,
        mailbox: Arc<Mailbox>,
        restart_policy: RestartPolicy,
        completion: oneshot::Sender<ProcessOutcome>,
    ) -> Self {
        Process {
            id,
            vm,
            mailbox,
            metrics: Arc::new(ProcessMetrics::default()),
            restart_policy,
            completion,
            pending_message: None,
            last_receive_dest: None,
            supervisor: None,
        }
    }

    pub(crate) fn complete(self, outcome: ProcessOutcome) {
        self.completion.send(outcome);
    }
}

// Timer-wheel entry types live in `crate::timer` — they need to reference
// both `Process` (for plain `Sleep`) and `Mailbox` (for `ReceiveTimeout`),
// so they're defined next to the wheel itself rather than here.
