# byteflow-actors

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](https://github.com/mchael158/bytecode-vm)

**Byteflow** is a small, embeddable actor runtime for Rust: register-based bytecode, lightweight virtual processes, mailboxes, cooperative scheduling, and a one-for-one supervisor — without a separate scripting language.

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
byteflow-actors = "0.2"
```

```rust
use byteflow::{ChunkBuilder, Opcode, ProcessOutcome, Runtime, Value};
```

CLI (same package):

```text
cargo install byteflow-actors
byteflow demo ping-pong
```

---

## Quick start

```rust
use byteflow::{ChunkBuilder, Opcode, ProcessOutcome, Runtime, Value};

fn main() {
    let mut b = ChunkBuilder::new("demo");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 41);
    b.emit_load_imm(1, 1);
    b.emit_binop(Opcode::Add, 0, 0, 1);
    b.emit_return(0);

    let rt = Runtime::new(b.finish());
    let outcome = rt.spawn(0, &[]).join();
    rt.shutdown();

    assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(42))));
}
```

### Std natives (`print`, `now_ms`)

Stable indices: **`print = 0`**, **`now_ms = 1`**.

```rust
use byteflow::{std_native_table, ChunkBuilder, Runtime};

let mut b = ChunkBuilder::new("clock");
b.begin_function("main", 0, 2);
b.emit_load_imm(0, 42);
b.emit_call_native(0, 0, 1); // print(r0)
b.emit_call_native(1, 1, 0); // r1 = now_ms()
b.emit_return(1);

let rt = Runtime::with_natives(b.finish(), std_native_table());
```

Or `std_natives()` → `(NativeTable, HashMap<name, index>)` so host registration stays aligned with bytecode.

### Messaging (ping-pong)

```text
cargo run --example ping_pong
# pong replied 2
```

Built-in samples: `byteflow::samples::{ping_pong, add_forty_two, boom}`.

---

## Architecture

| Layer | Responsibility |
|---|---|
| **Bytecode** | ISA, `ChunkBuilder`, BFV0 (`.bf`) encode/decode, static `verify` |
| **VM** | One process: registers, call stack, cooperative quantum, `CallNative` |
| **Scheduler** | M:N workers, FIFO mailboxes (park/wake), timer, supervisor |
| **Facade** | Public API + std natives + samples + `byteflow` CLI |

**Process lifecycle (sketch):**

1. Worker runs at most `quantum` instructions (default 10 000).  
2. `Yield` / budget → run queue (stealable).  
3. `Sleep` → timer thread → injector.  
4. Empty `Receive` → process parks **inside its mailbox**; the next `Send` wakes under the same lock (no lost wakeup).  
5. `Fault` / `Trap` → `ProcessState::Failed` → supervisor (`Always` / `OnFailure` / `Never`; default intensity 3 / 5s).

`join()` is for the embedder’s native thread only — workers never block on it.

Untrusted `.bf` files go through `Opcode::from_u8` + `verify` before execution.

---

## CLI

```text
byteflow demo [ping-pong|add]
byteflow pack  <demo> <out.bf>
byteflow verify <file.bf>
byteflow disasm <file.bf>
byteflow run    <file.bf> [function]
```

`run` attaches the std native table so modules that `CallNative` indices 0/1 work.

---

## Safety & design notes

- `#![forbid(unsafe_code)]`  
- Process panics are caught at the worker boundary so one bad process cannot kill the OS thread.  
- Native functions must **not block** — they run inline on a worker.  
- Values today: `Unit | Bool | Int | Float | Pid` (no strings yet).

---

## Status (v0.2)

**Included:** register ISA + assembler, BFV0, verifier, per-process VM, M:N scheduler, mailboxes, supervisor, std natives, CLI, examples.

**Not yet:** strings/bytes in `Value`, bounded mailboxes, Criterion benches, timing wheel, JIT, distribution.

---

## Links

- Repository: [github.com/mchael158/bytecode-vm](https://github.com/mchael158/bytecode-vm)  
- Docs: [docs.rs/byteflow-actors](https://docs.rs/byteflow-actors)  
- License: **MIT OR Apache-2.0**
