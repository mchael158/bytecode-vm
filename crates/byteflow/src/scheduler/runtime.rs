use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::bytecode::{Chunk, Value};
use crate::vm::{NativeTable, Vm};
use crossbeam_deque::{Injector, Stealer, Worker as LocalDeque};

use super::directory::Directory;
use super::handle::ProcessHandle;
use super::mailbox::{Delivery, Mailbox};
use super::metrics::{RuntimeMetrics, RuntimeMetricsSnapshot};
use super::process::{Process, ProcessId, RestartPolicy};
use super::supervisor::SupervisorLink;
use super::timer::TimerWheel;
use super::sync_lock;
use super::worker;

/// Default instruction budget per scheduling turn (design notes §10).
/// Chosen as a middle ground: large enough that the per-yield bookkeeping
/// cost is amortized over meaningful work, small enough that a
/// pathological `loop {}` in one process can't visibly stall the others —
/// at 10k simple instructions/turn and even a conservative tens-of-millions
/// of instructions/sec per core, worst-case added latency for a sibling
/// process is sub-millisecond.
pub const DEFAULT_QUANTUM: u32 = 10_000;

/// Tunables for [`Runtime::new`]. Everything has a sensible default via
/// [`RuntimeConfig::default`] so the common case is `Runtime::new(chunk)`.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Number of worker OS threads. Defaults to the number of logical CPUs
    /// — one worker per core is the right starting point for a CPU-bound
    /// M:N scheduler; embedders running alongside other CPU-heavy work on
    /// the same machine may want fewer.
    pub workers: usize,
    /// Instructions a process runs before being preempted back to the
    /// scheduler even if it never hits `Yield`.
    pub quantum: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig { workers: num_cpus::get().max(1), quantum: DEFAULT_QUANTUM }
    }
}

/// State shared by every worker thread and the timer thread. Everything in
/// here is either internally synchronized (`Injector`, `Directory`,
/// `TimerWheel`, the `RuntimeMetrics` atomics) or immutable after
/// construction (`stealers`, `quantum`) — there is no top-level lock
/// covering the whole runtime, by design: a global lock is exactly what an
/// M:N scheduler exists to avoid.
pub struct Shared {
    pub(crate) injector: Injector<Box<Process>>,
    pub(crate) stealers: Vec<Stealer<Box<Process>>>,
    pub(crate) directory: Directory,
    pub(crate) timer: Arc<TimerWheel>,
    pub(crate) notify: (Mutex<()>, Condvar),
    pub(crate) metrics: RuntimeMetrics,
    pub(crate) shutdown: AtomicBool,
    pub(crate) quantum: u32,
}

/// A running Byteflow runtime: a fixed pool of worker threads plus one
/// timer thread, all operating on processes compiled from a single shared
/// [`Chunk`] (design notes' Phase 1-3 milestone: VM + M:N scheduler +
/// spawn/yield/sleep/mailboxes — see the crate-level docs for what's
/// intentionally *not* here yet: JIT, FFI, capabilities, distribution).
pub struct Runtime {
    shared: Arc<Shared>,
    chunk: Arc<Chunk>,
    natives: Arc<NativeTable>,
    workers: Vec<JoinHandle<()>>,
    timer_thread: Option<JoinHandle<()>>,
}

impl Runtime {
    /// Convenience constructor for chunks that never call out through
    /// `Opcode::CallNative`. Equivalent to
    /// `Runtime::with_natives(chunk, NativeTable::empty())`.
    pub fn new(chunk: Chunk) -> Self {
        Self::with_config(chunk, RuntimeConfig::default())
    }

    /// Construct a runtime whose processes can call into `natives` via
    /// `Opcode::CallNative` — the real FFI boundary (design notes §30-31).
    pub fn with_natives(chunk: Chunk, natives: Arc<NativeTable>) -> Self {
        Self::with_natives_and_config(chunk, natives, RuntimeConfig::default())
    }

    pub fn with_config(chunk: Chunk, config: RuntimeConfig) -> Self {
        Self::with_natives_and_config(chunk, NativeTable::empty(), config)
    }

