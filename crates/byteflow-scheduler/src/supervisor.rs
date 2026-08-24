use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use byteflow_bytecode::Value;

use crate::handle::ProcessHandle;
use crate::process::{ProcessId, ProcessOutcome, RestartPolicy};
use crate::runtime::RuntimeSpawner;

/// How many restarts OTP-style supervisors allow inside a sliding window
/// before giving up (design notes §15). Three-in-five-seconds is the
/// classic default: enough to absorb a flaky child, tight enough that a
/// crash loop cannot spin the runtime forever.
const DEFAULT_MAX_RESTARTS: u32 = 3;
const DEFAULT_MAX_PERIOD: Duration = Duration::from_secs(5);

/// A child the supervisor should start (and possibly restart).
///
/// `function` is an index into the runtime's chunk — the same number
/// [`crate::runtime::Runtime::spawn`] takes. Args are cloned on every
/// restart so a child always comes back with the original call.
#[derive(Clone, Debug)]
pub struct ChildSpec {
    pub name: String,
    pub function: u32,
    pub args: Vec<Value>,
    pub restart: RestartPolicy,
}

impl ChildSpec {
    pub fn new(name: impl Into<String>, function: u32) -> Self {
        ChildSpec {
            name: name.into(),
            function,
            args: Vec::new(),
            restart: RestartPolicy::OnFailure,
        }
    }

    pub fn args(mut self, args: Vec<Value>) -> Self {
        self.args = args;
        self
    }

    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }
}

/// Tunables for [`Supervisor::with_config`].
///
/// Unlike a full OTP supervisor this does **not** implement one-for-all /
/// rest-for-one: those strategies require aborting sibling processes, and
/// Byteflow's preemption is cooperative (see [`crate::runtime::Runtime::shutdown`]).
/// v0 is one-for-one — only the child that exited is considered for restart.
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Restarts allowed inside [`Self::max_period`]. The initial start does
    /// not count; only respawns do. Hitting this cap sets
    /// [`Supervisor::intensity_exceeded`] and further restarts are refused.
    pub max_restarts: u32,
    pub max_period: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            max_restarts: DEFAULT_MAX_RESTARTS,
            max_period: DEFAULT_MAX_PERIOD,
        }
    }
}

struct ChildExit {
    id: ProcessId,
    outcome: ProcessOutcome,
}

struct LiveChild {
    spec: ChildSpec,
}

struct Inner {
    spawner: RuntimeSpawner,
    config: SupervisorConfig,
    events: Mutex<VecDeque<ChildExit>>,
    cvar: Condvar,
    children: Mutex<HashMap<ProcessId, LiveChild>>,
    restart_times: Mutex<VecDeque<Instant>>,
    intensity_exceeded: AtomicBool,
    shutdown: AtomicBool,
}

/// Cheap, `Clone` handle the worker uses to hand a terminal outcome back
/// without taking a lock on the supervisor's child table (the drive loop
/// is the only writer of that table).
#[derive(Clone)]
pub(crate) struct SupervisorLink {
    inner: Arc<Inner>,
}

impl SupervisorLink {
    pub(crate) fn notify(&self, id: ProcessId, outcome: ProcessOutcome) {
        let mut events = self.inner.events.lock().unwrap_or_else(|e| e.into_inner());
        events.push_back(ChildExit { id, outcome });
        self.inner.cvar.notify_one();
    }
}

/// Host-side child restarter (design notes §15-16).
///
/// A `Supervisor` is **not** a bytecode process. It is a dedicated OS
/// thread plus a table of [`ChildSpec`]s. When a supervised process
/// becomes `ProcessState::Failed` (or completes, under
/// [`RestartPolicy::Always`]), the worker delivers the
/// [`ProcessOutcome`] here instead of letting the fault take anything
/// else down. The supervisor then consults the child's
/// [`RestartPolicy`] and, if intensity allows, respawns it under a
/// fresh [`ProcessId`] — Pids are never reused (see
/// [`crate::process::ProcessId`]).
///
/// Constructed from a [`RuntimeSpawner`] so it does not have to own the
/// runtime's worker `JoinHandle`s.
pub struct Supervisor {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub fn new(spawner: RuntimeSpawner) -> Self {
        Self::with_config(spawner, SupervisorConfig::default())
    }

    pub fn with_config(spawner: RuntimeSpawner, config: SupervisorConfig) -> Self {
        let inner = Arc::new(Inner {
            spawner,
            config,
            events: Mutex::new(VecDeque::new()),
            cvar: Condvar::new(),
            children: Mutex::new(HashMap::new()),
            restart_times: Mutex::new(VecDeque::new()),
            intensity_exceeded: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        });
        let drive_inner = inner.clone();
        let thread = std::thread::Builder::new()
            .name("byteflow-supervisor".into())
            .spawn(move || drive(drive_inner))
            .expect("failed to spawn byteflow supervisor thread");
        Supervisor {
            inner,
            thread: Some(thread),
        }
    }

    /// Spawn `spec` and start supervising it. The returned handle is for
    /// this incarnation only — a restart allocates a new Pid and a new
    /// completion channel.
    pub fn start_child(&self, spec: ChildSpec) -> ProcessHandle {
        spawn_child(&self.inner, spec)
    }

