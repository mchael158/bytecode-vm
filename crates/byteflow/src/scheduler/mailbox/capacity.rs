/// Logical hop bound for one flow's inbox.
///
/// # Why not `usize`
///
/// A raw `capacity: usize` lets a 64-bit host pass an absurd value and
/// pretend it is a memory contract. An embeddable M:N runtime must pick
/// deliberate bounds: enough hops for request/reply bursts, not enough to
/// grow RSS until OOM when a slow consumer sits under a fast producer.
///
/// `MIN = 1` (a mailbox that cannot hold a hop is not a mailbox).
/// `MAX = 1 << 20` (~1M hops) is a hard ceiling, not a recommendation —
/// the default is [`MailboxCapacity::DEFAULT`] (256).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MailboxCapacity(u32);

impl MailboxCapacity {
    pub const MIN: u32 = 1;
    pub const MAX: u32 = 1 << 20;
    /// Default logical bound used by [`super::MailboxConfig::DEFAULT`].
    pub const DEFAULT: MailboxCapacity = MailboxCapacity(256);

    /// `Some` iff `value` is in `MIN..=MAX`.
    pub const fn new(value: u32) -> Option<Self> {
        if value < Self::MIN || value > Self::MAX {
            None
        } else {
            Some(Self(value))
        }
    }

    #[inline]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    #[inline]
    pub const fn get_u32(self) -> u32 {
        self.0
    }
}

impl Default for MailboxCapacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_over_max() -> Result<(), &'static str> {
        assert!(MailboxCapacity::new(0).is_none());
        assert!(MailboxCapacity::new(MailboxCapacity::MAX + 1).is_none());
        let min = MailboxCapacity::new(1).ok_or("min capacity")?;
        assert_eq!(min.get(), 1);
        let max = MailboxCapacity::new(MailboxCapacity::MAX).ok_or("max capacity")?;
        assert_eq!(max.get_u32(), MailboxCapacity::MAX);
        Ok(())
    }

    #[test]
    fn default_is_compile_time_valid() {
        assert_eq!(MailboxCapacity::DEFAULT.get(), 256);
        assert!(MailboxCapacity::new(256).is_some());
    }
}
