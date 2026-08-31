use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::bytecode::{Chunk, Value};
use crate::vm::{NativeTable, Vm};
use crossbeam_deque::{Injector, Stealer, Worker as LocalDeque};

use super::directory::Directory;
use super::error::SpawnError;
use super::handle::FlowHandle;
use super::mailbox::{Delivery, Mailbox, MailboxConfig, MailboxFullReason};
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
    /// Memory + overflow contract applied to **every** flow mailbox
    /// spawned by this runtime (bytecode `Spawn` and host `spawn`).
    /// See [`MailboxConfig`] / `docs/mailbox.md`.
    pub mailbox: MailboxConfig,
    /// Hard cap on concurrently live flows (`0` = unlimited).
    /// Checked on every host and bytecode `spawn`.
    pub max_flows: u32,
    /// Trace JIT settings (`feature = "jit"`). Ignored when the feature is off.
    #[cfg(feature = "jit")]
    pub jit: JitConfig,
}

/// Trace JIT toggles for [`RuntimeConfig`] (`feature = "jit"`).
#[cfg(feature = "jit")]
#[derive(Clone, Debug)]
pub struct JitConfig {
    /// When true, workers attempt compiled traces before interpreting.
    pub enabled: bool,
    /// How many times a `(function, pc)` pair must run before compilation.
    pub hot_threshold: u32,
}

#[cfg(feature = "jit")]
impl Default for JitConfig {
    fn default() -> Self {
        JitConfig {
            enabled: false,
            hot_threshold: crate::jit::HOT_THRESHOLD,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            workers: num_cpus::get().max(1),
            quantum: DEFAULT_QUANTUM,
            mailbox: MailboxConfig::DEFAULT,
            max_flows: 0,
            #[cfg(feature = "jit")]
            jit: JitConfig::default(),
        }
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
    pub(crate) mailbox: MailboxConfig,
    pub(crate) max_flows: u32,
    pub(crate) monitors: super::monitor::MonitorStore,
    pub(crate) links: super::link::LinkStore,
    pub(crate) registry: super::registry::RegistryStore,
    pub(crate) kill_signals: super::finalize::KillSignals,
    pub(crate) waiting_send_at: super::finalize::WaitingSendIndex,
    pub(crate) ask_waits: super::finalize::AskWaitIndex,
    /// Shared trace JIT state (`feature = "jit"`).
    #[cfg(feature = "jit")]
    pub(crate) jit: Option<std::sync::Arc<crate::jit::JitRuntime>>,
}

/// A running Byteflow runtime: worker pool + timer thread over one shared
/// [`Chunk`].
///
/// Owns M:N scheduling for **flows** (spawn, yield, sleep, mailboxes,
/// FlowCap resolution, supervised restarts). Optional trace JIT when built
/// with `feature = "jit"` and enabled in [`RuntimeConfig::jit`].
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
    /// [`docs::error_model`](crate::docs::error_model)).
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

