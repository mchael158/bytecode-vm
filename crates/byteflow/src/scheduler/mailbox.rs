use std::collections::VecDeque;
use std::sync::Mutex;

use crate::bytecode::Value;

use super::error::RuntimeError;
use super::process::Flow;
use super::sync_lock;

/// Selective wait criterion installed while a flow is parked in its mailbox.
///
/// Used by classic `Receive`, `ReceiveMatch`, and `Ask`. Matching always
/// **skips** (never drops) non-matching hops already in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitFilter {
    /// `Receive`: consume the oldest value regardless of contents.
    Any,
    /// `ReceiveMatch`: oldest `Message` whose application `tag` matches.
    Tag(u16),
    /// `Ask`: reply belonging to one specific request.
    ///
    /// `expect_request_id` is the RPC correlation key.
    /// `expect_sender`, when present, additionally constrains reply origin.
    /// Ask uses `Some(resolved_target_FlowId)` so a hop with the same
    /// `request_id` from an unrelated flow cannot complete the RPC.
    /// Compare against FlowId, never CapId (S2 after FlowCap).
    Correlation {
        expect_request_id: u64,
        expect_sender: Option<u64>,
    },
}

impl WaitFilter {
    #[inline]
    pub(crate) fn matches(&self, value: &Value) -> bool {
        match *self {
            Self::Any => true,
            Self::Tag(expected_tag) => value
                .as_message()
                .map(|m| m.tag == expected_tag)
                .unwrap_or(false),
            Self::Correlation {
                expect_request_id,
                expect_sender,
            } => match value.as_message() {
                Some(m) => {
                    m.request_id == expect_request_id
                        && expect_sender.map(|s| m.sender == s).unwrap_or(true)
                }
                None => false,
            },
        }
    }
}

/// A flow's inbox, plus (when the owning flow is blocked on
/// `Receive` / `ReceiveMatch` / `Ask` with nothing to read) the parked flow itself.
///
/// # Why the flow lives *inside* its own mailbox while waiting
///
/// Internally, a flow is reachable through its [`super::process::FlowId`], which
/// resolves (via [`super::directory::Directory`]) to this `Mailbox`. Bytecode
/// does **not** address by Pid anymore (FlowCap): `Send` / `Ask` resolve a
/// Cap to a FlowId first, then look up here. Host [`crate::Runtime::send`]
/// still uses FlowId directly (trusted).
///
/// Storing the blocked `Box<Flow>` directly in `MailboxInner::parked`, behind
/// the same mutex that guards the message queue, turns "deliver a message and
/// wake the receiver if it was waiting" into a single critical section — which
/// is what actually prevents the classic lost-wakeup race:
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
///
/// # Selective wait (`ReceiveMatch` / `Ask`)
///
/// When parked with a non-[`WaitFilter::Any`] filter, only a hop that
/// satisfies the filter wakes the flow. Other hops are appended to the
/// queue and the waiter stays parked (FIFO skip, never drop).
pub struct Mailbox {
    inner: Mutex<MailboxInner>,
}

struct MailboxInner {
    queue: VecDeque<Value>,
    parked: Option<Box<Flow>>,
    /// Active while `parked` is `Some`. Ignored when nobody is waiting.
    parked_filter: WaitFilter,
}

impl Default for MailboxInner {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            parked: None,
            parked_filter: WaitFilter::Any,
        }
    }
}

/// Outcome of pushing a message: either it was queued for later, or it
/// immediately handed off to a flow that was parked waiting for it — in
/// which case the caller (see `worker::deliver`) is responsible for feeding
/// the value back into that flow's VM and re-enqueuing it as `Ready`.
pub enum Delivery {
    Queued,
    Handoff(Box<Flow>),
}

impl Mailbox {
    pub fn new() -> Self {
        Mailbox {
            inner: Mutex::new(MailboxInner::default()),
        }
    }

