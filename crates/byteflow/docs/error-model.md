# Error model (engineering rules)

**Panic is a runtime bug, not an error-handling mechanism.**

## Taxonomy

| Kind | Example | Surface |
|------|---------|---------|
| **A — user / API** | `spawn` bad function index | `Result<T, SpawnError>` |
| **B — flow** | native fault, bad Atomic Hop | `FlowOutcome::Failed` → Supervisor |
| **C — infrastructure** | mutex poison | `RuntimeError` + fail-closed (`report_fault`) |
| **D — invariant** | empty VM frame stack | types / `debug_assert` — never `unwrap` to hide design |

## Mutex policy

Never:

```rust
lock.lock().unwrap()
lock.lock().unwrap_or_else(|e| e.into_inner())
```

Use [`sync_lock::lock`](../src/scheduler/sync_lock.rs) → `Result<_, RuntimeError::PoisonedLock>`.
In worker loops: `report_fault` + `return`. At API boundaries: propagate `Result`.

## Clippy (core crate)

```toml
unwrap_used = "deny"
expect_used = "deny"
```

Tests are allowed unwrap/expect via `cfg_attr(test, allow(...))` on the lib crate.

## Host API (category A)

```rust
let rt = Runtime::new(chunk)?;                         // SpawnError::VerifyFailed | ThreadSpawnFailed
let handle = rt.spawn(fn_idx, &args)?;                 // SpawnError::BadFunction | …
let sup = Supervisor::new(rt.spawner())?;
let child = sup.start_child(spec)?;
```

Never panic on bad function index, failed verify, or OS thread spawn failure.
