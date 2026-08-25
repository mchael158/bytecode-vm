# Atomic actors (Byteflow core)

Handoff context for humans / other AIs working on **this repo only** (`byteflow-actors`).
Hardware (`byteflow-hw`) was removed from the monorepo — do not restore it here.

## What “atomic” means here

One mailbox delivery carries a full [`Message`](../src/bytecode/value.rs) envelope:

```text
Message { sender, request_id, tag, payload }
```

- **sender** — who to reply to (`Pid` as `u64`)
- **request_id** — client correlation token (echoed on reply)
- **tag** — protocol discriminator (opaque to the VM)
- **payload** — small `u64` body

No multi-message handshake (unlike scalar `ping_pong`). Mailbox park/push share one mutex → no lost-wakeup (`scheduler/mailbox.rs`).

## Make / unpack (std natives)

Stable indices in [`natives.rs`](../src/natives.rs):

| Idx | Name | Args → result |
|----:|------|----------------|
| 0 | `print` | values… → `Unit` (actor-visible log) |
| 1 | `now_ms` | → `Int` |
| 2 | `make_msg` | sender, request_id, tag, payload → `Message` |
| 3 | `msg_sender` | msg → `Pid` |
| 4 | `msg_request_id` | msg → `Int` |
| 5 | `msg_tag` | msg → `Int` |
| 6 | `msg_payload` | msg → `Int` |

Runtime must use `Runtime::with_natives(chunk, std_native_table())` (or `with_natives_and_config`).

## Sample + refresh

```text
# unit + sample tests
cargo test -p byteflow-actors

# demo (stdout = print native; stderr = BYTEFLOW_LOG)
cargo run -p byteflow-actors --example atomic_actors

# Windows PowerShell — scheduler logs
$env:BYTEFLOW_LOG="info"
cargo run -p byteflow-actors --example atomic_actors

$env:BYTEFLOW_LOG="debug"
cargo run -p byteflow-actors --example atomic_actors
```

Expected: process joins with `Value::Int(42)`; metrics show ≥1 message sent.

Chunk: [`samples::atomic_request_reply`](../src/samples.rs) (`TAG_REQ=1`, `TAG_REP=2`).

## Scheduler logs

[`crate::log`](../src/log.rs) → stderr when `BYTEFLOW_LOG` is set:

- `info` — spawn, send, recv, finish
- `debug` — park, deliver queued/handoff

## Minimal host sketch

```rust
use byteflow::{samples, std_native_table, ProcessOutcome, Runtime, RuntimeConfig, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::with_natives_and_config(
        samples::atomic_request_reply(),
        std_native_table(),
        RuntimeConfig { workers: 2, quantum: 10_000 },
    )?;
    let outcome = rt.spawn(rt.function_index("main").ok_or("main")?, &[])?.join();
    rt.shutdown();
    assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(42))));
    Ok(())
}
```

## Do not

- Add hardware crates back into this workspace
- Strip explanatory comments in `mailbox` / `directory` / `sync_lock`
- Use `PoisonError::into_inner()` — fail-closed via `RuntimeError`
- Block inside natives (runs on worker thread)

## Key paths

| Path | Role |
|------|------|
| `src/bytecode/value.rs` | `Message` / `Value::Message` |
| `src/natives.rs` | make/unpack + print |
| `src/samples.rs` | `atomic_request_reply` |
| `src/scheduler/mailbox.rs` | atomic park + push |
| `src/scheduler/worker.rs` | deliver / park / logs |
| `src/log.rs` | `BYTEFLOW_LOG` |
| `examples/atomic_actors.rs` | runnable demo |
| `docs/error-model.md` | A/B/C/D taxonomy |
