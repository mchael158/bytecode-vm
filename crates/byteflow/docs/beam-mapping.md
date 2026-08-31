# BEAM → Byteflow (mental model)

Guide for Erlang/Elixir developers reading this codebase. Byteflow is **not**
BEAM — but many ideas rhyme once you map terminology.

## Units

| BEAM | Byteflow | Notes |
|------|----------|-------|
| process | **flow** | `FlowId`, `FlowHandle`, `FlowOutcome` |
| `pid()` | **`hop_sender(msg)`** | Identity inside a *delivered* hop |
| `pid()` as address | **`Value::Cap`** | `SelfPid` / `Spawn` return Caps, not Pids |
| registered name | **`Runtime::register_name` / `whereis`** | Stores a **Cap**, not a FlowId. Swept on exit. |

**Key difference:** on BEAM, a Pid is both identity and delivery address. In
Byteflow, **Cap = address**, **Pid = identity** inside `Message.sender`.

```text
BEAM:     send(Pid, Term)
Byteflow: send(Cap, Message)   // Message envelope required
```

See [`atomic-hop.md`](atomic-hop.md) and [`security.md`](security.md) (S1, S6).

## Messaging

| BEAM | Byteflow API | Opcode / host |
|------|--------------|---------------|
| `spawn(fun)` | `Fn::spawn(fn, argc)` | `Spawn` → Cap |
| `Pid ! Msg` | `Fn::send(cap, hop)` | `Send` |
| `receive` | `Fn::receive()` | `Receive` |
| selective receive (pattern) | `Fn::receive_match_imm(tag)` | `ReceiveMatchImm` — **tag u16 only** |
| `gen_server:call` | `Fn::ask(cap, hop)` / `Fn::ask_timeout` | `Ask` / `AskTimeout` |
| `gen_server:cast` | `Fn::send(cap, hop)` | fire-and-forget |
| reply to caller | `Fn::send_reply(req, tag, payload)` | uses `msg_reply_cap` |
| build message | `Fn::hop(req_id, tag, payload)` | scheduler stamps sender |
| unpack message | `Fn::hop_payload(msg)` etc. | std natives |

### Message shape

BEAM messages are arbitrary terms. Byteflow **Atomic Hop** is a fixed envelope:

```text
Message { sender, reply_cap, request_id, tag, payload }
```

- `payload` is a single `u64` today — encode richer data via tags + natives.
- `sender` is **authenticated by the runtime** on bytecode `Send` / `Ask`.
  Do not forge it with `make_msg` (see [`samples::forged_sender_send`](../src/samples.rs)).

### Typical server loop (BEAM-style)

```rust
use byteflow::{Program, samples::TAG_REQ, samples::TAG_REP};

let mut p = Program::new("server");
let server = p.function("server", 0, |f| {
    let loop_lbl = f.label();
    f.bind(loop_lbl);
    let req = f.receive_match_imm(TAG_REQ as u16);
    let payload = f.hop_payload(req);
    f.add_imm(payload, 1);
    f.send_reply(req, TAG_REP, payload);
    f.jump(loop_lbl);
});
```

Sample: [`samples::server_loop`](../src/samples.rs).

## Lifecycle (mapped)

| BEAM / OTP | Byteflow today |
|------------|----------------|
| **links** | `Runtime::link` / `Fn::link` — abnormal exit kills the peer |
| **monitors** `{'DOWN', ...}` | `Runtime::monitor` / `Fn::monitor` → `TAG_SYS_DOWN` hop |
| **OTP supervisor strategies** | Host `Supervisor` + `RestartStrategy` (`OneForOne` / `OneForAll` / `RestForOne`) |
| **`register` / `whereis`** | `Runtime::register_name` / `whereis` (Cap, not FlowId) |
| **`exit(Pid, kill)`** | `Runtime::kill` (cooperative) |

## What BEAM has that Byteflow does not (yet)

| BEAM / OTP | Byteflow today |
|------------|----------------|
| **distribution** | Single process, in-memory |
| **pattern matching receive** | Tag-based selective receive only |
| **process dictionary** | No |
| **ETS** | No |
| **`trap_exit`** | No — links always kill on abnormal exit |

Failures surface as `FlowOutcome::Failed` on `join`, not as mailbox messages.

## Backpressure

| Path | Mailbox full + `Reject` |
|------|-------------------------|
| `Runtime::send(FlowId, …)` | `Err(SendError::MailboxFull)` |
| bytecode `Send` / `Ask` | sender parks (`WAITING_SEND`); one waiter per freed slot |

## Host Rust vs bytecode

| Task | Where |
|------|-------|
| spawn top-level flows | `Runtime::spawn` |
| trusted host send | `Runtime::send(FlowId, Value::Message)` |
| restart policy | `Supervisor` (Rust) |
| protocol in flows | `Program` / `Fn` bytecode |

Supervisor trees are **not** expressed as bytecode flows today — plan host
Rust for OTP-style supervision.

## Quick equivalence cheat sheet

```text
self()              →  hop_sender on received msg; self_address() for Cap
!                   →  send(cap, hop(...))
receive             →  receive() / receive_match_imm(TAG)
call                →  ask(cap, hop(...)) / ask_timeout(cap, hop, ms)
reply               →  send_reply(req, TAG_REP, payload)
spawn               →  spawn(fn) → Cap
register/whereis    →  Runtime::register_name / whereis; ChildSpec.name also registers
link/monitor        →  Fn::link / Fn::monitor; DOWN via TAG_SYS_DOWN
```

## Further reading

- [`atomic-hop.md`](atomic-hop.md) — protocol details
- [`mailbox.md`](mailbox.md) — bounded queues, overflow
- [`security.md`](security.md) — S1 authenticated sender, S6 FlowCap
- [`samples.rs`](../src/samples.rs) — runnable specs
