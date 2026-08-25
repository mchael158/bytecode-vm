//! Infrastructure failures of the scheduler (category C in the error model).
//!
//! # Failure taxonomy (Byteflow)
//!
//! | Kind | Example | Surface |
//! |------|---------|---------|
//! | A — user / API | `spawn` bad function index | `Result<_, SpawnError>` |
//! | B — process | `HwError`, native fault | `ProcessOutcome::Failed` → Supervisor |
//! | C — infrastructure | mutex poison, dead worker | [`RuntimeError`] + fail-closed |
//! | D — invariant | empty frame stack while running | types / `debug_assert` — not `unwrap` |
//!
//! **Panic is a runtime bug, not an error-handling mechanism.** Device faults
//! kill actors; scheduler faults are explicit and fail-closed.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Count of infrastructure faults observed since process start (Relaxed).
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// Scheduler / host infrastructure error — not a bytecode process fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// A `Mutex` was poisoned: another thread panicked while holding it.
    /// Shared tables may be inconsistent; callers must not continue using them.
    PoisonedLock(&'static str),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::PoisonedLock(where_) => {
                write!(f, "runtime mutex poisoned at {where_}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Record an infrastructure fault (stderr + counter). Does not panic.
#[cold]
pub fn report_fault(err: RuntimeError) {
    FAULTS.fetch_add(1, Ordering::Relaxed);
    eprintln!("byteflow: {err} — fail-closed");
}

/// How many [`report_fault`] calls have been made (tests / diagnostics).
pub fn fault_count() -> u64 {
    FAULTS.load(Ordering::Relaxed)
}

/// User-facing spawn / load errors (category A in the taxonomy above).
///
/// These used to be `.expect(...)` panics on `Runtime::new` / `spawn` /
/// OS thread creation. Panic is reserved for *bugs*; a bad function index
/// or a refused `thread::spawn` is an embedder-visible failure and must
/// surface as `Result` so the host can recover without taking workers down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// `function` index is outside the runtime chunk's function table.
    BadFunction { index: u32, table_size: u32 },
    /// Chunk failed verification before the runtime could start.
    VerifyFailed(String),
    /// OS refused to create a worker / timer / supervisor thread.
    ThreadSpawnFailed(String),
    /// `Vm::new` failed for a reason other than a bad function index
    /// (mapped from [`crate::Fault`] via `From`).
    VmInit(String),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::BadFunction { index, table_size } => {
                write!(
                    f,
                    "spawn: function index {index} out of range (table size {table_size})"
                )
            }
            SpawnError::VerifyFailed(msg) => write!(f, "chunk verification failed: {msg}"),
            SpawnError::ThreadSpawnFailed(msg) => {
                write!(f, "failed to spawn runtime thread: {msg}")
            }
            SpawnError::VmInit(msg) => write!(f, "vm init failed: {msg}"),
        }
    }
}

impl std::error::Error for SpawnError {}

impl From<crate::vm::Fault> for SpawnError {
    fn from(fault: crate::vm::Fault) -> Self {
        match fault {
            crate::vm::Fault::BadFunction { index, table_size } => {
                SpawnError::BadFunction { index, table_size }
            }
            other => SpawnError::VmInit(other.to_string()),
        }
    }
}
