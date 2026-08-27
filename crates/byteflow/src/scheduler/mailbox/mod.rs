//! Per-flow inbox: bounded hop queue + parked waiter (anti lost-wakeup).
//!
//! # Memory contract
//!
//! Unbounded `VecDeque` growth is not a capacity API — it is an OOM path
//! when many flows share few workers. Every mailbox is constructed with a
//! [`MailboxConfig`]: a validated [`MailboxCapacity`] and an
//! [`OverflowPolicy`]. Logical bound ≠ physical allocation; the queue grows
//! geometrically up to the limit (see [`queue`]).
//!
//! There is **no** `Block` policy. Blocking an OS worker on a full inbox
//! would stall every other flow on that thread. Overflow is Reject /
//! DropNewest / DropOldest; scheduler-level `WAITING_SEND` is a later
//! phase.
//!
//! # Wake
//!
//! `park` and `push` share one mutex (lost-wakeup invariant — keep this
//! comment and the race diagram). A hop that does not match a selective
//! waiter is queued (if the bound allows) and the waiter stays parked.
//! Overflow that **drops** a hop never produces [`Delivery::Handoff`].
//!
//! See `docs/mailbox.md`.

mod capacity;
mod metrics;
mod policy;
mod queue;

use std::sync::Mutex;

use crate::bytecode::Value;

use super::error::RuntimeError;
use super::process::Flow;
use super::sync_lock;

pub use capacity::MailboxCapacity;
pub use metrics::MailboxStats;
pub use policy::{MailboxConfig, OverflowPolicy};

use queue::{EnqueueEffect, MailboxQueue};

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
            Self::Tag(expected_tag) => match value.as_message() {
                Some(m) => m.tag == expected_tag,
                None => false,
            },
            Self::Correlation {
                expect_request_id,
                expect_sender,
            } => match value.as_message() {
                Some(m) => {
                    let id_ok = m.request_id == expect_request_id;
                    let sender_ok = match expect_sender {
                        Some(s) => m.sender == s,
                        None => true,
                    };
                    id_ok && sender_ok
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
/// Wake only happens when a hop is **accepted and matches** the waiter.
/// A flow that is already runnable (message queued, nobody parked) does
/// not generate extra scheduler work — no wake storm on every `Send`.
///
/// # Selective wait (`ReceiveMatch` / `Ask`)
///
/// When parked with a non-[`WaitFilter::Any`] filter, only a hop that
/// satisfies the filter wakes the flow. Other hops are appended to the
/// queue (subject to the bound) and the waiter stays parked (FIFO skip,
/// never drop matching semantics).
///
/// # Bound
///
/// `push` may return [`MailboxFull`] when the policy is Reject and the
/// logical capacity is already occupied **and** nobody matching is parked.
/// A parked waiter that matches the hop still takes a **handoff** — that
/// hop never occupies a queue slot.
pub struct Mailbox {
    inner: Mutex<MailboxInner>,
    config: MailboxConfig,
}

struct MailboxInner {
    queue: MailboxQueue,
    parked: Option<Box<Flow>>,
    /// Active while `parked` is `Some`. Ignored when nobody is waiting.
    parked_filter: WaitFilter,
    stats: MailboxStats,
}

/// Outcome of pushing a message.
///
/// `Queued*` means the hop (or a replacement under DropOldest) lives in
/// the inbox for a later `Receive`. [`Handoff`] means a parked flow was
/// waiting for **this** hop — the caller (`worker::deliver` /
/// [`crate::Runtime::send`]) must `resume_with` and re-enqueue the flow.
/// Dropped variants never wake a waiter.
pub enum Delivery {
    Queued,
    QueuedDropOldest,
    DroppedNewest,
    Handoff(Box<Flow>),
}

/// Inbox at logical capacity under [`OverflowPolicy::Reject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxFull;

impl std::fmt::Display for MailboxFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mailbox full")
    }
}

impl std::error::Error for MailboxFull {}

impl Mailbox {
    pub fn new() -> Self {
        Self::with_config(MailboxConfig::DEFAULT)
    }

    pub fn with_config(config: MailboxConfig) -> Self {
        Mailbox {
            inner: Mutex::new(MailboxInner {
                queue: MailboxQueue::new(config.capacity().get()),
                parked: None,
                parked_filter: WaitFilter::Any,
                stats: MailboxStats::default(),
            }),
            config,
        }
    }

