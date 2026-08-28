# Error model (engineering rules)

**Panic is a runtime bug, not an error-handling mechanism.**

## Taxonomy

| Kind | Example | Surface |
|------|---------|---------|
| **A — user / API** | `spawn` bad function index | `Result<T, SpawnError>` |
| **B — flow** | native fault, bad Atomic Hop | `FlowOutcome::Failed` → Supervisor |
| **C — infrastructure** | mutex poison, abandoned flow | `RuntimeError` + fail-closed (`report_fault`) |
| **D — invariant** | empty VM frame stack | types / `debug_assert` — never `unwrap` to hide design |

## Mutex policy

Never (production):

```text
.lock().unwrap()
.unwrap_or(...)
.unwrap_or_else(...)
.lock().unwrap_or_else(|e| e.into_inner())
```

Use [`sync_lock::lock`](../src/scheduler/sync_lock.rs) → `Result<_, RuntimeError::PoisonedLock>`.
In worker loops: `report_fault` + `return`. At API boundaries: propagate `Result`.

## Category A — user / API, by example

A bad function index, a chunk that fails verification, or an OS that refuses
a thread are all things the embedder can act on. None of them panic:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{ChunkBuilder, Runtime, SpawnError};

let mut b = ChunkBuilder::new("demo");
b.begin_function("main", 0, 1);
b.emit_return(0);
let rt = Runtime::new(b.finish())?;

// The chunk has exactly one function, so index 99 is a caller mistake.
let result = rt.spawn(99, &[]);
rt.shutdown();

match result {
    Err(SpawnError::BadFunction { .. }) => {}
    Err(e) => return Err(format!("unexpected error: {e}").into()),
    Ok(_) => return Err("function index 99 does not exist".into()),
}
# Ok(())
# }
```

## Category B — flow faults

A fault inside bytecode fails **that flow** and reaches its supervisor;
the worker thread survives. See
[`docs::vm_safety`](crate::docs::vm_safety) for the fault taxonomy and a
`Trap` example.

## Fail-closed covers liveness, not only correctness

A blocking API that can wait on an event which will never happen is as
broken as one that returns a wrong answer — and harder to diagnose, since
"hung" looks identical to "slow".

There are two distinct questions, and a runtime owes an answer to both.

### "The outcome can never arrive"

`Runtime::shutdown` does not drain flows out of the timer or the worker
deques — they are destroyed where they sit, without producing an outcome.
Dropping a completion sender without sending is therefore a first-class
event: it wakes the joiner with
[`RuntimeError::Abandoned`](crate::RuntimeError::Abandoned) instead of
leaving it on a condvar nobody will notify again.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{ChunkBuilder, FlowOutcome, Runtime};
use std::time::Duration;

// A flow that sleeps for a minute.
let mut b = ChunkBuilder::new("sleeper");
b.begin_function("main", 0, 2);
b.emit_load_imm(0, 60_000);
b.emit_sleep(0);
b.emit_return(0);

let rt = Runtime::new(b.finish())?;
let handle = rt.spawn(0, &[])?;
// Give it time to reach the Sleep and park in the timer.
std::thread::sleep(Duration::from_millis(150));

// Shutdown destroys it where it sits: no outcome will ever be produced.
rt.shutdown();

// `join` reports that instead of blocking this thread forever.
match handle.join() {
    FlowOutcome::Failed(why) => assert!(why.contains("destroyed")),
    FlowOutcome::Completed(_) => return Err("a 60 s sleep cannot have finished".into()),
}
# Ok(())
# }
```

That verdict is **stable**: polling an abandoned handle again reports the
same thing. Only a successful outcome is a one-shot value.

### "The outcome has not arrived *yet*"

Here the honest answer is a bound the caller chooses:

| Call | Waits | Answer while the flow is still running |
|------|-------|----------------------------------------|
| `try_join` | never | `None` |
| `join_timeout(d)` / `join_deadline(t)` | up to the bound | `None` |
| `join` | unbounded | (blocks) |

`join` remains right for a `main` with nothing else to do. Anything with a
deadline — control loop, watchdog, driver, test harness — uses a bounded
form. Those take `&self`, so an expired bound leaves the handle usable:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{ChunkBuilder, FlowOutcome, Runtime, Value};
use std::time::Duration;

// A flow that sleeps 300 ms, then returns 7.
let mut b = ChunkBuilder::new("slow");
b.begin_function("main", 0, 2);
b.emit_load_imm(0, 300);
b.emit_sleep(0);
b.emit_load_imm(0, 7);
b.emit_return(0);

let rt = Runtime::new(b.finish())?;
let handle = rt.spawn(0, &[])?;

// Asking costs no waiting at all, and it cannot be finished yet.
assert!(handle.try_join().is_none());

// A bound far below the flow's own sleep expires and hands control back,
// rather than blocking until the flow happens to finish.
assert!(handle.join_timeout(Duration::from_millis(10)).is_none());

// Neither call consumed the handle, so the outcome is still collectable.
let outcome = handle.join_timeout(Duration::from_secs(10));
rt.shutdown();
assert!(matches!(outcome, Some(FlowOutcome::Completed(Value::Int(7)))));

// The outcome is handed out once. A later poll says so, instead of
// repeating it or blaming the runtime for destroying the flow.
match handle.try_join() {
    Some(FlowOutcome::Failed(why)) => assert!(why.contains("already collected")),
    _ => return Err("a collected outcome must not be handed out twice".into()),
}
# Ok(())
# }
```

The bound is anchored on an absolute `Instant`, never a duration re-fed into
the condvar loop. A condvar wait must sit in a loop because it can return
spuriously, and restarting the full budget on every spurious wakeup produces
a "timeout" with no upper bound — a failure that is invisible in testing and
unbounded in production. `oneshot` has a regression test that drives a storm
of spurious wakeups through the wait to pin this.

### Still open

Bytecode `Ask` parks with no deadline and is not notified when its target
dies, so it can wait forever. Same rule, not yet applied.

Calling `join` from inside a worker (from a native function, say) blocks
that worker on a flow that may need that very worker to progress. The rule
is documented but not yet machine-checked.

## Clippy (core crate)

```toml
unwrap_used = "deny"
expect_used = "deny"
```

Tests also return `Result` and use `?` — no `unwrap` / `expect` / `unwrap_or*` anywhere in the crate.

## Host API (category A)

```text
let rt = Runtime::new(chunk)?;                         // SpawnError::VerifyFailed | ThreadSpawnFailed
let handle = rt.spawn(fn_idx, &args)?;                 // SpawnError::BadFunction | …
let sup = Supervisor::new(rt.spawner())?;
let child = sup.start_child(spec)?;
```

Never panic on bad function index, failed verify, or OS thread spawn failure.