        #[cfg(feature = "jit")]
        let jit = if config.jit.enabled {
            Some(super::jit::new_runtime(chunk.clone(), config.jit.hot_threshold))
        } else {
            None
        };

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
            mailbox: config.mailbox,
            max_flows: config.max_flows,
            monitors: super::monitor::MonitorStore::new(),
            links: super::link::LinkStore::new(),
            registry: super::registry::RegistryStore::new(),
            kill_signals: super::finalize::KillSignals::new(),
            waiting_send_at: super::finalize::WaitingSendIndex::new(),
            ask_waits: super::finalize::AskWaitIndex::new(),
            #[cfg(feature = "jit")]
            jit,
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
                    .drive(
                        &shared_timer.injector,
                        &shared_timer.notify,
                        &shared_timer.ask_waits,
                    )
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
    /// callers that built their chunk with [`crate::Program`]
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
            Ok(Ok(Delivery::Queued | Delivery::QueuedDropOldest | Delivery::DroppedNewest)) => {
                Ok(())
            }
            Ok(Ok(Delivery::Handoff(mut flow))) => {
                let _ = self.shared.ask_waits.remove_asker(flow.id);
                if let Some(dest) = flow.last_receive_dest {
                    let _ = flow.vm.resume_with(dest, message);
                }
                self.shared.injector.push(flow);
                wake_workers(&self.shared);
                Ok(())
            }
            Ok(Err(full)) => Err(SendError::MailboxFull {
                flow: target,
                reason: full.reason(),
            }),
            Err(e) => {
                super::error::report_fault(e);
                Err(SendError::NoSuchFlow(target))
            }
        }
    }

    fn require_live(&self, id: FlowId) -> Result<(), super::error::LifecycleError> {
        match self.shared.directory.lookup(id) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(super::error::LifecycleError::NoSuchFlow(id)),
            Err(e) => Err(self.unavailable(e)),
        }
    }

    fn unavailable(&self, err: super::error::RuntimeError) -> super::error::LifecycleError {
        super::error::report_fault(err);
        super::error::LifecycleError::Unavailable
    }

    /// Mint a SEND|ASK Cap for a live flow (host equivalent of `SelfPid`).
    pub fn mint_cap(&self, flow: FlowId) -> Result<super::capability::CapId, super::error::LifecycleError> {
        self.require_live(flow)?;
        self.shared
            .caps
            .mint(flow, super::capability::CapRights::SEND_ASK)
            .map_err(|e| self.unavailable(e))
    }

    /// Watch `target`; when it exits, `owner` receives a [`crate::TAG_SYS_DOWN`] hop.
    ///
    /// Both flows must be live. `owner == target` is [`LifecycleError::SelfRelation`].
    pub fn monitor(
        &self,
        owner: FlowId,
        target: FlowId,
    ) -> Result<super::monitor::MonitorRef, super::error::LifecycleError> {
        if owner == target {
            return Err(super::error::LifecycleError::SelfRelation);
        }
        self.require_live(owner)?;
        self.require_live(target)?;
        let mon = self
            .shared
            .monitors
            .create(owner, target)
            .map_err(|e| self.unavailable(e))?;
        // Target may have finalized between the live check and insert.
        // Synthesize DOWN and drop the now-useless relation (owner still live).
        if self.require_live(target).is_err() {
            let _ = self.shared.monitors.remove_owned(owner, mon);
            super::finalize::deliver_down(
                &self.shared,
                super::monitor::DownEvent {
                    monitor: mon,
                    owner,
                    target,
                    reason: super::monitor::FlowExitReason::Fault,
                },
            );
        }
        Ok(mon)
    }

    /// Drop `monitor` if `owner` still owns it.
    pub fn demonitor(
        &self,
        owner: FlowId,
        monitor: super::monitor::MonitorRef,
    ) -> Result<(), super::error::LifecycleError> {
        self.require_live(owner)?;
        match self.shared.monitors.remove_owned(owner, monitor) {
            Ok(inner) => inner,
            Err(e) => Err(self.unavailable(e)),
        }
    }

    /// Bidirectional link. Abnormal exit of either side kills the peer.
    pub fn link(
        &self,
        a: FlowId,
        b: FlowId,
    ) -> Result<super::link::LinkId, super::error::LifecycleError> {
        if a == b {
            return Err(super::error::LifecycleError::SelfRelation);
        }
        self.require_live(a)?;
        self.require_live(b)?;
        match self.shared.links.link(a, b) {
            Ok(inner) => inner,
            Err(e) => Err(self.unavailable(e)),
        }
    }

    /// Drop `link` if `owner` is one of the endpoints.
    pub fn unlink(
        &self,
        owner: FlowId,
        link: super::link::LinkId,
    ) -> Result<(), super::error::LifecycleError> {
        self.require_live(owner)?;
        match self.shared.links.unlink_owned(owner, link) {
            Ok(inner) => inner,
            Err(e) => Err(self.unavailable(e)),
        }
    }

    /// Bind `name` to a live Cap (address). Names are swept when that flow exits.
    pub fn register_name(
        &self,
        name: &str,
        cap: super::capability::CapId,
    ) -> Result<(), super::error::LifecycleError> {
        let entry = match self.shared.caps.resolve(cap) {
            Ok(Some(e)) => e,
            Ok(None) => return Err(super::error::LifecycleError::InvalidCapability),
            Err(e) => return Err(self.unavailable(e)),
        };
        self.require_live(entry.flow)?;
        match self.shared.registry.register(
            super::registry::RegistryName::from(name),
            cap,
            entry.flow,
        ) {
            Ok(inner) => inner,
            Err(e) => Err(self.unavailable(e)),
        }
    }

    /// Look up a registered Cap, or `None` if the name is free / was swept.
    pub fn whereis(&self, name: &str) -> Result<Option<super::capability::CapId>, super::error::LifecycleError> {
        self.shared
            .registry
            .whereis(name)
            .map_err(|e| self.unavailable(e))
    }

    /// Cooperative abort. Parked flows finalize immediately; a running flow
    /// dies at the next quantum with [`super::monitor::FlowExitReason::Killed`].
    pub fn kill(&self, id: FlowId) -> Result<(), super::error::LifecycleError> {
        self.require_live(id)?;
        super::finalize::request_kill(&self.shared, id, super::monitor::FlowExitReason::Killed);
        Ok(())
    }

    /// Remove a name without waiting for the flow to exit. `false` if unknown.
    pub fn unregister_name(&self, name: &str) -> Result<bool, super::error::LifecycleError> {
        self.shared
            .registry
            .unregister(name)
            .map_err(|e| self.unavailable(e))
    }

    /// Replace the image used by **new host** [`Self::spawn`] calls and
    /// drop JIT traces. Live flows and bytecode `Spawn` keep the parent's
    /// existing `Vm` chunk.
    pub fn reload_chunk(&mut self, chunk: Chunk) -> Result<(), SpawnError> {
        crate::bytecode::verify(&chunk).map_err(|e| SpawnError::VerifyFailed(e.to_string()))?;
        let chunk = Arc::new(chunk);
        self.chunk = chunk.clone();
        #[cfg(feature = "jit")]
        if let Some(jit) = &self.shared.jit {
            jit.reload(chunk);
        }
        Ok(())
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
    /// Target inbox is at one of its logical bounds
    /// ([`OverflowPolicy::Reject`](crate::OverflowPolicy::Reject)). `reason` says which — see
    /// [`MailboxFullReason`].
    MailboxFull {
        flow: FlowId,
        reason: MailboxFullReason,
    },
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NoSuchFlow(id) => write!(f, "no live flow {id}"),
            SendError::NotAHop { got } => {
                write!(f, "atomic hop requires Value::Message, got {got}")
            }
            SendError::MailboxFull { flow, reason } => {
                write!(f, "mailbox full for {flow} ({reason})")
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
    if shared.max_flows > 0 {
        let current = shared.directory.len();
        if current >= shared.max_flows as usize {
            return Err(SpawnError::FlowLimit {
                current,
                max: shared.max_flows,
            });
        }
    }
    let id = super::process::next_flow_id();
    let vm = Vm::new(chunk.clone(), natives.clone(), function, args)?;
    let mailbox = Arc::new(Mailbox::with_config(shared.mailbox));
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

    pub(crate) fn request_kill(&self, id: FlowId, reason: super::monitor::FlowExitReason) {
        super::finalize::request_kill(&self.shared, id, reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{builder::ChunkBuilder, Opcode, Value};
    use crate::scheduler::FlowOutcome;
    use std::time::Duration;

    fn add_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 41);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);
        b.finish()
    }

    /// Sleeps `millis` inside the flow, then returns 7. The sleep is what
    /// makes "still running" an observable state from the host thread.
    fn sleep_then_return_chunk(millis: i32) -> Chunk {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, millis);
        b.emit_sleep(0);
        b.emit_load_imm(0, 7);
        b.emit_return(0);
        b.finish()
    }

    /// The host thread must be able to ask "done yet?" and to wait under a
    /// bound *it* chooses, instead of surrendering itself to `join` for
    /// however long the bytecode decides to take.
    #[test]
    fn polling_and_bounded_waits_never_commit_the_host_thread() -> Result<(), Box<dyn std::error::Error>>
    {
        const FLOW_SLEEP: i32 = 150;
        let rt = Runtime::with_config(
            sleep_then_return_chunk(FLOW_SLEEP),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                ..Default::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;

        // The flow cannot possibly be finished yet: it has to be picked up
        // and then sleep. A poll must say so without waiting.
        if let Some(outcome) = handle.try_join() {
            rt.shutdown();
            return Err(format!("try_join answered too early: {outcome:?}").into());
        }

        // A bound well below the flow's sleep must expire and hand control
        // back, not block until the flow happens to finish.
        if let Some(outcome) = handle.join_timeout(Duration::from_millis(20)) {
            rt.shutdown();
            return Err(format!("join_timeout answered too early: {outcome:?}").into());
        }

        // A generous bound collects the real outcome through the same
        // (non-consuming) handle.
        let outcome = handle.join_timeout(Duration::from_secs(10));
        rt.shutdown();
        match outcome {
            Some(FlowOutcome::Completed(Value::Int(7))) => Ok(()),
            other => Err(format!("unexpected outcome: {other:?}").into()),
        }
    }

    #[test]
    fn spawn_and_join_add() -> Result<(), Box<dyn std::error::Error>> {
        let rt = Runtime::with_config(
            add_chunk(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                ..Default::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(42)) => Ok(()),
            other => Err(format!("unexpected outcome: {other:?}").into()),
        }
    }

    fn receive_forever_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("recv");
        b.begin_function("main", 0, 1);
        b.emit_receive(0);
        b.emit_return(0);
        b.finish()
    }

    #[test]
    fn kill_parked_flow_joins_failed() -> Result<(), Box<dyn std::error::Error>> {
        let rt = Runtime::with_config(
            receive_forever_chunk(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                ..Default::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        rt.kill(handle.id())?;
        let outcome = handle.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "kill must fail the joiner, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn max_flows_rejects_extra_spawn() -> Result<(), Box<dyn std::error::Error>> {
        let rt = Runtime::with_config(
            receive_forever_chunk(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                max_flows: 1,
                ..Default::default()
            },
        )?;
        let first = rt.spawn(0, &[])?;
        let second = rt.spawn(0, &[]);
        rt.kill(first.id())?;
        let _ = first.join();
        rt.shutdown();
        match second {
            Err(SpawnError::FlowLimit { current, max }) => {
                assert_eq!(current, 1);
                assert_eq!(max, 1);
                Ok(())
            }
            other => Err(format!(
                "expected FlowLimit, got {}",
                match &other {
                    Ok(_) => "Ok(handle)".into(),
                    Err(e) => format!("Err({e})"),
                }
            )
            .into()),
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn runtime_with_jit_enabled_completes_add() -> Result<(), Box<dyn std::error::Error>> {
        use crate::JitConfig;

        let rt = Runtime::with_config(
            add_chunk(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                jit: JitConfig {
                    enabled: true,
                    hot_threshold: 1,
                },
                ..Default::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(42)) => Ok(()),
            other => Err(format!("unexpected outcome: {other:?}").into()),
        }
    }
}