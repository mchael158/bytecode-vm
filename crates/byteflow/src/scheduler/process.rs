use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::vm::Vm;

use super::mailbox::Mailbox;
use super::oneshot;

/// Identifier of a **flow** — Byteflow's unit of concurrent work.
///
/// Host APIs and the directory key on this type. Inside messages it appears
/// as [`crate::Value::Pid`] (`Message.sender` / `msg_sender`) for **identity**.
/// Bytecode addressing uses [`crate::Value::Cap`] (FlowCap) — a Pid is not a
/// Send/Ask authority token.
///
/// Backed by a single global, wait-free `AtomicU64` counter rather than
/// anything derived from memory addresses: ids must stay unique for the
/// lifetime of the runtime and must **never** be reused, or a stale id in
/// someone's registers could address a *different* later flow (ABA) via the
/// host/`Directory` path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FlowId(pub(crate) u64);

impl FlowId {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for FlowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "flow#{}", self.0)
    }
}

static NEXT_FLOW_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_flow_id() -> FlowId {
    FlowId(NEXT_FLOW_ID.fetch_add(1, Ordering::Relaxed))
}

/// Restart policy consulted by a [`super::supervisor::Supervisor`] when a
/// supervised flow terminates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

/// Live counters for one flow (updated only by the worker currently
/// running it).
#[derive(Debug, Default)]
pub struct FlowMetrics {
    pub instructions: AtomicU64,
    /// Atomic hops sent (`Send` of [`crate::Value::Message`]).
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub reschedules: AtomicU64,
}

/// A single **flow**: VM state, mailbox, and bookkeeping.
///
/// This is the unit of work moved by the scheduler — pushed onto worker
/// deques, stolen, parked inside a [`Mailbox`] on `Receive`, or held by
/// the timer wheel while sleeping. Flows talk only via **Atomic Hops**
/// ([`crate::Value::Message`] on `Send`).
pub struct Flow {
    pub id: FlowId,
    pub vm: Vm,
    pub mailbox: Arc<Mailbox>,
    pub metrics: Arc<FlowMetrics>,
    pub restart_policy: RestartPolicy,
    /// Completion channel consumed by [`super::handle::FlowHandle::join`].
    pub(crate) completion: oneshot::Sender<FlowOutcome>,
    /// Set by [`Mailbox::park`] when a hop wins the park race.
    pub pending_message: Option<crate::bytecode::Value>,
    /// Destination register of the most recent `Receive` / `ReceiveTimeout`.
    pub last_receive_dest: Option<u8>,
    /// Continuation after a `WAITING_SEND` park is admitted.
    pub(crate) pending_send: Option<PendingSend>,
    pub(crate) supervisor: Option<super::supervisor::SupervisorLink>,
}

/// What the worker should do after a parked sender's hop is admitted.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PendingSend {
    FireAndForget,
    Ask {
        dest_reg: u8,
        expect_request_id: u64,
        expect_sender: u64,
        timeout: Option<Duration>,
    },
}

/// Terminal outcome of a flow, delivered to whoever holds its
/// [`super::handle::FlowHandle`].
#[derive(Clone, Debug, PartialEq)]
pub enum FlowOutcome {
    Completed(crate::bytecode::Value),
    Failed(String),
}

impl Flow {
    pub fn new(
        id: FlowId,
        vm: Vm,
        mailbox: Arc<Mailbox>,
        restart_policy: RestartPolicy,
        completion: oneshot::Sender<FlowOutcome>,
    ) -> Self {
        Self {
            id,
            vm,
            mailbox,
            metrics: Arc::new(FlowMetrics::default()),
            restart_policy,
            completion,
            pending_message: None,
            last_receive_dest: None,
            pending_send: None,
            supervisor: None,
        }
    }

    pub(crate) fn complete(self, outcome: FlowOutcome) {
        self.completion.send(outcome);
    }
}