    pub fn with_natives_and_config(
        chunk: Chunk,
        natives: Arc<NativeTable>,
        config: RuntimeConfig,
    ) -> Self {
        crate::bytecode::verify(&chunk).expect(
            "Runtime::new requires a verified chunk; call crate::bytecode::verify() yourself \
             first if you want to handle a verification failure gracefully instead of panicking",
        );
        let chunk = Arc::new(chunk);
        let workers_n = config.workers.max(1);

        let locals: Vec<LocalDeque<Box<Process>>> = (0..workers_n).map(|_| LocalDeque::new_fifo()).collect();
        let stealers: Vec<Stealer<Box<Process>>> = locals.iter().map(|l| l.stealer()).collect();

        let shared = Arc::new(Shared {
            injector: Injector::new(),
            stealers,
            directory: Directory::new(),
            timer: TimerWheel::new(),
            notify: (Mutex::new(()), Condvar::new()),
            metrics: RuntimeMetrics::default(),
            shutdown: AtomicBool::new(false),
            quantum: config.quantum,
        });

        let mut workers = Vec::with_capacity(workers_n);
        for local in locals {
            let shared = shared.clone();
            workers.push(std::thread::Builder::new()
                .name("byteflow-worker".into())
                .spawn(move || worker::run_worker(shared, local))
                .expect("failed to spawn byteflow worker thread"));
        }

        let timer_thread = {
            let shared = shared.clone();
            Some(
                std::thread::Builder::new()
                    .name("byteflow-timer".into())
                    .spawn(move || shared.timer.clone().drive(&shared.injector, &shared.notify))
                    .expect("failed to spawn byteflow timer thread"),
            )
        };

        Runtime { shared, chunk, natives, workers, timer_thread }
    }

    /// Spawn a top-level process starting at `function` in this runtime's
    /// chunk, returning a [`ProcessHandle`] the caller can `.join()`.
    ///
    /// Panics if `function` is out of range for the chunk — this mirrors
    /// `Opcode::Spawn`'s own behavior of trusting a chunk that already
    /// passed [`crate::bytecode::verify`] (`Runtime::new` already ran it
    /// once for the whole chunk; a bad top-level `function` index here is a
    /// caller bug, not a runtime fault to recover from).
    pub fn spawn(&self, function: u32, args: &[Value]) -> ProcessHandle {
        spawn_on(
            &self.shared,
            &self.chunk,
            &self.natives,
            function,
            args,
            RestartPolicy::Never,
            None,
        )
    }

    /// A cheap, `Send + Sync` handle that can spawn processes into this
    /// runtime from any thread, independent of `Runtime`'s own lifetime
    /// bookkeeping (worker `JoinHandle`s). Used by [`super::supervisor::Supervisor`].
    pub fn spawner(&self) -> RuntimeSpawner {
        RuntimeSpawner { shared: self.shared.clone(), chunk: self.chunk.clone(), natives: self.natives.clone() }
    }

    /// A [`super::supervisor::Supervisor`] bound to this runtime, ready to
    /// take supervised children (design notes §15).
    pub fn supervisor(&self) -> super::supervisor::Supervisor {
        super::supervisor::Supervisor::new(self.spawner())
    }

    /// Look up a function by name in the runtime's chunk — convenience for
    /// callers that built their chunk with [`crate::bytecode::ChunkBuilder`]
    /// and don't want to thread raw indices through their own code.
    pub fn function_index(&self, name: &str) -> Option<u32> {
        self.chunk.functions.iter().position(|f| f.name == name).map(|i| i as u32)
    }

    pub fn metrics(&self) -> RuntimeMetricsSnapshot {
        self.shared.metrics.snapshot()
    }

