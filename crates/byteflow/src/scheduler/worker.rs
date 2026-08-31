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
use super::finalize::finalize_flow;
use super::link::LinkId;
use super::mailbox::{Delivery, ParkSender, WaitFilter};
use super::metrics::RuntimeMetrics;
use super::monitor::{FlowExitReason, MonitorRef};
use super::process::{Flow, FlowId, FlowOutcome, PendingSend};
use super::runtime::{flow_id_from_u64, spawn_on, wake_workers, Shared};
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
fn resolve_cap_any(shared: &Shared, raw: u64) -> Result<FlowId, String> {
    let id = CapId(raw);
    match shared.caps.resolve(id) {
        Ok(Some(entry)) => Ok(entry.flow),
        Ok(None) => Err(format!("unknown or revoked capability {id}")),
        Err(e) => Err(e.to_string()),
    }
}

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

    match shared.kill_signals.take(flow.id) {
        Ok(Some(reason)) => {
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed(format!("linked exit ({reason})")),
                reason,
            );
            return;
        }
        Ok(None) => {}
        Err(e) => {
            report_fault(e);
            return;
        }
    }

    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            local.push(flow);
            return;
        }

        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            #[cfg(feature = "jit")]
            {
                super::jit::run_flow_quantum(&mut flow, shared)
            }
            #[cfg(not(feature = "jit"))]
            {
                flow.vm.run(shared.quantum)
            }
        }));
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
                match deliver(shared, local, target, stamped.clone()) {
                    DeliverStatus::Ok => {}
                    DeliverStatus::Full => {
                        park_waiting_send(
                            shared,
                            flow,
                            target,
                            stamped,
                            PendingSend::FireAndForget,
                        );
                        return;
                    }
                }
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
                        admit_waiting_on(&flow.mailbox, shared, local);
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
                timeout,
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
                match deliver(shared, local, target, stamped.clone()) {
                    DeliverStatus::Ok => {}
                    DeliverStatus::Full => {
                        park_waiting_send(
                            shared,
                            flow,
                            target,
                            stamped,
                            PendingSend::Ask {
                                dest_reg,
                                expect_request_id: request_id,
                                expect_sender: target.as_u64(),
                                timeout,
                            },
                        );
                        return;
                    }
                }
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
                        park_ask(shared, flow, dest_reg, timeout, filter, target);
                        return;
                    }
                    Err(e) => {
                        report_fault(e);
                        return;
                    }
                }
            }
            VmResult::Monitor {
                dest_reg,
                target_cap,
            } => {
                let target = match resolve_cap_any(shared, target_cap) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e);
                        return;
                    }
                };
                if target == flow.id {
                    finish_failed(shared, *flow, "cannot monitor self".into());
                    return;
                }
                match shared.monitors.create(flow.id, target) {
                    Ok(mon) => {
                        let ref_i = i64::try_from(mon.as_u64()).unwrap_or(i64::MAX);
                        let _ = flow.vm.resume_with(dest_reg, Value::Int(ref_i));
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Demonitor { monitor_reg } => {
                let raw = match flow.vm.top_registers().and_then(|r| r.get(monitor_reg as usize)) {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => {
                        finish_failed(shared, *flow, "demonitor: expected Int ref".into());
                        return;
                    }
                };
                match shared.monitors.remove_owned(flow.id, MonitorRef::from_u64(raw)) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Link {
                dest_reg,
                target_cap,
            } => {
                let target = match resolve_cap_any(shared, target_cap) {
                    Ok(id) => id,
                    Err(e) => {
                        finish_failed(shared, *flow, e);
                        return;
                    }
                };
                if target == flow.id {
                    finish_failed(shared, *flow, "cannot link self".into());
                    return;
                }
                match shared.links.link(flow.id, target) {
                    Ok(Ok(id)) => {
                        let ref_i = i64::try_from(id.as_u64()).unwrap_or(i64::MAX);
                        let _ = flow.vm.resume_with(dest_reg, Value::Int(ref_i));
                    }
                    Ok(Err(e)) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Unlink { link_reg } => {
                let raw = match flow.vm.top_registers().and_then(|r| r.get(link_reg as usize)) {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => {
                        finish_failed(shared, *flow, "unlink: expected Int id".into());
                        return;
                    }
                };
                match shared.links.unlink_owned(flow.id, LinkId::from_u64(raw)) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                    Err(e) => {
                        finish_failed(shared, *flow, e.to_string());
                        return;
                    }
                }
            }
        }
    }
}

enum DeliverStatus {
    Ok,
    Full,
}

fn admit_waiting_on(
    mailbox: &std::sync::Arc<super::mailbox::Mailbox>,
    shared: &Arc<Shared>,
    local: &LocalDeque<Box<Flow>>,
) {
    match mailbox.admit_waiting_sender() {
        Ok(Some(mut sender)) => {
            if let Err(e) = shared.waiting_send_at.remove(sender.id) {
                report_fault(e);
            }
            match sender.pending_send.take() {
                Some(PendingSend::Ask {
                    dest_reg,
                    expect_request_id,
                    expect_sender,
                    timeout,
                }) => {
                    let filter = WaitFilter::Correlation {
                        expect_request_id,
                        expect_sender: Some(expect_sender),
                    };
                    park_ask(
                        shared,
                        sender,
                        dest_reg,
                        timeout,
                        filter,
                        flow_id_from_u64(expect_sender),
                    );
                }
                _ => {
                    local.push(sender);
                    wake_workers(shared);
                }
            }
        }
        Ok(None) => {}
        Err(e) => report_fault(e),
    }
}

fn deliver(
    shared: &Arc<Shared>,
    local: &LocalDeque<Box<Flow>>,
    target: FlowId,
    message: Value,
) -> DeliverStatus {
    let mailbox = match shared.directory.lookup(target) {
        Ok(Some(m)) => m,
        Ok(None) => return DeliverStatus::Ok,
        Err(e) => {
            report_fault(e);
            return DeliverStatus::Ok;
        }
    };
    match mailbox.push(message.clone()) {
        Ok(Ok(Delivery::Queued | Delivery::QueuedDropOldest)) => {
            log::debug(format!("deliver queued → flow#{target} msg={message}"));
            DeliverStatus::Ok
        }
        Ok(Ok(Delivery::DroppedNewest)) => {
            log::debug(format!(
                "deliver drop-newest → flow#{target} msg={message}"
            ));
            DeliverStatus::Ok
        }
        Ok(Ok(Delivery::Handoff(mut flow))) => {
            log::debug(format!(
                "deliver handoff → flow#{} msg={}",
                flow.id.as_u64(),
                message
            ));
            if let Err(e) = shared.ask_waits.remove_asker(flow.id) {
                report_fault(e);
            }
            if let Some(dest) = flow.last_receive_dest {
                let _ = flow.vm.resume_with(dest, message);
                flow
                    .metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            local.push(flow);
            wake_workers(shared);
            DeliverStatus::Ok
        }
        Ok(Err(full)) => {
            let reason = full.reason();
            log::info(format!(
                "deliver rejected (mailbox full: {reason}) → flow#{target} msg={message}"
            ));
            DeliverStatus::Full
        }
        Err(e) => {
            report_fault(e);
            DeliverStatus::Ok
        }
    }
}

fn park_ask(
    shared: &Arc<Shared>,
    flow: Box<Flow>,
    dest_reg: u8,
    timeout: Option<Duration>,
    filter: WaitFilter,
    target: FlowId,
) {
    let asker = flow.id;
    if let Err(e) = shared.ask_waits.insert(asker, target) {
        report_fault(e);
        finish_failed(
            shared,
            *flow,
            "ask-wait index".into(),
        );
        return;
    }
    let mailbox = flow.mailbox.clone();
    match mailbox.park_filter(flow, filter) {
        Ok(Ok(epoch)) => {
            if !matches!(shared.directory.lookup(target), Ok(Some(_))) {
                if let Ok(Some(mut parked)) = mailbox.take_parked() {
                    let _ = shared.ask_waits.remove_asker(asker);
                    let hop = Value::Message(Message::linked_exit(
                        target.as_u64(),
                        FlowExitReason::Fault.as_u64(),
                    ));
                    let _ = parked.vm.resume_with(dest_reg, hop);
                    parked
                        .metrics
                        .messages_received
                        .fetch_add(1, Ordering::Relaxed);
                    shared.injector.push(parked);
                    wake_workers(shared);
                }
                return;
            }
            if let Some(delay) = timeout {
                shared
                    .timer
                    .schedule_receive_timeout(delay, asker, mailbox, dest_reg, epoch);
            }
        }
        Ok(Err(mut flow)) => {
            let _ = shared.ask_waits.remove_asker(asker);
            if let Some(msg) = flow.pending_message.take() {
                let _ = flow.vm.resume_with(dest_reg, msg);
                flow.metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            shared.injector.push(flow);
            wake_workers(shared);
        }
        Err(e) => {
            let _ = shared.ask_waits.remove_asker(asker);
            report_fault(e);
        }
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
        Ok(Ok(epoch)) => {
            if let Some(delay) = timeout {
                shared
                    .timer
                    .schedule_receive_timeout(delay, pid, mailbox, dest_reg, epoch);
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
    finalize_flow(
        shared,
        flow,
        FlowOutcome::Completed(value),
        FlowExitReason::Normal,
    );
}

fn finish_failed(shared: &Shared, flow: Flow, msg: String) {
    finalize_flow(shared, flow, FlowOutcome::Failed(msg), FlowExitReason::Fault);
}

fn park_waiting_send(
    shared: &Shared,
    mut flow: Box<Flow>,
    target: FlowId,
    stamped: Value,
    pending: PendingSend,
) {
    flow.pending_send = Some(pending);
    if let Err(e) = shared.waiting_send_at.insert(flow.id, target) {
        report_fault(e);
        finalize_flow(
            shared,
            *flow,
            FlowOutcome::Failed("waiting-send index".into()),
            FlowExitReason::Fault,
        );
        return;
    }
    let Some(mailbox) = (match shared.directory.lookup(target) {
        Ok(m) => m,
        Err(e) => {
            report_fault(e);
            let _ = shared.waiting_send_at.remove(flow.id);
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed("send target gone".into()),
                FlowExitReason::Fault,
            );
            return;
        }
    }) else {
        let _ = shared.waiting_send_at.remove(flow.id);
        finalize_flow(
            shared,
            *flow,
            FlowOutcome::Failed("send target gone".into()),
            FlowExitReason::Fault,
        );
        return;
    };
    match mailbox.park_sender(flow, stamped) {
        ParkSender::Parked => {}
        ParkSender::Closed(flow) => {
            let _ = shared.waiting_send_at.remove(flow.id);
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed("send target gone".into()),
                FlowExitReason::Fault,
            );
        }
    }
}
