//! Single choke-point for flow termination.
//!
//! Workers call [`finalize_flow`] instead of scattering revoke / DOWN /
//! link / registry / supervisor cleanup. The VM only produces an outcome;
//! this module owns the lifecycle transition.
//!
//! Finalization is **iterative** (a work list): linked peers and stranded
//! `WAITING_SEND` senders are queued rather than recursed, so a wide link
//! graph cannot blow the worker stack.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use crate::bytecode::{Message, Value};
use crate::log;

use super::error::{report_fault, RuntimeError};
use super::mailbox::Delivery;
use super::metrics::RuntimeMetrics;
use super::monitor::{DownEvent, FlowExitReason};
use super::process::{Flow, FlowId, FlowOutcome};
use super::runtime::{wake_workers, Shared};
use super::sync_lock;

/// Kill / linked-exit signal consumed at the start of a worker quantum.
pub struct KillSignals {
    inner: Mutex<HashMap<FlowId, FlowExitReason>>,
}

impl KillSignals {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn set(&self, id: FlowId, reason: FlowExitReason) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "KillSignals::set")?.insert(id, reason);
        Ok(())
    }

    pub fn take(&self, id: FlowId) -> Result<Option<FlowExitReason>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "KillSignals::take")?.remove(&id))
    }
}

/// sender FlowId → target FlowId whose mailbox holds a `WAITING_SEND`.
pub struct WaitingSendIndex {
    inner: Mutex<HashMap<FlowId, FlowId>>,
}

impl WaitingSendIndex {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, sender: FlowId, target: FlowId) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "WaitingSendIndex::insert")?.insert(sender, target);
        Ok(())
    }

    pub fn remove(&self, sender: FlowId) -> Result<Option<FlowId>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "WaitingSendIndex::remove")?.remove(&sender))
    }

    pub fn get(&self, sender: FlowId) -> Result<Option<FlowId>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "WaitingSendIndex::get")?
            .get(&sender)
            .copied())
    }
}

/// Askers parked on their own mailbox waiting for a reply from `target`.
///
/// When the target exits, [`take_waiters_of`] lets finalize resume those
/// waiters with [`crate::TAG_SYS_EXIT`] instead of leaving them parked forever.
pub struct AskWaitIndex {
    inner: Mutex<AskWaitInner>,
}

struct AskWaitInner {
    by_asker: HashMap<FlowId, FlowId>,
    by_target: HashMap<FlowId, HashSet<FlowId>>,
}

impl AskWaitIndex {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AskWaitInner {
                by_asker: HashMap::new(),
                by_target: HashMap::new(),
            }),
        }
    }

    pub fn insert(&self, asker: FlowId, target: FlowId) -> Result<(), RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "AskWaitIndex::insert")?;
        if let Some(old) = g.by_asker.insert(asker, target) {
            if let Some(set) = g.by_target.get_mut(&old) {
                set.remove(&asker);
                if set.is_empty() {
                    g.by_target.remove(&old);
                }
            }
        }
        g.by_target.entry(target).or_default().insert(asker);
        Ok(())
    }

    pub fn remove_asker(&self, asker: FlowId) -> Result<Option<FlowId>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "AskWaitIndex::remove_asker")?;
        let Some(target) = g.by_asker.remove(&asker) else {
            return Ok(None);
        };
        if let Some(set) = g.by_target.get_mut(&target) {
            set.remove(&asker);
            if set.is_empty() {
                g.by_target.remove(&target);
            }
        }
        Ok(Some(target))
    }

    pub fn take_waiters_of(&self, target: FlowId) -> Result<Vec<FlowId>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "AskWaitIndex::take_waiters_of")?;
        let Some(set) = g.by_target.remove(&target) else {
            return Ok(Vec::new());
        };
        for asker in &set {
            g.by_asker.remove(asker);
        }
        Ok(set.into_iter().collect())
    }
}

struct PendingExit {
    flow: Flow,
    outcome: FlowOutcome,
    reason: FlowExitReason,
}

/// Unregister, notify monitors, propagate links, sweep registry, complete join.
pub(crate) fn finalize_flow(
    shared: &Shared,
    flow: Flow,
    outcome: FlowOutcome,
    reason: FlowExitReason,
) {
    let mut work = vec![PendingExit {
        flow,
        outcome,
        reason,
    }];
    while let Some(pending) = work.pop() {
        finalize_one(shared, pending, &mut work);
    }
}

