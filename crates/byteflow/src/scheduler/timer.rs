use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_deque::Injector;

use super::error::report_fault;
use super::mailbox::Mailbox;
use super::process::{Process, ProcessId};
use super::sync_lock;

/// What to do when a timer entry's deadline is reached.
enum TimerPayload {
    /// A `Sleep`-suspended process: just make it runnable again. Its `pc`
    /// already points past the `Sleep` instruction, so no register
    /// writeback is needed.
    WakeSleeper(Box<Process>),
    /// A `ReceiveTimeout`-suspended process, parked *inside its own
    /// mailbox* rather than held here directly (see
    /// [`super::mailbox::Mailbox`]'s doc comment). We only hold enough to
    /// find it again: its id (for logging/metrics) and a handle to the
    /// mailbox to attempt the take.
    WakeReceiver {
        pid: ProcessId,
        mailbox: Arc<Mailbox>,
        dest_reg: u8,
    },
}

struct TimerEntry {
    deadline: Instant,
    payload: TimerPayload,
}

// Ordering is by deadline only — two entries with the same deadline are
// "equal" for heap-ordering purposes even though they wake different
// processes. That's intentional: `BinaryHeap` only needs a total order to
// stay a valid heap, and we never rely on it to deduplicate entries.
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}

/// A single global `Sleep`/`ReceiveTimeout` deadline queue.
///
/// # On the choice of a binary heap over a real timing wheel
///
/// Design notes §19 calls for a hierarchical timing wheel (O(1) insert/
/// cancel, bucketed by coarse deadlines) — the right choice at BEAM/Tokio
/// scale, where timer churn is enormous. A `BinaryHeap<Reverse<TimerEntry>>`
/// behind one mutex is `O(log n)` insert and pop, which is the simpler,
/// well-understood structure and is more than fast enough to validate the
/// scheduling model end-to-end (this is exactly the "don't build everything
/// at once" ordering the design notes argue for in §36 — a wheel is a
/// drop-in replacement for this module's internals later, since nothing
/// outside `timer.rs` knows how deadlines are stored).
pub struct TimerWheel {
    heap: Mutex<BinaryHeap<Reverse<TimerEntry>>>,
    cvar: Condvar,
    shutdown: Mutex<bool>,
}

impl TimerWheel {
    pub fn new() -> Arc<Self> {
        Arc::new(TimerWheel {
            heap: Mutex::new(BinaryHeap::new()),
            cvar: Condvar::new(),
            shutdown: Mutex::new(false),
        })
    }

    pub fn schedule_sleep(&self, delay: Duration, process: Box<Process>) {
        let entry = TimerEntry {
            deadline: Instant::now() + delay,
            payload: TimerPayload::WakeSleeper(process),
        };
        self.push(entry);
    }

    pub fn schedule_receive_timeout(
        &self,
        delay: Duration,
        pid: ProcessId,
        mailbox: Arc<Mailbox>,
        dest_reg: u8,
    ) {
        let entry = TimerEntry {
            deadline: Instant::now() + delay,
            payload: TimerPayload::WakeReceiver {
                pid,
                mailbox,
                dest_reg,
            },
        };
        self.push(entry);
    }

    fn push(&self, entry: TimerEntry) {
        match sync_lock::lock(&self.heap, "TimerWheel::push") {
            Ok(mut heap) => {
                heap.push(Reverse(entry));
                self.cvar.notify_one();
            }
            Err(e) => report_fault(e),
        }
    }

    pub fn shutdown(&self) {
        match sync_lock::lock(&self.shutdown, "TimerWheel::shutdown") {
            Ok(mut flag) => *flag = true,
            Err(e) => report_fault(e),
        }
        self.cvar.notify_all();
    }

    /// Runs on a single dedicated OS thread (spawned by
    /// [`super::runtime::Runtime`]) for the life of the runtime.
    /// Pops every entry whose deadline has passed, resolves it into a
    /// runnable process, and pushes that process onto the shared global
    /// injector queue so any idle worker can pick it up — the timer thread
    /// itself never runs process code.
    ///
    /// Mutex poison → [`report_fault`] and exit the drive loop (fail-closed).
    pub fn drive(self: &Arc<Self>, injector: &Injector<Box<Process>>, notify: &(Mutex<()>, Condvar)) {
        loop {
            let mut heap = match sync_lock::lock(&self.heap, "TimerWheel::drive") {
                Ok(h) => h,
                Err(e) => {
                    report_fault(e);
                    return;
                }
            };
            let shutting_down = match sync_lock::lock(&self.shutdown, "TimerWheel::drive/shutdown") {
                Ok(g) => *g,
                Err(e) => {
                    report_fault(e);
                    return;
                }
            };
            if shutting_down {
                return;
            }
            match heap.peek() {
                None => {
                    match sync_lock::wait_timeout(
                        &self.cvar,
                        heap,
                        Duration::from_millis(250),
                        "TimerWheel::idle",
                    ) {
                        Ok((guard, _)) => {
                            drop(guard);
                        }
                        Err(e) => {
                            report_fault(e);
                            return;
                        }
                    }
                }
                Some(Reverse(top)) => {
                    let now = Instant::now();
                    if top.deadline <= now {
                        let Reverse(entry) = match heap.pop() {
                            Some(e) => e,
                            None => continue,
                        };
                        drop(heap);
                        self.fire(entry, injector, notify);
                    } else {
                        let wait_for = top.deadline - now;
                        match sync_lock::wait_timeout(
                            &self.cvar,
                            heap,
                            wait_for,
                            "TimerWheel::wait",
                        ) {
                            Ok((guard, _)) => drop(guard),
                            Err(e) => {
                                report_fault(e);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    fn fire(
        &self,
        entry: TimerEntry,
        injector: &Injector<Box<Process>>,
        notify: &(Mutex<()>, Condvar),
    ) {
        match entry.payload {
            TimerPayload::WakeSleeper(process) => {
                injector.push(process);
            }
            TimerPayload::WakeReceiver {
                pid,
                mailbox,
                dest_reg,
            } => {
                match mailbox.take_parked() {
                    Ok(Some(mut process)) => {
                        debug_assert_eq!(
                            process.id, pid,
                            "timer fired for a mailbox owned by a different process"
                        );
                        // Timeout won the race against a `Send` (see
                        // `Mailbox::take_parked`): deliver `Unit` as the
                        // "no message arrived in time" result.
                        let _ = process
                            .vm
                            .resume_with(dest_reg, crate::bytecode::Value::Unit);
                        injector.push(process);
                    }
                    Ok(None) => {
                        // A `Send` already woke it; nothing to do.
                    }
                    Err(e) => report_fault(e),
                }
            }
        }
        match sync_lock::lock(&notify.0, "TimerWheel::fire/notify") {
            Ok(_guard) => notify.1.notify_all(),
            Err(e) => report_fault(e),
        }
    }
}
