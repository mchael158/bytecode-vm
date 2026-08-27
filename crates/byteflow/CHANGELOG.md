# Changelog

All notable changes to **byteflow-actors** are documented here.

## [0.5.1] — 2026-08-26

### Docs
- Crate rustdoc rewritten for docs.rs: Atomic Hop, FlowCap, value table,
  scalar + ping-pong examples.
- Guides rendered on docs.rs via `byteflow::docs::{atomic_hop, security, error_model}`.

## [0.5.0] — 2026-08-26

### Breaking
- **FlowCap (security phase 2):** bytecode `Send` / `Ask` require `Value::Cap`.
  `SelfPid` / `Spawn` write Caps (`SEND|ASK`). `Value::Pid` is identity only
  (`Message.sender` / `msg_sender`).
- **`Message.reply_cap`:** stamped at the hop boundary with SEND-only rights;
  replies use `msg_reply_cap` (native index **7**).
- **ABI v4:** wire includes `reply_cap`, Cap tag `6`, **`Str` tag `7`**,
  **`Bytes` tag `8`**.

### Added
- **`Value::Str` / `Value::Bytes`** (`Arc`-backed): constant pool, registers,
  `print`, `Eq` by content; empty Str/Bytes are falsy for `Branch`.

### Docs
- `docs/security.md` — FlowCap as current model; phase 2 marked done.
- `docs/atomic-hop.md` — Cap addressing + `msg_reply_cap`.

## [0.4.0] — 2026-08-25

### Breaking
- Public concurrent unit renamed **flow**: `Flow`, `FlowId`, `FlowHandle`,
  `FlowOutcome`, `FlowState`, `FlowMetrics` (replaces `Process*`).
- **Atomic Hop:** `Send` / `Runtime::send` accept only `Value::Message`
  (`SendError::NotAHop` / VM type trap otherwise).
- `Runtime::live_processes` → `live_flows`.
- `samples::ping_pong` now uses Message hops (requires std natives).
- **Selective receive:** `ReceiveMatch` / `ReceiveMatchImm` (FIFO skip by `tag`);
  `samples::selective_receive`.
- **`Ask` (`0x55`):** atomic request/reply hop with `WaitFilter::Correlation`
  (`request_id` + `sender == target`); `samples::ask_reply`.
- **Authenticated sender (security phase 1):** bytecode `Send`/`Ask` stamp
  `Message.sender` with the executing flow id before delivery; forged
  `make_msg` sender is ignored (`docs/security.md`, samples
  `forged_sender_send` / `forged_sender_ask`).

### Docs
- `docs/atomic-hop.md` (flows + Atomic Hop + Ask). `atomic-actors.md` redirects.
- `docs/security.md` — threat model + authenticated sender contract.

## [0.3.0] — 2026-08-25

### Breaking
- `Runtime::new` / `with_*` return `Result<Runtime, SpawnError>` (no panic on verify / thread spawn).
- `Runtime::spawn`, `RuntimeSpawner::spawn`, `Supervisor::{new,with_config,start_child}` return `Result`.
- Std native table grew Message helpers at frozen slots **2–6** (`make_msg`, `msg_*`). Slots **0–1** unchanged.

### Added
- `Value::Message` / ABI v2 envelopes (`sender`, `request_id`, `tag`, `payload`).
- `samples::atomic_request_reply` + example `atomic_actors`.
- Scheduler logs via `BYTEFLOW_LOG` (`crate::log`).
- Fail-closed mutex helpers + `RuntimeError` / `SpawnError` (`docs/error-model.md`).
- Clippy: `unwrap_used` and `expect_used` denied (including tests).

### Docs
- `docs/atomic-actors.md`, `docs/error-model.md`.

## [0.2.1] — prior

Initial crates.io line: ISA, VM, M:N scheduler, mailboxes, supervisor, std natives (`print`, `now_ms`), CLI.