    /// Number of processes currently registered in the directory — i.e.
    /// alive (running, ready, sleeping, or waiting), not counting ones that
    /// have already completed or failed.
    pub fn live_processes(&self) -> usize {
        self.shared.directory.len()
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Deliver `message` to `target` from the embedder (not from bytecode).
    pub fn send(&self, target: ProcessId, message: Value) -> Result<(), SendError> {
        let Some(mailbox) = self.shared.directory.lookup(target) else {
            return Err(SendError::NoSuchProcess(target));
        };
        match mailbox.push(message.clone()) {
            Delivery::Queued => Ok(()),
            Delivery::Handoff(mut process) => {
                if let Some(dest) = process.last_receive_dest {
                    let _ = process.vm.resume_with(dest, message);
                }
                self.shared.injector.push(process);
                wake_workers(&self.shared);
                Ok(())
            }
        }
    }

    /// Stop accepting new scheduling work and join every worker + the timer
    /// thread. Processes that are mid-quantum are allowed to reach their
    /// next natural suspension point; this does **not** forcibly abort
    /// running bytecode (there is no safe way to do that to an OS thread
    /// mid-instruction — see design notes §11 on why preemption here is
    /// cooperative/budgeted rather than signal-based).
    pub fn shutdown(mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.timer.shutdown();
        {
            let (lock, cvar) = &self.shared.notify;
            let _g = sync_lock::lock(lock);
            cvar.notify_all();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if let Some(t) = self.timer_thread.take() {
            let _ = t.join();
        }
    }
}

/// A convenience Pid constructor for embedders that stored a raw `u64`
/// (e.g. round-tripped through `Value::Pid`) and need a [`ProcessId`] to
/// call APIs that take one.
pub fn pid_from_u64(raw: u64) -> ProcessId {
    ProcessId(raw)
}

/// Why [`Runtime::send`] could not deliver a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    NoSuchProcess(ProcessId),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NoSuchProcess(id) => write!(f, "no live process {id}"),
        }
    }
}

impl std::error::Error for SendError {}

/// Shared machinery behind `Runtime::spawn` and `RuntimeSpawner::spawn`
/// (and, transitively, `Supervisor`): build a fresh `Process` (VM +
/// mailbox + completion channel), register it in the directory, and push
/// it onto the global injector for any worker to pick up.
pub(crate) fn spawn_on(
    shared: &Arc<Shared>,
    chunk: &Arc<Chunk>,
    natives: &Arc<NativeTable>,
    function: u32,
    args: &[Value],
    restart_policy: RestartPolicy,
    supervisor: Option<SupervisorLink>,
) -> ProcessHandle {
    let id = super::process::next_process_id();
    let vm = Vm::new(chunk.clone(), natives.clone(), function, args)
        .expect("spawn: function index out of range for this runtime's chunk");
    let mailbox = Arc::new(Mailbox::new());
    shared.directory.register(id, mailbox.clone());
    let (tx, rx) = super::oneshot::channel();
    let mut process = Box::new(Process::new(id, vm, mailbox, restart_policy, tx));
    process.supervisor = supervisor;
    RuntimeMetrics::inc(&shared.metrics.processes_spawned);
    shared.injector.push(process);
    wake_workers(shared);
    ProcessHandle { id, receiver: rx }
}

pub(crate) fn wake_workers(shared: &Shared) {
    let (lock, cvar) = &shared.notify;
    let _g = super::sync_lock::lock(lock);
    cvar.notify_one();
}

/// A `Send + Sync`, freely cloneable capability to spawn processes into a
/// [`Runtime`], detached from the `Runtime` value itself. Exists because
/// [`super::supervisor::Supervisor`] needs to respawn processes from a
/// background monitor thread whose lifetime isn't tied to the `Runtime`
/// object's own (which owns non-`Sync` `JoinHandle`s for its workers).
#[derive(Clone)]
pub struct RuntimeSpawner {
    pub(crate) shared: Arc<Shared>,
    pub(crate) chunk: Arc<Chunk>,
    pub(crate) natives: Arc<NativeTable>,
}

impl RuntimeSpawner {
    pub fn spawn(
        &self,
        function: u32,
        args: &[Value],
        restart_policy: RestartPolicy,
    ) -> ProcessHandle {
        spawn_on(
            &self.shared,
            &self.chunk,
            &self.natives,
            function,
            args,
            restart_policy,
            None,
        )
    }

    pub(crate) fn spawn_linked(
        &self,
        function: u32,
        args: &[Value],
        restart_policy: RestartPolicy,
        supervisor: SupervisorLink,
    ) -> ProcessHandle {
        spawn_on(
            &self.shared,
            &self.chunk,
            &self.natives,
            function,
            args,
            restart_policy,
            Some(supervisor),
        )
    }

    pub fn metrics(&self) -> RuntimeMetricsSnapshot {
        self.shared.metrics.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{ChunkBuilder, Opcode, Value};
    use crate::scheduler::process::ProcessOutcome;

    fn add_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 41);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);
        b.finish()
    }

    #[test]
    fn spawn_and_join_add() {
        let rt = Runtime::with_config(
            add_chunk(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
            },
        );
        let outcome = rt.spawn(0, &[]).join();
        rt.shutdown();
        match outcome {
            ProcessOutcome::Completed(Value::Int(42)) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}