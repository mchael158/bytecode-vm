use std::time::Duration;

use byteflow_bytecode::Value;

use crate::fault::Fault;

/// What happened at the end of a [`crate::Vm::run`] slice.
///
/// This is the entire interface between `byteflow-vm` and the scheduler: the
/// VM never touches threads, mailboxes or timers directly. It runs bytecode
/// until it either finishes, needs an effect only the scheduler can perform,
/// or exhausts its instruction budget — then hands one of these back and
/// stops. That separation is what lets `byteflow-scheduler` move a suspended
/// `Vm` between worker threads freely: it's just a value sitting in a
/// `Process`.
#[derive(Debug)]
pub enum VmResult {
    /// The outermost frame returned/exited. The process should terminate
    /// with this value delivered to `ProcessHandle::join`.
    Complete(Value),
    /// Cooperative yield or instruction-budget exhaustion. Re-enqueue as
    /// `Ready` on any worker; resuming picks up at the saved `pc` with no
    /// register writeback needed.
    Yield,
    /// `Sleep` opcode. Register the process on the timer wheel; resume with
    /// a plain `run()` call (no writeback) once it elapses.
    Sleep(Duration),
    /// `Spawn` opcode. The scheduler creates a new `Process` from
    /// `function` with `args` in the same chunk and must call
    /// [`crate::Vm::resume_with`]`(dest_reg, Value::Pid(new_id))` before the
    /// next `run()`.
    Spawn {
        function: u32,
        args: Vec<Value>,
        dest_reg: u8,
    },
    /// `SelfPid` opcode. The scheduler writes this process's Pid into
    /// `dest_reg` via [`crate::Vm::resume_with`] and continues.
    SelfPid { dest_reg: u8 },
    /// `Send` opcode; fire-and-forget. The scheduler delivers `message` to
    /// `target`'s mailbox (waking it if it was `Waiting` on `Receive`) and
    /// then simply calls `run()` again — no writeback.
    Send { target: u64, message: Value },
    /// `Receive`/`ReceiveTimeout` opcode. If the process's mailbox has a
    /// message, the scheduler must call `resume_with(dest_reg, message)`. If
    /// not, it transitions the process to `Waiting` (optionally also
    /// registering `timeout` on the timer wheel) until a `Send` arrives.
    Receive {
        dest_reg: u8,
        timeout: Option<Duration>,
    },
    /// A fault occurred; the process fails. See [`Fault`].
    Trap(Fault),
}
