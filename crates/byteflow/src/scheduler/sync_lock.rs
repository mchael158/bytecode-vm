//! Fail-fast locking for scheduler shared state.
//!
//! # Why we do **not** recover from poison
//!
//! `std::sync::Mutex` marks itself poisoned when a thread panics while holding
//! the guard. That means the protected value *may* be mid-update
//! (directory shard, mailbox queue, supervisor tables, oneshot slot, …).
//!
//! Silently calling `PoisonError::into_inner()` and continuing is the wrong
//! trade-off for a concurrent runtime: you schedule and deliver messages
//! against potentially inconsistent tables. High-integrity practice
//! (fail closed) is to abort the operation with a loud diagnostic so the
//! fault cannot cascade as silent corruption.
//!
//! Process-level panics are already isolated by `catch_unwind` around
//! `Vm::run` and never hold these locks across that boundary. Poison here
//! therefore implies a **scheduler / host bug**, not a buggy actor.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Acquire `m`. Panics if the mutex is poisoned (integrity fault).
#[inline]
pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(err) => integrity_fault("Mutex::lock", err),
    }
}

/// Wait on `cvar`. Panics if the wait observes a poisoned mutex.
#[inline]
pub(super) fn wait<'a, T>(cvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    match cvar.wait(guard) {
        Ok(guard) => guard,
        Err(err) => integrity_fault("Condvar::wait", err),
    }
}

/// Timed wait. Panics if the wait observes a poisoned mutex.
#[inline]
pub(super) fn wait_timeout<'a, T>(
    cvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> (MutexGuard<'a, T>, std::sync::WaitTimeoutResult) {
    match cvar.wait_timeout(guard, timeout) {
        Ok(pair) => pair,
        Err(err) => integrity_fault("Condvar::wait_timeout", err),
    }
}

#[cold]
#[inline(never)]
fn integrity_fault<T>(op: &'static str, err: PoisonError<T>) -> ! {
    // Drop the poisoned guard without reading `T`: we refuse to trust it.
    drop(err);
    panic!(
        "byteflow: {op} observed a poisoned mutex — shared scheduler state \
         may be inconsistent; refusing to continue (fail-closed)"
    );
}
