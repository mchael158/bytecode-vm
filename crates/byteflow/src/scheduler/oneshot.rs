use std::sync::{Arc, Condvar, Mutex};

use super::sync_lock;

/// A single-value, single-producer/single-consumer handoff, used to deliver
/// a process's terminal [`super::process::ProcessOutcome`] to whoever holds
/// its [`super::handle::ProcessHandle`].
///
/// This is intentionally not a general MPSC channel — every process has
/// exactly one completion event and exactly one handle — so it is just a
/// `Mutex<Option<T>>` + `Condvar`, with no allocation beyond the shared
/// `Arc` and no dependency on a channel crate for something this narrow.
struct Inner<T> {
    slot: Mutex<Option<T>>,
    cvar: Condvar,
}

pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        slot: Mutex::new(None),
        cvar: Condvar::new(),
    });
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

impl<T> Sender<T> {
    /// Deliver the value. Consumes `self`: a `Sender` can only send once,
    /// enforced at the type level rather than by a runtime "already sent"
    /// check.
    pub fn send(self, value: T) {
        let mut slot = sync_lock::lock(&self.inner.slot);
        *slot = Some(value);
        self.inner.cvar.notify_one();
    }
}

impl<T> Receiver<T> {
    /// Block the calling (native) thread until the value is available.
    /// Only ever called from an embedder's own thread (e.g. `main`) waiting
    /// on a top-level process — never from inside a worker thread, which
    /// must never block on anything but its own park/steal loop.
    pub fn join(self) -> T {
        let mut slot = sync_lock::lock(&self.inner.slot);
        loop {
            if let Some(v) = slot.take() {
                return v;
            }
            slot = sync_lock::wait(&self.inner.cvar, slot);
        }
    }
}
