/// What happens when a hop would exceed the mailbox's **logical** capacity.
///
/// # Why there is no `Block`
///
/// Parking the **OS worker** on a full mailbox would stall every other flow
/// on that thread — the opposite of an M:N scheduler. Backpressure that
/// waits belongs to a future scheduler state (`WAITING_SEND` → runnable
/// when a slot frees), not to a queue method that holds a worker.
///
/// This revision therefore only offers **non-blocking** overflow:
///
/// | Policy | Effect |
/// |--------|--------|
/// | [`Reject`](Self::Reject) | Hop is not accepted; caller sees [`super::MailboxFull`] / [`crate::SendError::MailboxFull`] |
/// | [`DropNewest`](Self::DropNewest) | Incoming hop discarded; queue unchanged |
/// | [`DropOldest`](Self::DropOldest) | Oldest queued hop dropped, incoming enqueued (still FIFO among survivors) |
///
/// Default is [`Reject`](Self::Reject): fail-closed. Silent drop is an
/// explicit embedder choice.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowPolicy {
    Reject = 0,
    DropNewest = 1,
    DropOldest = 2,
}

impl Default for OverflowPolicy {
    fn default() -> Self {
        Self::Reject
    }
}

/// Memory + overflow contract for every mailbox spawned by a [`crate::Runtime`].
///
/// Fields are private so an invalid `u32` cannot sneak in as capacity.
/// Construct with [`MailboxConfig::new`] or [`MailboxConfig::DEFAULT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailboxConfig {
    capacity: super::MailboxCapacity,
    overflow: OverflowPolicy,
}

impl MailboxConfig {
    /// Compile-time-valid default: 256 hops, reject on overflow.
    pub const DEFAULT: MailboxConfig = MailboxConfig {
        capacity: super::MailboxCapacity::DEFAULT,
        overflow: OverflowPolicy::Reject,
    };

    pub const fn new(capacity: super::MailboxCapacity, overflow: OverflowPolicy) -> Self {
        Self {
            capacity,
            overflow,
        }
    }

    #[inline]
    pub const fn capacity(&self) -> super::MailboxCapacity {
        self.capacity
    }

    #[inline]
    pub const fn overflow(&self) -> OverflowPolicy {
        self.overflow
    }
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::mailbox::MailboxCapacity;

    #[test]
    fn default_is_reject_256() {
        let c = MailboxConfig::DEFAULT;
        assert_eq!(c.capacity().get(), 256);
        assert_eq!(c.overflow(), OverflowPolicy::Reject);
    }

    #[test]
    fn new_preserves_parts() -> Result<(), &'static str> {
        let cap = MailboxCapacity::new(4).ok_or("cap")?;
        let c = MailboxConfig::new(cap, OverflowPolicy::DropOldest);
        assert_eq!(c.capacity().get(), 4);
        assert_eq!(c.overflow(), OverflowPolicy::DropOldest);
        Ok(())
    }
}