    pub fn live_children(&self) -> usize {
        self.inner
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// `true` once more than [`SupervisorConfig::max_restarts`] respawns
    /// landed inside the intensity window. Remaining children keep
    /// running; we just stop bringing them back (no safe abort of a
    /// mid-quantum process).
    pub fn intensity_exceeded(&self) -> bool {
        self.inner.intensity_exceeded.load(Ordering::Acquire)
    }

    /// Stop the drive thread. Does not terminate live children — they
    /// belong to the runtime, not to us.
    pub fn shutdown(mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.cvar.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn spawn_child(inner: &Arc<Inner>, spec: ChildSpec) -> ProcessHandle {
    let link = SupervisorLink {
        inner: inner.clone(),
    };
    // Hold the table across spawn so a child that faults in its first
    // quantum cannot notify us before its row exists (the drive loop
    // takes this same lock in `handle_exit`, so the event waits).
    let mut children = inner.children.lock().unwrap_or_else(|e| e.into_inner());
    let handle = inner
        .spawner
        .spawn_linked(spec.function, &spec.args, spec.restart, link);
    children.insert(handle.id(), LiveChild { spec });
    handle
}

fn should_restart(policy: RestartPolicy, outcome: &ProcessOutcome) -> bool {
    match policy {
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => matches!(outcome, ProcessOutcome::Failed(_)),
        RestartPolicy::Never => false,
    }
}

fn intensity_hit(inner: &Inner) -> bool {
    let now = Instant::now();
    let mut times = inner.restart_times.lock().unwrap_or_else(|e| e.into_inner());
    times.push_back(now);
    let window_start = now.checked_sub(inner.config.max_period).unwrap_or(now);
    while times.front().map(|t| *t < window_start).unwrap_or(false) {
        times.pop_front();
    }
    if times.len() as u32 > inner.config.max_restarts {
        inner.intensity_exceeded.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

fn drive(inner: Arc<Inner>) {
    loop {
        if inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        let exit = {
            let mut events = inner.events.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if inner.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if let Some(exit) = events.pop_front() {
                    break exit;
                }
                let (guard, _) = inner
                    .cvar
                    .wait_timeout(events, Duration::from_millis(100))
                    .unwrap_or_else(|e| e.into_inner());
                events = guard;
            }
        };
        handle_exit(&inner, exit);
    }
}

fn handle_exit(inner: &Arc<Inner>, exit: ChildExit) {
    let spec = {
        let mut children = inner.children.lock().unwrap_or_else(|e| e.into_inner());
        match children.remove(&exit.id) {
            Some(live) => live.spec,
            None => return,
        }
    };

    if !should_restart(spec.restart, &exit.outcome) {
        return;
    }
    if inner.intensity_exceeded.load(Ordering::Acquire) || intensity_hit(inner) {
        return;
    }

    let _ = spawn_child(inner, spec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use byteflow_bytecode::{Chunk, ChunkBuilder, Value};

    use crate::runtime::{Runtime, RuntimeConfig};

    fn trap_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("trap");
        b.begin_function("boom", 0, 1);
        b.emit_trap(1);
        b.finish()
    }

    fn ok_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("ok");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 7);
        b.emit_return(0);
        b.finish()
    }

    fn tiny_runtime(chunk: Chunk) -> Runtime {
        Runtime::with_config(
            chunk,
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
            },
        )
    }

    fn wait_until(mut pred: impl FnMut() -> bool) {
        let start = Instant::now();
        while !pred() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "supervisor test timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn on_failure_does_not_restart_a_clean_exit() {
        let rt = tiny_runtime(ok_chunk());
        let sup = Supervisor::new(rt.spawner());
        let outcome = sup
            .start_child(ChildSpec::new("main", 0).restart(RestartPolicy::OnFailure))
            .join();
        wait_until(|| sup.live_children() == 0);
        let spawned = rt.metrics().processes_spawned;
        sup.shutdown();
        rt.shutdown();
        assert!(matches!(outcome, ProcessOutcome::Completed(_)));
        assert_eq!(spawned, 1);
    }

    #[test]
    fn on_failure_restarts_until_intensity() {
        let rt = tiny_runtime(trap_chunk());
        let sup = Supervisor::with_config(
            rt.spawner(),
            SupervisorConfig {
                max_restarts: 2,
                max_period: Duration::from_secs(5),
            },
        );
        let _first = sup.start_child(ChildSpec::new("boom", 0).restart(RestartPolicy::OnFailure));
        wait_until(|| sup.intensity_exceeded() && rt.metrics().processes_failed >= 3);
        let spawned = rt.metrics().processes_spawned;
        let failed = rt.metrics().processes_failed;
        sup.shutdown();
        rt.shutdown();
        // initial start + 2 restarts, then intensity refuses the 3rd restart
        assert_eq!(spawned, 3);
        assert_eq!(failed, 3);
    }

    #[test]
    fn always_restarts_a_clean_exit_until_intensity() {
        let rt = tiny_runtime(ok_chunk());
        let sup = Supervisor::with_config(
            rt.spawner(),
            SupervisorConfig {
                max_restarts: 2,
                max_period: Duration::from_secs(5),
            },
        );
        let _ = sup.start_child(ChildSpec::new("main", 0).restart(RestartPolicy::Always));
        wait_until(|| sup.intensity_exceeded() && rt.metrics().processes_completed >= 3);
        let spawned = rt.metrics().processes_spawned;
        sup.shutdown();
        rt.shutdown();
        assert_eq!(spawned, 3);
    }

    #[test]
    fn policy_table() {
        let ok = ProcessOutcome::Completed(Value::Unit);
        let fail = ProcessOutcome::Failed("boom".into());
        assert!(should_restart(RestartPolicy::Always, &ok));
        assert!(should_restart(RestartPolicy::Always, &fail));
        assert!(!should_restart(RestartPolicy::OnFailure, &ok));
        assert!(should_restart(RestartPolicy::OnFailure, &fail));
        assert!(!should_restart(RestartPolicy::Never, &ok));
        assert!(!should_restart(RestartPolicy::Never, &fail));
    }
}
