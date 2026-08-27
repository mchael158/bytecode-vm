//! Worker threads: drive flows, apply scheduler effects, enforce hop identity
//! and **FlowCap** resolution.
//!
//! # Authenticated Atomic Hop + capabilities
//!
//! Before mailbox delivery, outgoing hops are stamped (`Message.sender`) and
//! granted a **SEND**-only `reply_cap`. `Send` / `Ask` resolve
//! [`Value::Cap`] through [`CapTable`](super::capability::CapTable); raw
//! [`Value::Pid`] is not an address.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{Message, Value};
use crate::log;
use crate::vm::VmResult;
use crossbeam_deque::{Steal, Worker as LocalDeque};

use super::capability::{CapId, CapRights};
use super::error::report_fault;
use super::mailbox::{Delivery, WaitFilter};
use super::metrics::RuntimeMetrics;
use super::process::{Flow, FlowId, FlowOutcome};
use super::runtime::{spawn_on, wake_workers, Shared};
use super::sync_lock;

/// S1 + reply grant: single choke-point before mailbox `push` on bytecode hops.
///
/// `Message.sender` / `reply_cap` are register/native data until this call.
/// The scheduler owns the executing flow’s identity and **assigns** both:
/// it does not “check equality” against forgeable fields (wrong security
/// primitive). Every future opcode that delivers a [`Message`] must call
/// this (or equivalent); do not duplicate ad-hoc stamp assignments elsewhere.
fn authenticate_outgoing_message(
    shared: &Shared,
    current_flow: FlowId,
    message: Message,
) -> Result<Message, String> {
    let reply = shared
        .caps
        .mint(current_flow, CapRights::SEND)
        .map_err(|e| e.to_string())?;
    Ok(message.authenticate(current_flow.as_u64(), reply.as_u64()))
}