    #[inline]
    pub fn config(&self) -> MailboxConfig {
        self.config
    }

    /// Snapshot of enqueue/dequeue/drop counters (under the mailbox lock).
    pub fn stats(&self) -> Result<MailboxStats, RuntimeError> {
        let inner = sync_lock::lock(&self.inner, "Mailbox::stats")?;
        Ok(inner.stats)
    }

    /// Push `value` under the mailbox config.
    ///
    /// 1. If a matching waiter is parked → [`Delivery::Handoff`] (does not
    ///    consume a queue slot).
    /// 2. Else enqueue / overflow according to [`OverflowPolicy`].
    /// 3. [`MailboxFull`] only for Reject when the queue is already at
    ///    the logical limit.
    ///
    /// Mutex poison → [`RuntimeError`] (fail-closed).
    pub fn push(&self, value: Value) -> Result<Result<Delivery, MailboxFull>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::push")?;
        if let Some(flow) = inner.parked.take() {
            if inner.parked_filter.matches(&value) {
                inner.parked_filter = WaitFilter::Any;
                inner.stats.dequeued = inner.stats.dequeued.saturating_add(1);
                inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
                return Ok(Ok(Delivery::Handoff(flow)));
            }
            inner.parked = Some(flow);
            return Ok(enqueue_locked(
                &mut inner,
                value,
                self.config.overflow(),
            ));
        }
        Ok(enqueue_locked(
            &mut inner,
            value,
            self.config.overflow(),
        ))
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
        let got = take_with_filter(inner.queue.inner_mut(), filter);
        if got.is_some() {
            inner.stats.dequeued = inner.stats.dequeued.saturating_add(1);
        }
        Ok(got)
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
        if let Some(value) = take_with_filter(inner.queue.inner_mut(), filter) {
            inner.stats.dequeued = inner.stats.dequeued.saturating_add(1);
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

fn enqueue_locked(
    inner: &mut MailboxInner,
    value: Value,
    policy: OverflowPolicy,
) -> Result<Delivery, MailboxFull> {
    match inner.queue.enqueue(value, policy) {
        Some(EnqueueEffect::Enqueued) => {
            inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
            Ok(Delivery::Queued)
        }
        Some(EnqueueEffect::DroppedOldest) => {
            inner.stats.dropped_oldest = inner.stats.dropped_oldest.saturating_add(1);
            inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
            Ok(Delivery::QueuedDropOldest)
        }
        Some(EnqueueEffect::DroppedNewest) => {
            inner.stats.dropped_newest = inner.stats.dropped_newest.saturating_add(1);
            Ok(Delivery::DroppedNewest)
        }
        None => {
            inner.stats.rejected = inner.stats.rejected.saturating_add(1);
            Err(MailboxFull)
        }
    }
}

/// Remove one matching hop from `queue`, preserving relative order of
/// everything else (FIFO skip — never drop non-matching entries).
fn take_with_filter(
    queue: &mut std::collections::VecDeque<Value>,
    filter: WaitFilter,
) -> Option<Value> {
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

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn dummy_flow() -> Result<Box<Flow>, Box<dyn std::error::Error>> {
        let mut b = ChunkBuilder::new("mb");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let chunk = b.finish();
        let vm = Vm::new(Arc::new(chunk), NativeTable::empty(), 0, &[])?;
        let (tx, _rx) = oneshot::channel();
        Ok(Box::new(Flow::new(
            next_flow_id(),
            vm,
            Arc::new(Mailbox::new()),
            RestartPolicy::Never,
            tx,
        )))
    }

    fn hop(sender: u64, request_id: u64, tag: u16, payload: u64) -> Value {
        Value::Message(Message::new(sender, request_id, tag, payload))
    }

    fn msg(tag: u16, payload: u64) -> Value {
        hop(1, 1, tag, payload)
    }

    fn tiny_reject(n: u32) -> Result<Mailbox, Box<dyn std::error::Error>> {
        let cap = MailboxCapacity::new(n).ok_or("invalid mailbox capacity")?;
        Ok(Mailbox::with_config(MailboxConfig::new(
            cap,
            OverflowPolicy::Reject,
        )))
    }

    fn hop_msg(value: &Value) -> Result<Message, Box<dyn std::error::Error>> {
        value.as_message().ok_or("expected Message hop".into())
    }

    #[test]
    fn try_pop_match_skips_non_matching_fifo() -> TestResult {
        let mb = Mailbox::new();
        mb.push(msg(9, 1))??;
        mb.push(msg(1, 42))??;
        mb.push(msg(9, 2))??;
        let got = mb.try_pop_match(1)?.ok_or("match")?;
        assert_eq!(hop_msg(&got)?.payload, 42);
        assert_eq!(
            hop_msg(&mb.try_pop()?.ok_or("first leftover")?)?.tag,
            9
        );
        assert_eq!(
            hop_msg(&mb.try_pop()?.ok_or("second leftover")?)?.payload,
            2
        );
        Ok(())
    }

    #[test]
    fn push_while_park_match_queues_junk_keeps_waiter() -> TestResult {
        let mb = Mailbox::new();
        let flow = dummy_flow()?;
        assert!(mb.park_match(flow, 1)?.is_ok());
        assert!(matches!(mb.push(msg(9, 0))??, Delivery::Queued));
        assert!(matches!(mb.push(msg(1, 7))??, Delivery::Handoff(_)));
        assert_eq!(hop_msg(&mb.try_pop()?.ok_or("queued junk")?)?.tag, 9);
        Ok(())
    }

    #[test]
    fn ask_does_not_consume_reply_for_another_request() -> TestResult {
        let mb = Mailbox::new();
        mb.push(hop(10, 2, 2, 99))??;
        mb.push(hop(10, 1, 2, 42))??;
        let filter = WaitFilter::Correlation {
            expect_request_id: 1,
            expect_sender: Some(10),
        };
        let got = mb.try_pop_filter(filter)?.ok_or("id=1")?;
        assert_eq!(hop_msg(&got)?.payload, 42);
        let left = mb.try_pop()?.ok_or("leftover")?;
        assert_eq!(hop_msg(&left)?.request_id, 2);
        Ok(())
    }

    #[test]
    fn ask_requires_reply_from_target() -> TestResult {
        let mb = Mailbox::new();
        let flow = dummy_flow()?;
        let filter = WaitFilter::Correlation {
            expect_request_id: 1,
            expect_sender: Some(10),
        };
        assert!(mb.park_filter(flow, filter)?.is_ok());
        assert!(matches!(mb.push(hop(99, 1, 2, 0))??, Delivery::Queued));
        assert!(matches!(mb.push(hop(10, 1, 2, 42))??, Delivery::Handoff(_)));
        assert_eq!(
            hop_msg(&mb.try_pop()?.ok_or("non-matching queued")?)?.sender,
            99
        );
        Ok(())
    }

    #[test]
    fn reject_when_full_without_waiter() -> TestResult {
        let mb = tiny_reject(1)?;
        assert!(matches!(mb.push(msg(1, 1))??, Delivery::Queued));
        assert!(matches!(mb.push(msg(1, 2))?, Err(MailboxFull)));
        let s = mb.stats()?;
        assert_eq!(s.enqueued, 1);
        assert_eq!(s.rejected, 1);
        Ok(())
    }

    #[test]
    fn matching_handoff_does_not_count_as_full() -> TestResult {
        let mb = tiny_reject(1)?;
        mb.push(msg(9, 0))??;
        let flow = dummy_flow()?;
        assert!(mb.park_match(flow, 1)?.is_ok());
        assert!(matches!(mb.push(msg(1, 7))??, Delivery::Handoff(_)));
        assert_eq!(hop_msg(&mb.try_pop()?.ok_or("queued")?)?.tag, 9);
        Ok(())
    }

    #[test]
    fn drop_oldest_still_wakes_on_match() -> TestResult {
        let cap = MailboxCapacity::new(1).ok_or("cap")?;
        let mb = Mailbox::with_config(MailboxConfig::new(cap, OverflowPolicy::DropOldest));
        let flow = dummy_flow()?;
        assert!(mb.park_match(flow, 1)?.is_ok());
        mb.push(msg(9, 1))??;
        assert!(matches!(mb.push(msg(1, 2))??, Delivery::Handoff(_)));
        Ok(())
    }
}
