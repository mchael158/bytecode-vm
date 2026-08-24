use std::sync::atomic::{AtomicU64, Ordering};

/// Runtime-wide counters (design notes §26). Every field is a plain
/// `AtomicU64` bumped with `Relaxed` ordering: these are monitoring
/// counters, not synchronization primitives, so we don't pay for anything
/// stronger than "eventually visible to a metrics scrape."
#[derive(Default)]
pub struct RuntimeMetrics {
    pub processes_spawned: AtomicU64,
    pub processes_completed: AtomicU64,
    pub processes_failed: AtomicU64,
    pub messages_sent: AtomicU64,
    pub steals: AtomicU64,
    pub reschedules: AtomicU64,
}

impl RuntimeMetrics {
    #[inline]
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RuntimeMetricsSnapshot {
        RuntimeMetricsSnapshot {
            processes_spawned: self.processes_spawned.load(Ordering::Relaxed),
            processes_completed: self.processes_completed.load(Ordering::Relaxed),
            processes_failed: self.processes_failed.load(Ordering::Relaxed),
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            steals: self.steals.load(Ordering::Relaxed),
            reschedules: self.reschedules.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time, non-atomic copy of [`RuntimeMetrics`] suitable for
/// printing or exporting.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeMetricsSnapshot {
    pub processes_spawned: u64,
    pub processes_completed: u64,
    pub processes_failed: u64,
    pub messages_sent: u64,
    pub steals: u64,
    pub reschedules: u64,
}

impl std::fmt::Display for RuntimeMetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "spawned={} completed={} failed={} messages={} steals={} reschedules={}",
            self.processes_spawned,
            self.processes_completed,
            self.processes_failed,
            self.messages_sent,
            self.steals,
            self.reschedules,
        )
    }
}
