# byteflow-actors

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](https://github.com/mchael158/bytecode-vm)

**Byteflow** is a small, embeddable **flow** runtime for Rust: register-based bytecode, lightweight flows, **Atomic Hop** messaging (`Value::Message` only on `Send`), cooperative scheduling, and a one-for-one supervisor — without a separate scripting language.

You assemble programs with `ChunkBuilder` in host Rust. The host owns I/O; Byteflow owns cheap concurrency.

> **Package name:** `byteflow-actors` on [crates.io](https://crates.io/crates/byteflow-actors)  
> **Rust import:** `use byteflow::...` (the library crate is named `byteflow`)

---

## Why this exists

Use Byteflow when you need **many isolated units of work** that talk through messages, share a handful of OS threads, and can fail without taking the worker down:

- plugin / rule / workflow engines inside a larger binary  
- simulations and game logic (not the render loop)  
- sandboxed “virtual processes” with a host-defined FFI table  

It is **not** a Tokio replacement, not a distributed cluster, and not a JVM.

---

## Install

```toml
[dependencies]
byteflow-actors = "0.6"
```

```rust
use byteflow::{ChunkBuilder, Opcode, FlowOutcome, Runtime, Value};
```

CLI (same package):

```text
cargo install byteflow-actors
byteflow demo ping-pong
```

---

## Quick start

```rust
use byteflow::{ChunkBuilder, Opcode, FlowOutcome, Runtime, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut b = ChunkBuilder::new("demo");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 41);
    b.emit_load_imm(1, 1);
    b.emit_binop(Opcode::Add, 0, 0, 1);
    b.emit_return(0);

    let rt = Runtime::new(b.finish())?;
    let outcome = rt.spawn(0, &[])?.join();
    rt.shutdown();

    assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
    Ok(())
}
```

### Collecting a result without committing the thread

`join()` blocks until the flow finishes, which is right for a `main` that
has nothing else to do. Anything with a deadline — a control loop, a
watchdog, a test harness — picks its own bound instead:

| Call | Waits | While the flow is still running |
|---|---|---|
| `try_join()` | never | `None` |
| `join_timeout(d)` / `join_deadline(t)` | up to the bound | `None` |
| `join()` | unbounded | (blocks) |

```rust
use byteflow::{ChunkBuilder, FlowOutcome, Runtime, Value};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut b = ChunkBuilder::new("slow");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 300);
    b.emit_sleep(0); // sleeps 300 ms
    b.emit_load_imm(0, 7);
    b.emit_return(0);

    let rt = Runtime::new(b.finish())?;
    let handle = rt.spawn(0, &[])?;

    // Poll for free, or wait under a bound — neither consumes the handle.
    assert!(handle.try_join().is_none());
    assert!(handle.join_timeout(Duration::from_millis(10)).is_none());

    let outcome = handle.join_timeout(Duration::from_secs(10));
    rt.shutdown();

    assert!(matches!(outcome, Some(FlowOutcome::Completed(Value::Int(7)))));
    Ok(())
}
```

The bounds are anchored on an absolute `Instant`, so a spurious condvar
wakeup cannot silently restart the budget. And a flow destroyed before
producing an outcome (`shutdown` does not drain suspended flows) wakes its
joiner with a failure rather than leaving it parked forever — see
[`docs/error-model.md`](docs/error-model.md).

### Std natives (`print`, `now_ms`, `make_msg`, …)

Stable indices: **`print = 0`**, **`now_ms = 1`**, **`make_msg = 2`**, **`msg_*` = 3–6**, **`msg_reply_cap = 7`**.

```rust
use byteflow::{std_native_table, ChunkBuilder, Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut b = ChunkBuilder::new("clock");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 42);
    b.emit_call_native(0, 0, 1); // print(r0)
    b.emit_call_native(1, 1, 0); // r1 = now_ms()
    b.emit_return(1);

    let rt = Runtime::with_natives(b.finish(), std_native_table())?;
    let _ = rt.spawn(0, &[])?.join();
    rt.shutdown();
    Ok(())
}
```

Or `std_natives()` → `(NativeTable, HashMap<name, index>)` so host registration stays aligned with bytecode.

### Messaging (Atomic Hop)

Every `Send` carries one `Value::Message` envelope (scalars trap):

```text
cargo run --example ping_pong
cargo run --example atomic_actors
# optional scheduler logs on stderr:
# BYTEFLOW_LOG=info cargo run --example atomic_actors
```

See [`docs/atomic-hop.md`](docs/atomic-hop.md). Built-in samples:
`byteflow::samples::{ping_pong, atomic_request_reply, add_forty_two, boom}`.

---

## Architecture

| Layer | Responsibility |
|---|---|
| **Bytecode** | ISA, `ChunkBuilder`, BFV0 (`.bf`) encode/decode, static `verify` |
| **VM** | One flow: registers, call stack, cooperative quantum, `CallNative` |
| **Scheduler** | M:N workers, **bounded** FIFO mailboxes (park/wake), timer, supervisor |
| **Facade** | Public API + std natives + samples + `byteflow` CLI |

**Flow lifecycle (sketch):**

1. Worker runs at most `quantum` instructions (default 10 000).  
2. `Yield` / budget → run queue (stealable).  
3. `Sleep` → timer thread → injector.  
4. Empty `Receive` → flow parks **inside its mailbox**; the next Atomic Hop wakes under the same lock (no lost wakeup).  
5. `Fault` / `Trap` → `FlowState::Failed` → supervisor (`Always` / `OnFailure` / `Never`; default intensity 3 / 5s).

`join()` and its bounded forms are for the embedder’s native thread only —
workers never block on them.

Untrusted `.bf` files go through `Opcode::from_u8` + `verify` before
execution. The verifier settles what is a property of the chunk (jump
targets, constant/function indices, `arity ≤ num_registers`); the VM checks
what depends on runtime state (register bounds, index overflow, types,
division, call depth) and turns each into a `Fault` on that one flow — see
[`docs/vm-safety.md`](docs/vm-safety.md).

---

## CLI

```text
byteflow demo [ping-pong|atomic|add]
byteflow pack  <demo> <out.bf>
byteflow verify <file.bf>
byteflow disasm <file.bf>
byteflow run    <file.bf> [function]
```

`run` and hop demos attach the std native table (`print`, `now_ms`, `make_msg`, `msg_*`).

---

## Safety & design notes

- `#![forbid(unsafe_code)]`
- Flow panics are caught at the worker boundary so one bad flow cannot kill the OS thread.
- Malformed bytecode is a `Fault` on one flow, never a panic on the thread that spawned it (see [`docs/vm-safety.md`](docs/vm-safety.md)).
- Register indices are never computed with plain `u8` arithmetic — no “panics in debug, silently wraps in release” divergence.
- Native functions must **not block** — they run inline on a worker.
- Host APIs return `Result` (`SpawnError` / `RuntimeError`) — no `unwrap`/`expect`/`unwrap_or*` anywhere (see [`docs/error-model.md`](docs/error-model.md)).
- Blocking APIs are bounded by choice: `try_join` / `join_timeout` / `join_deadline`, and an abandoned flow wakes its joiner instead of hanging it.
- Values today: `Unit | Bool | Int | Float | Pid | Message | Cap | Str | Bytes`.
- **Atomic Hop:** only `Value::Message` may cross `Send`.
- **FlowCap:** bytecode `Send`/`Ask` targets are `Value::Cap`; replies use `msg_reply_cap`.
- **Security:** authenticated hop sender + FlowCap — see [`docs/security.md`](docs/security.md).

---

## Status (v0.6)

**Included:** register ISA + assembler, BFV0 (ABI v4 / `Message` + `Cap` + `Str`/`Bytes`), verifier, per-flow VM, M:N scheduler, **bounded mailboxes** (`MailboxConfig`: 256 hops + 4 MiB / Reject by default), Atomic Hop, FlowCap, supervisor, std natives, CLI, examples, fail-closed error model.

**Not yet:** `WAITING_SEND` backpressure, runtime-wide resource governor (flow count / spawn rate), Criterion benches, JIT, distribution.

Design guides: [`docs/atomic-hop.md`](docs/atomic-hop.md) ·
[`docs/mailbox.md`](docs/mailbox.md) ·
[`docs/vm-safety.md`](docs/vm-safety.md) ·
[`docs/error-model.md`](docs/error-model.md) ·
[`docs/security.md`](docs/security.md)

See [`CHANGELOG.md`](CHANGELOG.md).

---

## Links

- Repository: [github.com/mchael158/bytecode-vm](https://github.com/mchael158/bytecode-vm)
- Docs: [docs.rs/byteflow-actors](https://docs.rs/byteflow-actors)
- License: **MIT OR Apache-2.0**
