# Atomic Hop (Byteflow core)

Handoff context for humans / other AIs working on **this repo only** (`byteflow-actors`).
Hardware (`byteflow-hw`) was removed from the monorepo — do not restore it here.

## Units: **flows**

Byteflow's concurrent unit is a **flow** (`Flow`, `FlowId`, `FlowHandle`, `FlowOutcome`) — not an “actor” API surface.

Wire identity still uses `Value::Pid` (FlowId as `u64`) inside messages.
**Addressing** for bytecode `Send` / `Ask` uses `Value::Cap` (FlowCap, ABI v4).
Scalars include `Value::Str` / `Value::Bytes` in the constant pool; hops remain
`Message`-only.

## What “Atomic Hop” means

Every `Send` (bytecode or `Runtime::send`) must carry a full [`Message`](../src/bytecode/value.rs) envelope:

```text
Message { sender, reply_cap, request_id, tag, payload }
```

- **sender** — authenticated origin FlowId (stamped by the worker)
- **reply_cap** — SEND-only Cap back to the sender (minted at stamp time)
- **request_id** — client correlation token (echoed on reply)
- **tag** — protocol discriminator (opaque to the VM)
- **payload** — small `u64` body

**Authenticated sender (security S1):** bytecode `Send` / `Ask` overwrite
`Message.sender` and attach `reply_cap` before delivery.
The `make_msg` sender argument is untrusted metadata — see
[`security.md`](security.md).

**FlowCap (security S6):** bytecode `Send` / `Ask` targets must be `Value::Cap`.
`SelfPid` / `Spawn` return Caps. Reply with `msg_reply_cap`, not `msg_sender`.

Bare scalars (`Int`, `Pid`, …) on `Send` → VM trap / `SendError::NotAHop`.  
Mailbox park/push share one mutex → no lost-wakeup (`scheduler/mailbox/`).
Inboxes are **bounded** — see [`mailbox.md`](mailbox.md).

### Selective receive (`ReceiveMatch`)

`Receive` takes the next hop. `ReceiveMatch` / `ReceiveMatchImm` wait for a
hop whose `Message.tag` matches — earlier non-matching hops stay in the
mailbox (**FIFO skip**, never drop). A parked selective waiter is woken only
by a matching hop; junk is queued behind the same lock.

| Opcode | Form |
|--------|------|
| `ReceiveMatch` `0x53` | `ra, rb` — tag from `r[b]` (`Int` in `0..=u16::MAX`) |
| `ReceiveMatchImm` `0x54` | `ra, imm` — immediate tag |

Sample: [`samples::selective_receive`](../src/samples.rs) (`TAG_JUNK` then `TAG_REQ`).

### `Ask` — atomic RPC hop (`0x55`)

`Ask ra, rb, rc` delivers `r[c]` (`Message`) to `r[b]` (`Cap`), then parks the
caller until a reply matches:

```text
reply.request_id == request.request_id
&& reply.sender  == resolved_FlowId(target_cap)
```

Implemented via mailbox `WaitFilter::Correlation { expect_request_id, expect_sender: Some(flow_id) }`.
FIFO skip applies: unrelated hops (wrong id or wrong sender) stay queued.

`AskTimeout` (`0x5A`) is the same hop plus a deadline: the dest register
gets `Value::Unit` if no correlated reply arrives in time (same writeback
as `ReceiveTimeout`). If the **target exits** first, dest is a
`TAG_SYS_EXIT` hop instead (see [`lifecycle.md`](lifecycle.md)). Sample:
[`samples::ask_reply`](../src/samples.rs),
[`samples::ask_timeout_expires`](../src/samples.rs),
[`samples::ask_target_exits`](../src/samples.rs).

That is the deliberate difference vs classic actor runtimes that allow any value on send.

## Make / unpack (std natives)

Stable indices in [`natives.rs`](../src/natives.rs):

| Idx | Name | Args → result |
|----:|------|----------------|
| 0 | `print` | values… → `Unit` (flow-visible log) |
| 1 | `now_ms` | → `Int` |
| 2 | `make_msg` | sender, request_id, tag, payload → `Message` |
| 3 | `msg_sender` | msg → `Pid` (identity) |
| 4 | `msg_request_id` | msg → `Int` |
| 5 | `msg_tag` | msg → `Int` |
| 6 | `msg_payload` | msg → `Int` |
| 7 | `msg_reply_cap` | msg → `Cap` (SEND grant) |

Runtime must use `Runtime::with_natives(chunk, std_native_table())` (or `with_natives_and_config`).

## Sample + refresh

```text
# unit + sample tests
cargo test -p byteflow-actors

# demos
cargo run -p byteflow-actors --example ping_pong
cargo run -p byteflow-actors --example atomic_actors

# Windows PowerShell — scheduler logs
$env:BYTEFLOW_LOG="info"
cargo run -p byteflow-actors --example atomic_actors
```

## Hard rules (do not regress)

1. Opcodes are **append-only** — never renumber.
2. Std natives **0–6 frozen**; **7** is `msg_reply_cap` (append-only thereafter).
3. Fail-closed: no `PoisonError::into_inner()`; mutex helpers → `Result`.
4. Preserve long design comments (mailbox, directory, oneshot, timer, sync_lock).
5. Clippy: `unwrap_used` + `expect_used` = deny (including tests).
