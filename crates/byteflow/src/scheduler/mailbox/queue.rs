use std::collections::VecDeque;

use crate::bytecode::Value;

use super::OverflowPolicy;

/// FIFO hop storage with a **logical** bound independent of physical
/// allocation.
///
/// # Logical vs physical
///
/// `limit` is how many hops this inbox may hold. `VecDeque` capacity is
/// how much the allocator currently reserved. A flow that receives one hop
/// with `limit = 4096` must **not** pay for 4096 slots up front.
///
/// Growth is geometric and capped at `limit` (`reserve_for_push`). That
/// keeps hot mailboxes from reallocating on every push without pre-paying
/// the worst-case footprint.
///
/// # Why `VecDeque`, not an `unsafe` ring
///
/// A dedicated `MaybeUninit` ring would be a natural next step, but this
/// crate is `#![forbid(unsafe_code)]`. The queue is a `pub(crate)`
/// abstraction so a later ring can replace `inner` without touching
/// FlowCap, Ask, or the worker loop.
pub(crate) struct MailboxQueue {
    inner: VecDeque<Value>,
    limit: usize,
}

impl MailboxQueue {
    pub(crate) fn new(limit: usize) -> Self {
        debug_assert!(limit >= 1);
        Self {
            inner: VecDeque::new(),
            limit,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[inline]
    pub(crate) fn is_full(&self) -> bool {
        self.inner.len() >= self.limit
    }

    #[inline]
    pub(crate) fn inner_mut(&mut self) -> &mut VecDeque<Value> {
        &mut self.inner
    }

    /// Try to accept `value` under `policy`. Returns `None` on
    /// [`OverflowPolicy::Reject`] when already at the logical limit.
    pub(crate) fn enqueue(
        &mut self,
        value: Value,
        policy: OverflowPolicy,
    ) -> Option<EnqueueEffect> {
        if !self.is_full() {
            if !reserve_for_push(&mut self.inner, self.limit) {
                return None;
            }
            self.inner.push_back(value);
            return Some(EnqueueEffect::Enqueued);
        }
        match policy {
            OverflowPolicy::Reject => None,
            OverflowPolicy::DropNewest => Some(EnqueueEffect::DroppedNewest),
            OverflowPolicy::DropOldest => {
                let _ = self.inner.pop_front();
                self.inner.push_back(value);
                Some(EnqueueEffect::DroppedOldest)
            }
        }
    }
}

/// Physical growth: double current `VecDeque` capacity, never past `limit`.
fn reserve_for_push(queue: &mut VecDeque<Value>, capacity: usize) -> bool {
    if queue.len() < queue.capacity() {
        return true;
    }
    let current = queue.capacity();
    let next = current.max(1).saturating_mul(2).min(capacity);
    if next <= current {
        return queue.len() < capacity;
    }
    queue.reserve(next - current);
    true
}

/// What [`MailboxQueue::enqueue`] did (stats + [`super::Delivery`] mapping).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnqueueEffect {
    Enqueued,
    DroppedNewest,
    DroppedOldest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Message, Value};

    fn hop(n: u64) -> Value {
        Value::Message(Message::new(1, n, 1, n))
    }

    fn hop_payload(q: &mut MailboxQueue) -> Result<u64, &'static str> {
        match q.inner.pop_front() {
            Some(v) => match v.as_message() {
                Some(m) => Ok(m.payload),
                None => Err("expected Message"),
            },
            None => Err("queue empty"),
        }
    }

    #[test]
    fn reject_at_limit() {
        let mut q = MailboxQueue::new(2);
        assert!(matches!(
            q.enqueue(hop(1), OverflowPolicy::Reject),
            Some(EnqueueEffect::Enqueued)
        ));
        assert!(matches!(
            q.enqueue(hop(2), OverflowPolicy::Reject),
            Some(EnqueueEffect::Enqueued)
        ));
        assert!(q.enqueue(hop(3), OverflowPolicy::Reject).is_none());
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn drop_newest_keeps_old() -> Result<(), &'static str> {
        let mut q = MailboxQueue::new(1);
        q.enqueue(hop(1), OverflowPolicy::DropNewest);
        assert_eq!(
            q.enqueue(hop(2), OverflowPolicy::DropNewest),
            Some(EnqueueEffect::DroppedNewest)
        );
        assert_eq!(hop_payload(&mut q)?, 1);
        Ok(())
    }

    #[test]
    fn drop_oldest_slides() -> Result<(), &'static str> {
        let mut q = MailboxQueue::new(2);
        q.enqueue(hop(1), OverflowPolicy::DropOldest);
        q.enqueue(hop(2), OverflowPolicy::DropOldest);
        assert_eq!(
            q.enqueue(hop(3), OverflowPolicy::DropOldest),
            Some(EnqueueEffect::DroppedOldest)
        );
        assert_eq!(hop_payload(&mut q)?, 2);
        assert_eq!(hop_payload(&mut q)?, 3);
        Ok(())
    }
}
