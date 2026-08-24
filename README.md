# Byteflow

[English](README.md) · [Português (Brasil)](README.pt-BR.md)

A tiny Erlang inside a Rust binary: **register-based bytecode**, **virtual processes**, **mailboxes**, and a **supervisor**. You assemble programs with [`ChunkBuilder`](crates/byteflow-bytecode/src/builder.rs) in Rust — no separate source language. Tens of thousands of actors share a handful of OS threads. One process trapping cannot take a worker down.

```text
cargo run -p byteflow --example ping_pong
# pong replied 2
```

```text
cargo run -p byteflow-cli -- demo ping-pong
# 2
```

## Numbers (measured, not invented)

On this machine — **Intel Core i5-9400F @ 2.90 GHz** (6C/6T), Windows 10 Pro, `rustc 1.97.0`, release build, 6 workers:

```text
cargo run -p byteflow --example throughput --release -- 100000
# processes=100000/100000
# elapsed_ms≈173
# spawns_per_sec≈579000
```

Re-run on your box; quote *your* numbers, not these.

## What it is

| Layer | Crate | Job |
|---|---|---|
| ISA + `.bf` + verifier | `byteflow-bytecode` | Pure data. `ChunkBuilder` is the assembler. |
| Interpreter + FFI table | `byteflow-vm` | One process. `CallNative` → `NativeTable`. |
| Scheduler | `byteflow-scheduler` | M:N workers, mailboxes, timer, supervisor. |
| Facade | `byteflow` | Embed API, std natives, samples. |
| CLI | `byteflow-cli` | `verify` / `disasm` / `run` / `pack` / `demo`. |

It is **not** a cluster, not a Tokio replacement, and not a JVM. Host Rust owns I/O. Byteflow owns cheap concurrency. Logic is written as Rust that emits bytecode — the host language stays Rust.

## 60-second embed (`ChunkBuilder`)

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

## Std natives (`print`, `now_ms`)

Stable indices: **`print = 0`**, **`now_ms = 1`**. Pair the table with `emit_call_native`:

```rust
use byteflow::{std_native_table, ChunkBuilder, ProcessOutcome, Runtime, Value};

let mut b = ChunkBuilder::new("clock");
b.begin_function("main", 0, 2);
b.emit_load_imm(0, 42);
b.emit_call_native(0, 0, 1); // print(r0)
b.emit_call_native(1, 1, 0); // r1 = now_ms()
b.emit_return(1);

let rt = Runtime::with_natives(b.finish(), std_native_table());
```

Or `std_natives()` for `(table, name→index map)` so host code and bytecode stay aligned.

Demos:

```text
cargo run -p byteflow --example ping_pong
cargo run -p byteflow --example crash_and_restart
cargo run -p byteflow --example throughput --release
```

## CLI and the `.bf` format

Modules on disk start with magic `BFV0`. Encode/decode live in `byteflow-bytecode` (the CLI does the `read`/`write`).

```text
cargo run -p byteflow-cli -- pack ping-pong ping.bf
cargo run -p byteflow-cli -- verify ping.bf
cargo run -p byteflow-cli -- disasm ping.bf
cargo run -p byteflow-cli -- run ping.bf main
```

Untrusted files go through `Opcode::from_u8` + `verify` before anything runs. Unknown bytes never become a jump-table index.

## How a process lives

1. A worker runs at most `quantum` instructions (default 10 000).
2. `Yield` / budget → back on a run queue (stealable).
3. `Sleep` → timer thread, then injector.
4. `Receive` on an empty mailbox → the process is **parked inside its own mailbox**. The next `Send` wakes it under the same lock (no lost wakeup).
5. `Fault` / `Trap` → `ProcessState::Failed`, handed to a [`Supervisor`](crates/byteflow-scheduler/src/supervisor.rs) (`Always` / `OnFailure` / `Never`). Intensity: 3 restarts / 5s by default.

`join()` is for the embedder's native thread only. Workers never block on it.

## Project layout

```text
crates/byteflow-bytecode   ISA, ChunkBuilder, BFV0, verifier
crates/byteflow-vm         per-process interpreter + NativeTable
crates/byteflow-scheduler  processes, mailboxes, runtime, supervisor
crates/byteflow            public facade + samples + std natives
crates/byteflow-cli        `byteflow` binary
```

## Status (v0)

**Done**

- Register ISA, assembler (`ChunkBuilder`), verifier
- `.bf` (BFV0) encode/decode + CLI (`verify` / `disasm` / `run` / `pack`)
- Per-process VM with cooperative quantum
- M:N scheduler (work-stealing), mailboxes, timer heap, supervisor
- Real FFI: `CallNative` → `NativeTable` / std natives (`print`, `now_ms`)
- Spawn with args, `SelfPid`, ping-pong + crash-restart + throughput examples

**Not done / next**

- Strings / bytes in `Value`
- Bounded mailboxes + backpressure
- Message-throughput benches (Criterion / CI)
- Timing wheel (still a binary heap), JIT, distribution, capabilities
- More std natives; capability-scoped FFI

License: MIT OR Apache-2.0.
