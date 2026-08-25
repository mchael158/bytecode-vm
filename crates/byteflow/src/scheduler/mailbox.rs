use std::collections::VecDeque;
use std::sync::Mutex;

use crate::bytecode::Value;

use super::error::RuntimeError;
use super::process::Process;
use super::sync_lock;

/// A process's inbox, plus (when the owning process is blocked on
/// `Receive` with nothing to read) the parked process itself.
///
/// # Why the process lives *inside* its own mailbox while waiting
///
/// A process is only reachable by other processes through its `Pid`, which
/// resolves (via [`super::directory::Directory`]) to this `Mailbox`. So the
/// mailbox is the one object that is *always* addressable, whether the
/// process is currently running on a worker, sitting in a run queue, or
/// blocked. Storing the blocked `Box<Process>` directly in
/// `MailboxInner::parked`, behind the same mutex that guards the message
/// queue, turns "deliver a message and wake the receiver if it was
/// waiting" into a single critical section — which is what actually
/// prevents the classic lost-wakeup race:
///
/// ```text
/// racing without a shared lock:
///   receiver: queue.pop() -> None
///   sender:   queue.push(msg); wake(receiver)   // receiver isn't parked yet!
///   receiver: park()                            // ...and now sleeps forever
///
/// with both steps under one mutex (what this type does):
///   receiver: lock; queue.pop() -> None; store self in `parked`; unlock
///   sender:   lock; parked.take() -> Some(receiver); unlock; wake(receiver)
/// ```
/// Because "check the queue" and "become parked" happen atomically with
/// respect to "push and check for a parked receiver", there is no window
/// where a message can be pushed without either landing in the queue for a
/// later `Receive` or immediately waking an already-parked one.
pub struct Mailbox {
    inner: Mutex<MailboxInner>,
}

#[derive(Default)]
struct MailboxInner {
    queue: VecDeque<Value>,
    parked: Option<Box<Process>>,
}

/// Outcome of pushing a message: either it was queued for later, or it
/// immediately handed off to a process that was parked waiting for it — in
/// which case the caller (see `worker::deliver`) is responsible for feeding
/// the value back into that process's VM and re-enqueuing it as `Ready`.
pub enum Delivery {
    Queued,
    Handoff(Box<Process>),
}

impl Mailbox {
    pub fn new() -> Self {
        Mailbox {
            inner: Mutex::new(MailboxInner::default()),
        }
    }

    /// Push `value`. If a process is currently parked on this mailbox
    /// (blocked in `Receive`), it is atomically removed and returned via
    /// [`Delivery::Handoff`] instead of the message being queued — the
    /// caller must resume that process with `value`, not read it back out
    /// of the mailbox.
    ///
    /// Mutex poison → [`RuntimeError`] (fail-closed; do not continue on
    /// inconsistent shared state).
    pub fn push(&self, value: Value) -> Result<Delivery, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::push")?;
        Ok(match inner.parked.take() {
            Some(process) => Delivery::Handoff(process),
            None => {
                inner.queue.push_back(value);
                Delivery::Queued
            }
        })
    }

    /// Non-blocking pop, used by a worker that is *currently running* this
    /// mailbox's owning process (i.e. is about to decide whether it can
    /// satisfy a `Receive` immediately or must park).
    pub fn try_pop(&self) -> Result<Option<Value>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::try_pop")?;
        Ok(inner.queue.pop_front())
    }

    /// Atomically re-check the queue and, if still empty, store `process`
    /// as parked.
    ///
    /// Outer `Result` is infrastructure (mutex poison). Inner `Result` is
    /// the lost-wakeup race: `Err(process)` means a message arrived between
    /// the worker's `try_pop` and this call — resume immediately with the
    /// stashed pending message rather than parking forever.
    pub fn park(&self, process: Box<Process>) -> Result<Result<(), Box<Process>>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::park")?;
        if let Some(value) = inner.queue.pop_front() {
            drop(inner);
            // A message beat us here; hand it straight back via a
            // one-off out-of-band so the caller can resume with it.
            return Ok(Err(with_pending(process, value)));
        }
        inner.parked = Some(process);
        Ok(Ok(()))
    }

    /// Attempt to take a timed-out parked process back out, used by the
    /// timer wheel when a `ReceiveTimeout` deadline fires. Returns `None`
    /// if the process was already woken by a `Send` in the meantime.
    pub fn take_parked(&self) -> Result<Option<Box<Process>>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::take_parked")?;
        Ok(inner.parked.take())
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Stashes a message that arrived just as we were about to park, so the
/// worker loop can resume the process with it on the very next step
/// without re-entering the mailbox. See [`Mailbox::park`].
fn with_pending(mut process: Box<Process>, value: Value) -> Box<Process> {
    process.pending_message = Some(value);
    process
}
