use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::Value;
use crate::log;
use crate::vm::VmResult;
use crossbeam_deque::{Steal, Worker as LocalDeque};

use super::error::report_fault;
use super::mailbox::Delivery;
use super::metrics::RuntimeMetrics;
use super::process::{Process, ProcessId, ProcessOutcome};
use super::runtime::{pid_from_u64, spawn_on, wake_workers, Shared};
use super::sync_lock;

/// Worker main loop. Panics inside a process are caught here so one
/// process's bug cannot take the OS thread down (see `Fault` docs).
pub fn run_worker(shared: Arc<Shared>, local: LocalDeque<Box<Process>>) {
    while !shared.shutdown.load(Ordering::Acquire) {
        match find_work(&shared, &local) {
            Some(process) => drive_process(&shared, &local, process),
            None => wait_for_work(&shared),
        }
    }
}

fn find_work(shared: &Shared, local: &LocalDeque<Box<Process>>) -> Option<Box<Process>> {
    if let Some(process) = local.pop() {
        return Some(process);
    }

    loop {
        match shared.injector.steal() {
            Steal::Success(process) => return Some(process),
            Steal::Empty => break,
            Steal::Retry => continue,
        }
    }

    for stealer in &shared.stealers {
        loop {
            match stealer.steal() {
                Steal::Success(process) => {
                    RuntimeMetrics::inc(&shared.metrics.steals);
                    return Some(process);
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
    local: &LocalDeque<Box<Process>>,
    mut process: Box<Process>,
) {
    if let Some(msg) = process.pending_message.take() {
        if let Some(dest) = process.last_receive_dest {
            let _ = process.vm.resume_with(dest, msg);
            process
                .metrics
                .messages_received
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            local.push(process);
            return;
        }

        let ran = panic::catch_unwind(AssertUnwindSafe(|| process.vm.run(shared.quantum)));
        process
            .metrics
            .instructions
            .store(process.vm.instructions_executed(), Ordering::Relaxed);

        let result = match ran {
            Ok(r) => r,
            Err(_) => {
                finish_failed(shared, *process, "process panicked".into());
                return;
            }
        };

        match result {
            VmResult::Complete(value) => {
                finish_ok(shared, *process, value);
                return;
            }
            VmResult::Trap(fault) => {
                finish_failed(shared, *process, fault.to_string());
                return;
            }
            VmResult::Yield => {
                RuntimeMetrics::inc(&shared.metrics.reschedules);
                process.metrics.reschedules.fetch_add(1, Ordering::Relaxed);
                local.push(process);
                return;
            }
            VmResult::Sleep(delay) => {
                shared.timer.schedule_sleep(delay, process);
                return;
            }
            VmResult::SelfPid { dest_reg } => {
                let _ = process
                    .vm
                    .resume_with(dest_reg, Value::Pid(process.id.as_u64()));
            }
            VmResult::Spawn {
                function,
                args,
                dest_reg,
            } => {
                match spawn_on(
                    shared,
                    &process.vm.chunk_arc(),
                    &process.vm.natives_arc(),
                    function,
                    &args,
                    process.restart_policy,
                    None,
                ) {
                    Ok(child) => {
                        log::info(format!(
                            "spawn parent=pid#{} child=pid#{} fn={}",
                            process.id.as_u64(),
                            child.id().as_u64(),
                            function
                        ));
                        let _ = process
                            .vm
                            .resume_with(dest_reg, Value::Pid(child.id().as_u64()));
                    }
                    Err(e) => {
                        finish_failed(shared, *process, e.to_string());
                        return;
                    }
                }
            }
            VmResult::Send { target, message } => {
                RuntimeMetrics::inc(&shared.metrics.messages_sent);
                process
                    .metrics
                    .messages_sent
                    .fetch_add(1, Ordering::Relaxed);
                log::info(format!(
                    "send from=pid#{} to=pid#{} msg={}",
                    process.id.as_u64(),
                    target,
                    message
                ));
                deliver(shared, local, pid_from_u64(target), message);
            }
            VmResult::Receive { dest_reg, timeout } => {
                process.last_receive_dest = Some(dest_reg);
                match process.mailbox.try_pop() {
                    Ok(Some(msg)) => {
                        process
                            .metrics
                            .messages_received
                            .fetch_add(1, Ordering::Relaxed);
                        log::info(format!(
                            "recv pid#{} msg={}",
                            process.id.as_u64(),
                            msg
                        ));
                        let _ = process.vm.resume_with(dest_reg, msg);
                    }
                    Ok(None) => {
                        log::debug(format!(
                            "park pid#{} waiting mailbox (timeout={:?})",
                            process.id.as_u64(),
                            timeout
                        ));
                        park_on_mailbox(shared, process, dest_reg, timeout);
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
    local: &LocalDeque<Box<Process>>,
    target: ProcessId,
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
        Ok(Delivery::Queued) => {
            log::debug(format!("deliver queued → pid#{target} msg={message}"));
        }
        Ok(Delivery::Handoff(mut process)) => {
            log::debug(format!(
                "deliver handoff → pid#{} msg={}",
                process.id.as_u64(),
                message
            ));
            if let Some(dest) = process.last_receive_dest {
                let _ = process.vm.resume_with(dest, message);
                process
                    .metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            local.push(process);
            wake_workers(shared);
        }
        Err(e) => report_fault(e),
    }
}

fn park_on_mailbox(
    shared: &Arc<Shared>,
    process: Box<Process>,
    dest_reg: u8,
    timeout: Option<Duration>,
) {
    let mailbox = process.mailbox.clone();
    let pid = process.id;

    match mailbox.park(process) {
        Ok(Ok(())) => {
            if let Some(delay) = timeout {
                shared
                    .timer
                    .schedule_receive_timeout(delay, pid, mailbox, dest_reg);
            }
        }
        Ok(Err(mut process)) => {
            if let Some(msg) = process.pending_message.take() {
                let _ = process.vm.resume_with(dest_reg, msg);
                process
                    .metrics
                    .messages_received
                    .fetch_add(1, Ordering::Relaxed);
            }
            shared.injector.push(process);
            wake_workers(shared);
        }
        Err(e) => report_fault(e),
    }
}

fn finish_ok(shared: &Shared, process: Process, value: Value) {
    finish(shared, process, ProcessOutcome::Completed(value), true);
}

fn finish_failed(shared: &Shared, process: Process, msg: String) {
    finish(shared, process, ProcessOutcome::Failed(msg), false);
}

fn finish(shared: &Shared, mut process: Process, outcome: ProcessOutcome, completed: bool) {
    log::info(format!(
        "finish pid#{} completed={completed} outcome={outcome:?}",
        process.id.as_u64()
    ));
    if let Err(e) = shared.directory.unregister(process.id) {
        report_fault(e);
    }
    if completed {
        RuntimeMetrics::inc(&shared.metrics.processes_completed);
    } else {
        RuntimeMetrics::inc(&shared.metrics.processes_failed);
    }
    if let Some(link) = process.supervisor.take() {
        link.notify(process.id, outcome.clone());
    }
    process.complete(outcome);
}