/// Resolve `CapId` and require `need` rights. Returns target [`FlowId`].
///
/// Fail-closed: unknown Cap, revoked Cap, or insufficient rights → error
/// string (worker finishes the flow). Never treat CapId as FlowId.
fn resolve_cap(shared: &Shared, raw: u64, need: CapRights) -> Result<FlowId, String> {
    let id = CapId(raw);
    match shared.caps.resolve(id) {
        Ok(Some(entry)) if entry.rights.contains(need) => Ok(entry.flow),
        Ok(Some(_)) => Err(format!("capability {id} lacks required rights")),
        Ok(None) => Err(format!("unknown or revoked capability {id}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Worker main loop. Panics inside a flow are caught here so one
/// flow's bug cannot take the OS thread down (see `Fault` docs).
pub fn run_worker(shared: Arc<Shared>, local: LocalDeque<Box<Flow>>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        match find_work(&shared, &local) {
            Some(flow) => drive_process(&shared, &local, flow),
            None => wait_for_work(&shared),
        }
    }
}

fn find_work(shared: &Shared, local: &LocalDeque<Box<Flow>>) -> Option<Box<Flow>> {
    if let Some(flow) = local.pop() {
        return Some(flow);
    }

    loop {
        match shared.injector.steal() {
            Steal::Success(flow) => return Some(flow),
            Steal::Empty => break,
            Steal::Retry => continue,
        }
    }

    for stealer in &shared.stealers {
        loop {
            match stealer.steal() {
                Steal::Success(flow) => {
                    RuntimeMetrics::inc(&shared.metrics.steals);
                    return Some(flow);
                }
                Steal::Empty => break,
                Steal::Retry => continue,
            }
        }
    }

    None
}

fn wait_for_work(shared: &Shared) {
    let (lock, cvar) = &shared.notify;
    let guard = match sync_lock::lock(lock, "worker::wait_for_work") {
        Ok(g) => g,
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    if shared.shutdown.load(Ordering::Acquire) {
        return;
    }
    if let Err(e) =
        sync_lock::wait_timeout(cvar, guard, Duration::from_millis(50), "worker::wait_timeout")
    {
        report_fault(e);
    }
}

fn drive_process(
    shared: &Arc<Shared>,
    local: &LocalDeque<Box<Flow>>,
    mut flow: Box<Flow>,
) {
    if let Some(msg) = flow.pending_message.take() {
        if let Some(dest) = flow.last_receive_dest {
            let _ = flow.vm.resume_with(dest, msg);
            flow
                .metrics
                .messages_received
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            local.push(flow);
            return;
        }

        let ran = panic::catch_unwind(AssertUnwindSafe(|| flow.vm.run(shared.quantum)));
        flow
            .metrics
            .instructions
            .store(flow.vm.instructions_executed(), Ordering::Relaxed);

        let result = match ran {
            Ok(r) => r,
            Err(_) => {
                finish_failed(shared, *flow, "flow panicked".into());
                return;
            }
        };

        match result {
            VmResult::Complete(value) => {
                finish_ok(shared, *flow, value);
                return;
            }
            VmResult::Trap(fault) => {
                finish_failed(shared, *flow, fault.to_string());
                return;
            }
            VmResult::Yield => {
                RuntimeMetrics::inc(&shared.metrics.reschedules);
                flow.metrics.reschedules.fetch_add(1, Ordering::Relaxed);
                local.push(flow);
                return;
            }
            VmResult::Sleep(delay) => {
                shared.timer.schedule_sleep(delay, flow);
                return;
            }
            VmResult::SelfPid { dest_reg } => {
                match shared.caps.mint(flow.id, CapRights::SEND_ASK) {
                    Ok(cap) => {
                        let _ = flow.vm.resume_with(dest_reg, Value::Cap(cap.as_u64()));
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Spawn {
                function,
                args,
                dest_reg,
            } => {
                match spawn_on(
                    shared,
                    &flow.vm.chunk_arc(),
                    &flow.vm.natives_arc(),
                    function,
                    &args,
                    flow.restart_policy,
                    None,
                ) {
                    Ok(child) => {
                        let child_id = child.id();
                        log::info(format!(
                            "spawn parent=flow#{} child=flow#{} fn={}",
                            flow.id.as_u64(),
                            child_id.as_u64(),
                            function
                        ));
                        match shared.caps.mint(child_id, CapRights::SEND_ASK) {
                            Ok(cap) => {
                                let _ = flow.vm.resume_with(dest_reg, Value::Cap(cap.as_u64()));
                            }
                            Err(e) => {
                                finish_failed(shared, *flow, e.to_string());
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Send {
                target_cap,
                message,
            } => {
                let Some(msg) = message.as_message() else {
                    finish_failed(
                        shared,
                        *flow,
                        "Send invariant broken: hop is not Value::Message".into(),
                    );
                    return;
                };
                let target = match resolve_cap(shared, target_cap, CapRights::SEND) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e);
                        return;
                    }
                };
                let stamped = match authenticate_outgoing_message(shared, flow.id, msg) {
                    Ok(m) => Value::Message(m),
                    Err(e) => {
                        finish_failed(shared, *flow, e);
                        return;
                    }
                };
                RuntimeMetrics::inc(&shared.metrics.messages_sent);
                flow.metrics
                    .messages_sent
                    .fetch_add(1, Ordering::Relaxed);
                log::info(format!(
                    "send from=flow#{} to=flow#{} via=cap#{} msg={}",
                    flow.id.as_u64(),
                    target.as_u64(),
                    target_cap,
                    stamped
                ));
                deliver(shared, local, target, stamped);
            }
            VmResult::Receive {
                dest_reg,
                timeout,
                match_tag,
            } => {
                flow.last_receive_dest = Some(dest_reg);
                let filter = match match_tag {
                    None => WaitFilter::Any,
                    Some(tag) => WaitFilter::Tag(tag),
                };
                match flow.mailbox.try_pop_filter(filter) {
                    Ok(Some(msg)) => {
                        flow.metrics
                            .messages_received
                            .fetch_add(1, Ordering::Relaxed);
                        log::info(format!(
                            "recv flow#{} filter={filter:?} msg={}",
                            flow.id.as_u64(),
                            msg
                        ));
                        let _ = flow.vm.resume_with(dest_reg, msg);
                    }
                    Ok(None) => {
                        log::debug(format!(
                            "park flow#{} waiting mailbox filter={filter:?} timeout={timeout:?}",
                            flow.id.as_u64(),
                        ));
                        park_on_mailbox(shared, flow, dest_reg, timeout, filter);
                        return;
                    }
                    Err(e) => {
                        report_fault(e);
                        return;
                    }
                }
            }
            VmResult::Ask {
                dest_reg,
                target_cap,
                request,
            } => {
                let Some(req_msg) = request.as_message() else {
                    finish_failed(
                        shared,
                        *flow,
                        "Ask invariant broken: request is not Value::Message".into(),
                    );
                    return;
                };
                let target = match resolve_cap(shared, target_cap, CapRights::ASK) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e);
                        return;
                    }
                };
                let stamped_msg = match authenticate_outgoing_message(shared, flow.id, req_msg) {
                    Ok(m) => m,
                    Err(e) => {
                        finish_failed(shared, *flow, e);
                        return;
                    }
                };
                let request_id = stamped_msg.request_id;
                let stamped = Value::Message(stamped_msg);
                // S2: expect FlowId of the Cap target (not CapId).
                let filter = WaitFilter::Correlation {
                    expect_request_id: request_id,
                    expect_sender: Some(target.as_u64()),
                };
                flow.last_receive_dest = Some(dest_reg);
                RuntimeMetrics::inc(&shared.metrics.messages_sent);
                flow.metrics
                    .messages_sent
                    .fetch_add(1, Ordering::Relaxed);
                log::info(format!(
                    "ask from=flow#{} to=flow#{} via=cap#{} req={} wait={filter:?}",
                    flow.id.as_u64(),
                    target.as_u64(),
                    target_cap,
                    stamped
                ));
                // Order: deliver request to *target*, then wait on *our*
                // mailbox. park_filter re-checks under the same mutex if the
                // reply raced ahead (anti lost-wakeup on the caller's inbox).
                deliver(shared, local, target, stamped);
                match flow.mailbox.try_pop_filter(filter) {
                    Ok(Some(reply)) => {
                        flow.metrics
                            .messages_received
                            .fetch_add(1, Ordering::Relaxed);
                        log::info(format!(
                            "ask-reply ready flow#{} msg={}",
                            flow.id.as_u64(),
                            reply
                        ));
                        let _ = flow.vm.resume_with(dest_reg, reply);
                    }
                    Ok(None) => {
                        log::debug(format!(
                            "ask park flow#{} filter={filter:?}",
                            flow.id.as_u64()
                        ));
                        park_on_mailbox(shared, flow, dest_reg, None, filter);
                        return;
                    }
                    Err(e) => {
                        report_fault(e);
                        return;
                    }
                }
            }
        }
    }
}

fn deliver(
    shared: &Arc<Shared>,
    local: &LocalDeque<Box<Flow>>,
    target: FlowId,
    message: Value,
) {
    let mailbox = match shared.directory.lookup(target) {
        Ok(Some(m)) => m,
        Ok(None) => return,
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    match mailbox.push(message.clone()) {
        Ok(Ok(Delivery::Queued | Delivery::QueuedDropOldest)) => {
            log::debug(format!("deliver queued → flow#{target} msg={message}"));
        }
        Ok(Ok(Delivery::DroppedNewest)) => {
            log::debug(format!(
                "deliver drop-newest → flow#{target} msg={message}"
            ));
        }
        Ok(Ok(Delivery::Handoff(mut flow))) => {
            log::debug(format!(
                "deliver handoff → flow#{} msg={}",
                flow.id.as_u64(),
                message
            ));
            if let Some(dest) = flow.last_receive_dest {
                let _ = flow.vm.resume_with(dest, message);
                flow
                    .metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            local.push(flow);
            wake_workers(shared);
        }
        Ok(Err(_)) => {
            log::info(format!(
                "deliver rejected (mailbox full) → flow#{target} msg={message}"
            ));
        }
        Err(e) => report_fault(e),
    }
}

fn park_on_mailbox(
    shared: &Arc<Shared>,
    flow: Box<Flow>,
    dest_reg: u8,
    timeout: Option<Duration>,
    filter: WaitFilter,
) {
    let mailbox = flow.mailbox.clone();
    let pid = flow.id;

    match mailbox.park_filter(flow, filter) {
        Ok(Ok(())) => {
            if let Some(delay) = timeout {
                shared
                    .timer
                    .schedule_receive_timeout(delay, pid, mailbox, dest_reg);
            }
        }
        Ok(Err(mut flow)) => {
            if let Some(msg) = flow.pending_message.take() {
                let _ = flow.vm.resume_with(dest_reg, msg);
                flow.metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            shared.injector.push(flow);
            wake_workers(shared);
        }
        Err(e) => report_fault(e),
    }
}

fn finish_ok(shared: &Shared, flow: Flow, value: Value) {
    finish(shared, flow, FlowOutcome::Completed(value), true);
}

fn finish_failed(shared: &Shared, flow: Flow, msg: String) {
    finish(shared, flow, FlowOutcome::Failed(msg), false);
}

fn finish(shared: &Shared, mut flow: Flow, outcome: FlowOutcome, completed: bool) {
    log::info(format!(
        "finish flow#{} completed={completed} outcome={outcome:?}",
        flow.id.as_u64()
    ));
    if let Err(e) = shared.caps.revoke_target(flow.id) {
        report_fault(e);
    }
    if let Err(e) = shared.directory.unregister(flow.id) {
        report_fault(e);
    }
    if completed {
        RuntimeMetrics::inc(&shared.metrics.processes_completed);
    } else {
        RuntimeMetrics::inc(&shared.metrics.processes_failed);
    }
    if let Some(link) = flow.supervisor.take() {
        link.notify(flow.id, outcome.clone());
    }
    flow.complete(outcome);
}
