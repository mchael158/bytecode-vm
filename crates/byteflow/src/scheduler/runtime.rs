use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::bytecode::{Chunk, Value};
use crate::vm::{NativeTable, Vm};
use crossbeam_deque::{Injector, Stealer, Worker as LocalDeque};

use super::directory::Directory;
use super::error::SpawnError;
use super::handle::FlowHandle;
use super::mailbox::{Delivery, Mailbox};
use super::metrics::{RuntimeMetrics, RuntimeMetricsSnapshot};
use super::process::{Flow, FlowId, RestartPolicy};
use super::supervisor::SupervisorLink;
use super::timer::TimerWheel;
use super::worker;

/// Default instruction budget per scheduling turn (design notes §10).
/// Chosen as a middle ground: large enough that the per-yield bookkeeping
/// cost is amortized over meaningful work, small enough that a
/// pathological `loop {}` in one flow can't visibly stall the others —
/// at 10k simple instructions/turn and even a conservative tens-of-millions
/// of instructions/sec per core, worst-case added latency for a sibling
/// flow is sub-millisecond.
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
    /// Instructions a flow runs before being preempted back to the
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
/// `CapTable`, `TimerWheel`, the `RuntimeMetrics` atomics) or immutable after
/// construction (`stealers`, `quantum`) — there is no top-level lock
/// covering the whole runtime, by design: a global lock is exactly what an
/// M:N scheduler exists to avoid.
pub struct Shared {
    pub(crate) injector: Injector<Box<Flow>>,
    pub(crate) stealers: Vec<Stealer<Box<Flow>>>,
    /// FlowId → mailbox (delivery after Cap resolution).
    pub(crate) directory: Directory,
    /// CapId → { FlowId, rights } (bytecode Send/Ask addressing — FlowCap).
    pub(crate) caps: super::capability::CapTable,
    pub(crate) timer: Arc<TimerWheel>,
    pub(crate) notify: (Mutex<()>, Condvar),
    pub(crate) metrics: RuntimeMetrics,
    pub(crate) shutdown: AtomicBool,
    pub(crate) quantum: u32,
}