fn finalize_one(shared: &Shared, mut pending: PendingExit, work: &mut Vec<PendingExit>) {
    let id = pending.flow.id;
    let reason = pending.reason;

    log::info(format!(
        "finalize flow#{id} reason={reason} outcome={:?}",
        pending.outcome
    ));

    if let Err(e) = shared.kill_signals.take(id) {
        report_fault(e);
    }
    if let Err(e) = shared.waiting_send_at.remove(id) {
        report_fault(e);
    }
    if let Err(e) = shared.ask_waits.remove_asker(id) {
        report_fault(e);
    }

    if let Ok(Some(mailbox)) = shared.directory.lookup(id) {
        match mailbox.close() {
            Ok(senders) => {
                for sender in senders {
                    if let Err(e) = shared.waiting_send_at.remove(sender.id) {
                        report_fault(e);
                    }
                    work.push(PendingExit {
                        flow: sender,
                        outcome: FlowOutcome::Failed(format!("send target {id} exited")),
                        reason: FlowExitReason::Fault,
                    });
                }
            }
            Err(e) => report_fault(e),
        }
    }

    wake_orphaned_asks(shared, id, reason);

    if let Err(e) = shared.caps.revoke_flow(id) {
        report_fault(e);
    }
    if let Err(e) = shared.quotas.remove(id) {
        report_fault(e);
    }
    if let Err(e) = shared.directory.unregister(id) {
        report_fault(e);
    }
    if let Err(e) = shared.monitors.remove_owned_by(id) {
        report_fault(e);
    }
    if let Err(e) = shared.registry.unregister_flow(id) {
        report_fault(e);
    }

    let downs = match shared.monitors.notify_target_exit(id, reason) {
        Ok(events) => events,
        Err(e) => {
            report_fault(e);
            Vec::new()
        }
    };
    for event in downs {
        deliver_down(shared, event);
    }

    let peers = match shared.links.remove_links_of(id) {
        Ok(list) => list,
        Err(e) => {
            report_fault(e);
            Vec::new()
        }
    };
    if reason.is_abnormal() {
        for (_, peer) in peers {
            collect_link_exit(shared, peer, work);
        }
    }

    if matches!(pending.outcome, FlowOutcome::Completed(_)) {
        RuntimeMetrics::inc(&shared.metrics.processes_completed);
    } else {
        RuntimeMetrics::inc(&shared.metrics.processes_failed);
    }
    if let Some(link) = pending.flow.supervisor.take() {
        link.notify(id, pending.outcome.clone());
    }
    pending.flow.complete(pending.outcome);
}

pub(crate) fn deliver_down(shared: &Shared, event: DownEvent) {
    let hop = Value::Message(Message::down(
        event.monitor.as_u64(),
        event.target.as_u64(),
        event.reason.as_u64(),
    ));
    let mailbox = match shared.directory.lookup(event.owner) {
        Ok(Some(m)) => m,
        Ok(None) => return,
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    match mailbox.push_system(hop.clone()) {
        Ok(Delivery::Handoff(mut owner)) => {
            let _ = shared.ask_waits.remove_asker(owner.id);
            if let Some(dest) = owner.last_receive_dest {
                let _ = owner.vm.resume_with(dest, hop);
                owner
                    .metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            shared.injector.push(owner);
            wake_workers(shared);
        }
        Ok(_) => {}
        Err(e) => report_fault(e),
    }
}

fn wake_orphaned_asks(shared: &Shared, target: FlowId, reason: FlowExitReason) {
    let waiters = match shared.ask_waits.take_waiters_of(target) {
        Ok(w) => w,
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    if waiters.is_empty() {
        return;
    }
    let hop = Value::Message(Message::linked_exit(target.as_u64(), reason.as_u64()));
    for asker in waiters {
        let mailbox = match shared.directory.lookup(asker) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(e) => {
                report_fault(e);
                continue;
            }
        };
        match mailbox.take_parked() {
            Ok(Some(mut flow)) => {
                if let Some(dest) = flow.last_receive_dest {
                    let _ = flow.vm.resume_with(dest, hop.clone());
                    flow.metrics
                        .messages_received
                        .fetch_add(1, Ordering::Relaxed);
                }
                shared.injector.push(flow);
                wake_workers(shared);
            }
            Ok(None) => {}
            Err(e) => report_fault(e),
        }
    }
}

/// Set a kill signal and pull the flow out if it is parked (receive or
/// `WAITING_SEND`). A running flow stays on its worker and dies at the
/// next quantum (cooperative preemption).
pub(crate) fn extract_for_kill(
    shared: &Shared,
    id: FlowId,
    reason: FlowExitReason,
) -> Option<Flow> {
    let mailbox = match shared.directory.lookup(id) {
        Ok(Some(m)) => m,
        Ok(None) => return None,
        Err(e) => {
            report_fault(e);
            return None;
        }
    };
    if let Err(e) = shared.kill_signals.set(id, reason) {
        report_fault(e);
        return None;
    }

    match mailbox.take_parked() {
        Ok(Some(parked)) => return Some(*parked),
        Ok(None) => {}
        Err(e) => report_fault(e),
    }

    let target = match shared.waiting_send_at.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return None,
        Err(e) => {
            report_fault(e);
            return None;
        }
    };
    let mb = match shared.directory.lookup(target) {
        Ok(Some(m)) => m,
        Ok(None) => return None,
        Err(e) => {
            report_fault(e);
            return None;
        }
    };
    match mb.take_waiting_sender(id) {
        Ok(Some(sender)) => {
            if let Err(e) = shared.waiting_send_at.remove(id) {
                report_fault(e);
            }
            Some(*sender)
        }
        Ok(None) => None,
        Err(e) => {
            report_fault(e);
            None
        }
    }
}

/// Host / supervisor abort: finalize immediately when parked, otherwise
/// the next worker quantum consumes the signal.
pub(crate) fn request_kill(shared: &Shared, id: FlowId, reason: FlowExitReason) {
    if let Some(flow) = extract_for_kill(shared, id, reason) {
        finalize_flow(
            shared,
            flow,
            FlowOutcome::Failed(format!("killed ({reason})")),
            reason,
        );
    }
}

fn collect_link_exit(shared: &Shared, peer: FlowId, work: &mut Vec<PendingExit>) {
    if let Some(flow) = extract_for_kill(shared, peer, FlowExitReason::Link) {
        work.push(PendingExit {
            flow,
            outcome: FlowOutcome::Failed("linked exit (link)".into()),
            reason: FlowExitReason::Link,
        });
    }
}