    /// Push `value`. If a flow is currently parked on this mailbox
    /// and the hop satisfies its wait filter, it is atomically removed and
    /// returned via [`Delivery::Handoff`]. Otherwise the hop is queued
    /// (and a selective waiter stays parked).
    ///
    /// Mutex poison → [`RuntimeError`] (fail-closed; do not continue on
    /// inconsistent shared state).
    pub fn push(&self, value: Value) -> Result<Delivery, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::push")?;
        if let Some(flow) = inner.parked.take() {
            if inner.parked_filter.matches(&value) {
                inner.parked_filter = WaitFilter::Any;
                return Ok(Delivery::Handoff(flow));
            }
            // Selective wait: keep parked, queue the non-matching hop.
            inner.parked = Some(flow);
            inner.queue.push_back(value);
            return Ok(Delivery::Queued);
        }
        inner.queue.push_back(value);
        Ok(Delivery::Queued)
    }

    /// Non-blocking pop of the front hop (classic `Receive`).
    pub fn try_pop(&self) -> Result<Option<Value>, RuntimeError> {
        self.try_pop_filter(WaitFilter::Any)
    }

    /// Non-blocking selective pop by application `tag` (`ReceiveMatch`).
    pub fn try_pop_match(&self, tag: u16) -> Result<Option<Value>, RuntimeError> {
        self.try_pop_filter(WaitFilter::Tag(tag))
    }

    /// Non-blocking pop under an arbitrary [`WaitFilter`] (FIFO skip).
    pub(crate) fn try_pop_filter(
        &self,
        filter: WaitFilter,
    ) -> Result<Option<Value>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::try_pop_filter")?;
        Ok(take_with_filter(&mut inner.queue, filter))
    }

    /// Atomically re-check the queue and, if still empty, store `flow`
    /// as parked (classic `Receive` — any hop wakes).
    ///
    /// Outer `Result` is infrastructure (mutex poison). Inner `Result` is
    /// the lost-wakeup race: `Err(flow)` means a message arrived between
    /// the worker's `try_pop` and this call — resume immediately with the
    /// stashed pending message rather than parking forever.
    pub fn park(&self, flow: Box<Flow>) -> Result<Result<(), Box<Flow>>, RuntimeError> {
        self.park_filter(flow, WaitFilter::Any)
    }

    /// Like [`park`](Self::park), but only a hop with `Message.tag == tag`
    /// ends the wait. Non-matching hops already in the queue are left
    /// untouched (FIFO skip).
    pub fn park_match(
        &self,
        flow: Box<Flow>,
        tag: u16,
    ) -> Result<Result<(), Box<Flow>>, RuntimeError> {
        self.park_filter(flow, WaitFilter::Tag(tag))
    }

    /// Park under an arbitrary filter. Re-checks the queue under the same
    /// mutex before installing the waiter (anti lost-wakeup).
    pub(crate) fn park_filter(
        &self,
        flow: Box<Flow>,
        filter: WaitFilter,
    ) -> Result<Result<(), Box<Flow>>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::park_filter")?;
        if let Some(value) = take_with_filter(&mut inner.queue, filter) {
            drop(inner);
            return Ok(Err(with_pending(flow, value)));
        }
        inner.parked_filter = filter;
        inner.parked = Some(flow);
        Ok(Ok(()))
    }

    /// Attempt to take a timed-out parked flow back out, used by the
    /// timer wheel when a `ReceiveTimeout` deadline fires. Returns `None`
    /// if the flow was already woken by a `Send` in the meantime.
    pub fn take_parked(&self) -> Result<Option<Box<Flow>>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::take_parked")?;
        inner.parked_filter = WaitFilter::Any;
        Ok(inner.parked.take())
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Remove one matching hop from `queue`, preserving relative order of
/// everything else (FIFO skip — never drop non-matching entries).
fn take_with_filter(queue: &mut VecDeque<Value>, filter: WaitFilter) -> Option<Value> {
    match filter {
        WaitFilter::Any => queue.pop_front(),
        other => {
            let idx = queue.iter().position(|v| other.matches(v))?;
            queue.remove(idx)
        }
    }
}