/// A running Byteflow runtime: worker pool + timer thread over one shared
/// [`Chunk`].
///
/// Owns M:N scheduling for **flows** (spawn, yield, sleep, mailboxes,
/// FlowCap resolution, supervised restarts). Intentionally *not* here yet:
/// JIT, native quotas, distribution across machines.
///
/// Construct with [`Runtime::new`] (no natives) or
/// [`Runtime::with_natives`] when the chunk uses `CallNative` /
/// [`crate::std_native_table`].
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
    ///
    /// Returns [`SpawnError`] instead of panicking: verify failures and OS
    /// thread-spawn refusals are category-A errors (see
    /// [`super::error`]).
    pub fn new(chunk: Chunk) -> Result<Self, SpawnError> {
        Self::with_config(chunk, RuntimeConfig::default())
    }

    /// Construct a runtime whose flows can call into `natives` via
    /// `Opcode::CallNative` — the host FFI boundary.
    pub fn with_natives(chunk: Chunk, natives: Arc<NativeTable>) -> Result<Self, SpawnError> {
        Self::with_natives_and_config(chunk, natives, RuntimeConfig::default())
    }

    pub fn with_config(chunk: Chunk, config: RuntimeConfig) -> Result<Self, SpawnError> {
        Self::with_natives_and_config(chunk, NativeTable::empty(), config)
    }

    /// Verify `chunk`, spawn the worker pool + timer thread, and return a
    /// live [`Runtime`].
    ///
    /// Failures here mean the runtime was **never** started (no orphan
    /// threads): either the bytecode is invalid
    /// ([`SpawnError::VerifyFailed`]) or the OS refused a thread
    /// ([`SpawnError::ThreadSpawnFailed`]).
    pub fn with_natives_and_config(
        chunk: Chunk,
        natives: Arc<NativeTable>,
        config: RuntimeConfig,
    ) -> Result<Self, SpawnError> {
        crate::bytecode::verify(&chunk).map_err(|e| SpawnError::VerifyFailed(e.to_string()))?;
        let chunk = Arc::new(chunk);
        let workers_n = config.workers.max(1);

        let locals: Vec<LocalDeque<Box<Flow>>> =
            (0..workers_n).map(|_| LocalDeque::new_fifo()).collect();
        let stealers: Vec<Stealer<Box<Flow>>> = locals.iter().map(|l| l.stealer()).collect();

        let shared = Arc::new(Shared {
            injector: Injector::new(),
            stealers,
            directory: Directory::new(),
            caps: super::capability::CapTable::new(),
            timer: TimerWheel::new(),
            notify: (Mutex::new(()), Condvar::new()),
            metrics: RuntimeMetrics::default(),
            shutdown: AtomicBool::new(false),
            quantum: config.quantum,
        });

        let mut workers = Vec::with_capacity(workers_n);
        for local in locals {
            let shared = shared.clone();
            let handle = std::thread::Builder::new()
                .name("byteflow-worker".into())
                .spawn(move || worker::run_worker(shared, local))
                .map_err(|e| SpawnError::ThreadSpawnFailed(e.to_string()))?;
            workers.push(handle);
        }

        let shared_timer = shared.clone();
        let timer_thread = std::thread::Builder::new()
            .name("byteflow-timer".into())
            .spawn(move || {
                shared_timer
                    .timer
                    .clone()
                    .drive(&shared_timer.injector, &shared_timer.notify)
            })
            .map_err(|e| SpawnError::ThreadSpawnFailed(e.to_string()))?;

        Ok(Runtime {
            shared,
            chunk,
            natives,
            workers,
            timer_thread: Some(timer_thread),
        })
    }

    /// Spawn a top-level flow starting at `function` in this runtime's
    /// chunk, returning a [`FlowHandle`] the caller can `.join()`.
    ///
    /// Returns [`SpawnError::BadFunction`] if `function` is out of range.
    pub fn spawn(&self, function: u32, args: &[Value]) -> Result<FlowHandle, SpawnError> {
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
    pub fn supervisor(&self) -> Result<super::supervisor::Supervisor, SpawnError> {
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

    /// Number of flows currently registered in the directory — i.e.
    /// alive (running, ready, sleeping, or waiting), not counting ones that
    /// have already completed or failed.
    pub fn live_flows(&self) -> usize {
        self.shared.directory.len()
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Deliver an **Atomic Hop** (`Value::Message`) to `target` from the
    /// embedder (not from bytecode).
    ///
    /// # Host trust boundary
    ///
    /// This path takes a [`FlowId`] directly — **no Cap required**. The host
    /// is trusted; bytecode must use `Value::Cap` via `Opcode::Send` /
    /// `Ask`. Host-injected messages are not re-stamped (`sender` /
    /// `reply_cap` stay as built). Bare scalars are rejected
    /// ([`SendError::NotAHop`]).
    pub fn send(&self, target: FlowId, message: Value) -> Result<(), SendError> {
        if message.as_message().is_none() {
            return Err(SendError::NotAHop {
                got: message.type_name(),
            });
        }
        let mailbox = match self.shared.directory.lookup(target) {
            Ok(Some(m)) => m,
            Ok(None) => return Err(SendError::NoSuchFlow(target)),
            Err(e) => {
                super::error::report_fault(e);
                return Err(SendError::NoSuchFlow(target));
            }
        };
        match mailbox.push(message.clone()) {
            Ok(Delivery::Queued) => Ok(()),
            Ok(Delivery::Handoff(mut flow)) => {
                if let Some(dest) = flow.last_receive_dest {
                    let _ = flow.vm.resume_with(dest, message);
                }
                self.shared.injector.push(flow);
                wake_workers(&self.shared);
                Ok(())
            }
            Err(e) => {
                super::error::report_fault(e);
                Err(SendError::NoSuchFlow(target))
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
            match super::sync_lock::lock(lock, "Runtime::shutdown") {
                Ok(_g) => cvar.notify_all(),
                Err(e) => super::error::report_fault(e),
            }
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
/// (e.g. round-tripped through `Value::Pid`) and need a [`FlowId`] to
/// call APIs that take one.
pub fn flow_id_from_u64(raw: u64) -> FlowId {
    FlowId(raw)
}

/// Why [`Runtime::send`] could not deliver a hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    NoSuchFlow(FlowId),
    /// Atomic Hop rule: only [`crate::Value::Message`] may cross `Send`.
    NotAHop { got: &'static str },
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NoSuchFlow(id) => write!(f, "no live flow {id}"),
            SendError::NotAHop { got } => {
                write!(f, "atomic hop requires Value::Message, got {got}")
            }
        }
    }
}

impl std::error::Error for SendError {}

/// Shared machinery behind `Runtime::spawn` and `RuntimeSpawner::spawn`
/// (and, transitively, `Supervisor`): build a fresh `Flow` (VM +
/// mailbox + completion channel), register it in the directory, and push
/// it onto the global injector for any worker to pick up.
///
/// Returns [`SpawnError`] on bad function index / VM init / directory
/// poison — never panics. Bytecode `Opcode::Spawn` that fails here turns
/// into `FlowOutcome::Failed` for the *parent* (see `worker`).
pub(crate) fn spawn_on(
    shared: &Arc<Shared>,
    chunk: &Arc<Chunk>,
    natives: &Arc<NativeTable>,
    function: u32,
    args: &[Value],
    restart_policy: RestartPolicy,
    supervisor: Option<SupervisorLink>,
) -> Result<FlowHandle, SpawnError> {
    let id = super::process::next_flow_id();
    let vm = Vm::new(chunk.clone(), natives.clone(), function, args)?;
    let mailbox = Arc::new(Mailbox::new());
    if let Err(e) = shared.directory.register(id, mailbox.clone()) {
        super::error::report_fault(e);
        return Err(SpawnError::VmInit(
            "directory register failed (poisoned lock)".into(),
        ));
    }
    let (tx, rx) = super::oneshot::channel();
    let mut flow = Box::new(Flow::new(id, vm, mailbox, restart_policy, tx));
    flow.supervisor = supervisor;
    RuntimeMetrics::inc(&shared.metrics.processes_spawned);
    shared.injector.push(flow);
    wake_workers(shared);
    Ok(FlowHandle { id, receiver: rx })
}

pub(crate) fn wake_workers(shared: &Shared) {
    let (lock, cvar) = &shared.notify;
    match super::sync_lock::lock(lock, "wake_workers") {
        Ok(_g) => cvar.notify_one(),
        Err(e) => super::error::report_fault(e),
    }
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
    ) -> Result<FlowHandle, SpawnError> {
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
    ) -> Result<FlowHandle, SpawnError> {
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
    use crate::scheduler::FlowOutcome;

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
        )
        .expect("runtime");
        let outcome = rt.spawn(0, &[]).expect("spawn").join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(42)) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}