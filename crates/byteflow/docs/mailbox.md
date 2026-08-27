# Bounded mailbox

Every flow has one inbox. Unbounded `VecDeque` growth is not a capacity
API — it is an OOM path when many flows share few workers. This crate
treats mailbox size as a **memory contract**, not a raw `usize`.

## Contract

| Piece | Role |
|-------|------|
| [`MailboxCapacity`](../src/scheduler/mailbox/capacity.rs) | Validated bound (`MIN=1`, `MAX=1<<20`, default **256**) |
| [`OverflowPolicy`](../src/scheduler/mailbox/policy.rs) | `Reject` / `DropNewest` / `DropOldest` |
| [`MailboxConfig`](../src/scheduler/mailbox/policy.rs) | The pair applied to **every** mailbox the runtime spawns |

There is **no** `Block` policy. Parking an OS worker on a full inbox
would stall every other flow on that thread. Scheduler-level
`WAITING_SEND` (sender flow waits until a slot frees) is a later phase.

Logical capacity ≠ physical allocation. The queue grows geometrically
and stops at the limit — a flow that receives one hop with `limit=4096`
does not pre-pay 4096 slots.

`MailboxQueue` is a `pub(crate)` abstraction so a future ring buffer can
replace `VecDeque` without touching FlowCap, Ask, or the worker loop.
`#![forbid(unsafe_code)]` — no `MaybeUninit` ring in this revision.

## Wake (lost-wakeup)

`park` and `push` share **one mutex**. A hop that matches a parked
waiter is a [`Delivery::Handoff`](../src/scheduler/mailbox/mod.rs) — it
does **not** consume a queue slot. Non-matching hops are queued (if the
bound allows) and the waiter stays parked (FIFO skip).

Nobody parked → no wake. Overflow that **drops** a hop never produces
Handoff.

## Overflow

| Policy | Host `Runtime::send` | Bytecode `Send` / `Ask` |
|--------|----------------------|-------------------------|
| **Reject** (default) | `SendError::MailboxFull` | hop logged and discarded; sender flow is **not** failed (worker must not stall) |
| **DropNewest** | `Ok` (incoming hop gone) | same |
| **DropOldest** | `Ok` (oldest queued hop gone) | same |

Configure via `RuntimeConfig.mailbox`:

```rust
use byteflow::{MailboxCapacity, MailboxConfig, OverflowPolicy, RuntimeConfig};

let mailbox = MailboxConfig::new(
    MailboxCapacity::DEFAULT,
    OverflowPolicy::Reject,
);
let cfg = RuntimeConfig {
    workers: 2,
    quantum: byteflow::DEFAULT_QUANTUM,
    mailbox,
};
```

`MailboxConfig::DEFAULT` is compile-time valid (256, Reject) — no
`expect` in production.

## Hard rules

1. Preserve park+push under one mutex (comment + race diagram in
   `scheduler/mailbox/mod.rs`).
2. Matching waiter → Handoff, never a queue slot.
3. No `Block`. No `unwrap` on the push path.
4. Capacity is [`MailboxCapacity`], never a raw `usize` at the API.
