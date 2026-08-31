use std::time::Duration;

use crate::bytecode::Value;

use super::fault::Fault;

/// What happened at the end of a [`crate::Vm::run`] slice.
///
/// This is the entire interface between `byteflow-vm` and the scheduler: the
/// VM never touches threads, mailboxes or timers directly. It runs bytecode
/// until it either finishes, needs an effect only the scheduler can perform,
/// or exhausts its instruction budget — then hands one of these back and
/// stops. That separation is what lets `byteflow-scheduler` move a suspended
/// `Vm` between worker threads freely: it's just a value sitting in a
/// `Flow`.
#[derive(Debug)]
pub enum VmResult {
    /// The outermost frame returned/exited. The Flow should terminate
    /// with this value delivered to `FlowHandle::join`.
    Complete(Value),
    /// Cooperative yield or instruction-budget exhaustion. Re-enqueue as
    /// `Ready` on any worker; resuming picks up at the saved `pc` with no
    /// register writeback needed.
    Yield,
    /// `Sleep` opcode. Register the Flow on the timer wheel; resume with
    /// a plain `run()` call (no writeback) once it elapses.
    Sleep(Duration),
    /// `Spawn` — create a child flow; parent receives a **Cap** (`SEND|ASK`).
    Spawn {
        function: u32,
        args: Vec<Value>,
        dest_reg: u8,
    },
    /// `SelfPid` — write a **self Cap** (`SEND|ASK`) into `dest_reg`.
    SelfPid { dest_reg: u8 },
    /// `Send` — Atomic Hop to a **capability** target (requires SEND).
    Send { target_cap: u64, message: Value },
    /// `Receive` / `ReceiveTimeout` / `ReceiveMatch` / `ReceiveMatchImm`.
    Receive {
        dest_reg: u8,
        timeout: Option<Duration>,
        match_tag: Option<u16>,
    },
    /// `Ask` / `AskTimeout` — RPC hop to a **capability** target (requires ASK).
    Ask {
        dest_reg: u8,
        target_cap: u64,
        request: Value,
        timeout: Option<Duration>,
    },
    /// `Monitor ra, rb` — watch the flow addressed by Cap `r[b]`.
    Monitor { dest_reg: u8, target_cap: u64 },
    /// `Demonitor ra` — drop monitor whose ref is `r[a]` (Int).
    Demonitor { monitor_reg: u8 },
    /// `Link ra, rb` — bidirectional link with Cap `r[b]`.
    Link { dest_reg: u8, target_cap: u64 },
    /// `Unlink ra` — drop link whose id is `r[a]` (Int).
    Unlink { link_reg: u8 },
    /// A fault occurred; the Flow fails. See [`Fault`].
    Trap(Fault),
}
