use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use byteflow_bytecode::Value;
use byteflow_vm::VmResult;
use crossbeam_deque::{Steal, Worker as LocalDeque};

use crate::mailbox::Delivery;
use crate::metrics::RuntimeMetrics;
use crate::process::{Process, ProcessId, ProcessOutcome};
use crate::runtime::{pid_from_u64, spawn_on, wake_workers, Shared};

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
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if shared.shutdown.load(Ordering::Acquire) {
        return;
    }
    let _ = cvar.wait_timeout(guard, Duration::from_millis(50));
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
                let child = spawn_on(
                    shared,
                    &process.vm.chunk_arc(),
                    &process.vm.natives_arc(),
                    function,
                    &args,
                    process.restart_policy,
                    None,
                );
                let _ = process
                    .vm
                    .resume_with(dest_reg, Value::Pid(child.id().as_u64()));
            }
            VmResult::Send { target, message } => {
                RuntimeMetrics::inc(&shared.metrics.messages_sent);
                process
                    .metrics
                    .messages_sent
                    .fetch_add(1, Ordering::Relaxed);
                deliver(shared, local, pid_from_u64(target), message);
            }
            VmResult::Receive { dest_reg, timeout } => {
                process.last_receive_dest = Some(dest_reg);
                if let Some(msg) = process.mailbox.try_pop() {
                    process
                        .metrics
                        .messages_received
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = process.vm.resume_with(dest_reg, msg);
                } else {
                    park_on_mailbox(shared, process, dest_reg, timeout);
                    return;
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
    let Some(mailbox) = shared.directory.lookup(target) else {
        return;
    };
    match mailbox.push(message.clone()) {
        Delivery::Queued => {}
        Delivery::Handoff(mut process) => {
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
        Ok(()) => {
            if let Some(delay) = timeout {
                shared
                    .timer
                    .schedule_receive_timeout(delay, pid, mailbox, dest_reg);
            }
        }
        Err(mut process) => {
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
    }
}

fn finish_ok(shared: &Shared, process: Process, value: Value) {
    finish(shared, process, ProcessOutcome::Completed(value), true);
}

fn finish_failed(shared: &Shared, process: Process, msg: String) {
    finish(shared, process, ProcessOutcome::Failed(msg), false);
}

fn finish(shared: &Shared, mut process: Process, outcome: ProcessOutcome, completed: bool) {
    shared.directory.unregister(process.id);
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
