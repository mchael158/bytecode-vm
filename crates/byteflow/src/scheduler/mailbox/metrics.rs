/// Per-mailbox counters (Relaxed atomics would over-synchronize a hot
/// inbox). Updated only while the mailbox mutex is held — same critical
/// section as enqueue / park — then copied out for host scrapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailboxStats {
    pub enqueued: u64,
    pub dequeued: u64,
    pub rejected: u64,
    pub dropped_newest: u64,
    pub dropped_oldest: u64,
}