/// Stashes a message that arrived just as we were about to park, so the
/// worker loop can resume the flow with it on the very next step
/// without re-entering the mailbox. See [`Mailbox::park`].
fn with_pending(mut flow: Box<Flow>, value: Value) -> Box<Flow> {
    flow.pending_message = Some(value);
    flow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Message;
    use crate::scheduler::oneshot;
    use crate::scheduler::process::{next_flow_id, RestartPolicy};
    use crate::vm::{NativeTable, Vm};
    use crate::ChunkBuilder;
    use std::sync::Arc;

    fn dummy_flow() -> Box<Flow> {
        let mut b = ChunkBuilder::new("mb");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let chunk = b.finish();
        let vm = Vm::new(Arc::new(chunk), NativeTable::empty(), 0, &[]).expect("vm");
        let (tx, _rx) = oneshot::channel();
        Box::new(Flow::new(
            next_flow_id(),
            vm,
            Arc::new(Mailbox::new()),
            RestartPolicy::Never,
            tx,
        ))
    }

    fn hop(sender: u64, request_id: u64, tag: u16, payload: u64) -> Value {
        Value::Message(Message::new(sender, request_id, tag, payload))
    }

    fn msg(tag: u16, payload: u64) -> Value {
        hop(1, 1, tag, payload)
    }

    #[test]
    fn try_pop_match_skips_non_matching_fifo() {
        let mb = Mailbox::new();
        mb.push(msg(9, 1)).unwrap();
        mb.push(msg(1, 42)).unwrap();
        mb.push(msg(9, 2)).unwrap();
        let got = mb.try_pop_match(1).unwrap().expect("match");
        assert_eq!(got.as_message().unwrap().payload, 42);
        assert_eq!(mb.try_pop().unwrap().unwrap().as_message().unwrap().tag, 9);
        assert_eq!(mb.try_pop().unwrap().unwrap().as_message().unwrap().payload, 2);
    }

    #[test]
    fn push_while_park_match_queues_junk_keeps_waiter() {
        let mb = Mailbox::new();
        let flow = dummy_flow();
        assert!(mb.park_match(flow, 1).unwrap().is_ok());
        assert!(matches!(mb.push(msg(9, 0)).unwrap(), Delivery::Queued));
        assert!(matches!(mb.push(msg(1, 7)).unwrap(), Delivery::Handoff(_)));
        assert_eq!(mb.try_pop().unwrap().unwrap().as_message().unwrap().tag, 9);
    }

    #[test]
    fn ask_does_not_consume_reply_for_another_request() {
        let mb = Mailbox::new();
        mb.push(hop(10, 2, 2, 99)).unwrap();
        mb.push(hop(10, 1, 2, 42)).unwrap();
        let filter = WaitFilter::Correlation {
            expect_request_id: 1,
            expect_sender: Some(10),
        };
        let got = mb.try_pop_filter(filter).unwrap().expect("id=1");
        assert_eq!(got.as_message().unwrap().payload, 42);
        // request_id=2 remains
        let left = mb.try_pop().unwrap().unwrap();
        assert_eq!(left.as_message().unwrap().request_id, 2);
    }

    #[test]
    fn ask_requires_reply_from_target() {
        let mb = Mailbox::new();
        let flow = dummy_flow();
        let filter = WaitFilter::Correlation {
            expect_request_id: 1,
            expect_sender: Some(10), // expect server=10
        };
        assert!(mb.park_filter(flow, filter).unwrap().is_ok());
        // spoof: same request_id, wrong sender
        assert!(matches!(
            mb.push(hop(99, 1, 2, 0)).unwrap(),
            Delivery::Queued
        ));
        // real reply from target
        assert!(matches!(
            mb.push(hop(10, 1, 2, 42)).unwrap(),
            Delivery::Handoff(_)
        ));
        assert_eq!(mb.try_pop().unwrap().unwrap().as_message().unwrap().sender, 99);
    }
}
