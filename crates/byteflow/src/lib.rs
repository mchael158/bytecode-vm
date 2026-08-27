//! Byteflow — embeddable **flow** runtime (package **`byteflow-actors`**).
//!
//! Not a language, not Tokio, not a JVM. You assemble register bytecode in
//! host Rust ([`ChunkBuilder`]), spawn many lightweight **flows** on an M:N
//! scheduler, and they talk through mailboxes with a strict hop protocol.
//!
//! Dependents write `use byteflow::...` (crate name) while crates.io lists
//! the package as [`byteflow-actors`](https://crates.io/crates/byteflow-actors).
//!
//! # What you get
//!
//! | Piece | Role |
//! |-------|------|
//! | [`ChunkBuilder`] / [`Opcode`] | Assemble `.bf` programs in Rust (no source language) |
//! | [`Vm`] / [`VmResult`] | Per-flow register interpreter; effects hand off to the scheduler |
//! | [`Runtime`] | Worker pool + timer; spawn / join / host [`Runtime::send`] |
//! | [`Value::Message`] | **Atomic Hop** envelope — the only value allowed on `Send` / `Ask` |
//! | [`Value::Cap`] | **FlowCap** address for bytecode delivery (`Send` / `Ask` targets) |
//! | [`Supervisor`] | Restart policies when a flow fails |
//! | [`std_native_table`] | `print`, `now_ms`, `make_msg`, `msg_*`, `msg_reply_cap` |
//!
//! # Atomic Hop (messaging contract)
//!
//! Every bytecode `Send` / `Ask` carries exactly one [`Message`]:
//!
//! ```text
//! Message { sender, reply_cap, request_id, tag, payload }
//! ```
//!
//! - Bare `Int` / `Pid` / `Str` on `Send` → trap / [`SendError::NotAHop`]
//! - Scheduler **stamps** `sender` (authenticated origin) and mints
//!   `reply_cap` (SEND-only Cap back to the caller)
//! - Reply with [`std_native_table`]'s `msg_reply_cap` — **not** `msg_sender`
//!   (`Pid` is identity, not an address)
//!
//! Also: selective receive (`ReceiveMatch`), and `Ask` for correlated RPC.
//!
//! # FlowCap (addressing)
//!
//! | Value | Use |
//! |-------|-----|
//! | [`Value::Cap`] | Target of `Send` / `Ask`; from `SelfPid`, `Spawn`, or `reply_cap` |
//! | [`Value::Pid`] | Identity inside a delivered hop (`msg_sender`) |
//!
//! Host [`Runtime::send`] still takes [`FlowId`] (trusted embedder path).
//!
//! # Values (ABI v4)
//!
//! `Unit | Bool | Int | Float | Pid | Message | Cap | Str | Bytes`
//!
//! `Str` / `Bytes` are `Arc`-backed for cheap register/mailbox clones. They
//! are **not** Atomic Hops by themselves.
//!
//! # Quick start — scalar
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use byteflow::{ChunkBuilder, Opcode, FlowOutcome, Runtime, Value};
//!
//! let mut b = ChunkBuilder::new("demo");
//! b.begin_function("main", 0, 2);
//! b.emit_load_imm(0, 41);
//! b.emit_load_imm(1, 1);
//! b.emit_binop(Opcode::Add, 0, 0, 1);
//! b.emit_return(0);
//!
//! let rt = Runtime::new(b.finish())?;
//! let outcome = rt.spawn(0, &[])?.join();
//! rt.shutdown();
//! assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
//! # Ok(())
//! # }
//! ```
//!
//! # Quick start — Atomic Hop (ping-pong)
//!
//! Hop demos need the std native table (`make_msg` / `msg_*`):
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use byteflow::{samples, std_native_table, FlowOutcome, Runtime, Value};
//!
//! let rt = Runtime::with_natives(samples::ping_pong(), std_native_table())?;
//! let Some(main) = rt.function_index("main") else { return Ok(()); };
//! let handle = rt.spawn(main, &[])?;
//! let outcome = handle.join();
//! rt.shutdown();
//! assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(2))));
//! # Ok(())
//! # }
//! ```
//!
//! More samples: [`samples::atomic_request_reply`], [`samples::ask_reply`],
//! [`samples::selective_receive`], forged-sender security regressions.
//!
//! # Design guides (rendered on docs.rs)
//!
//! - [`docs::atomic_hop`] — hop protocol, Cap addressing, natives table
//! - [`docs::mailbox`] — bounded inbox, overflow, lost-wakeup
//! - [`docs::security`] — threat model, invariants S1–S7, roadmap
//! - [`docs::error_model`] — fail-closed errors (no `unwrap`)
//!
//! # What this is *not*
//!
//! - Not a replacement for Tokio / async Rust (no `.await` IO loop)
//! - Not a distributed cluster runtime (single process, in-memory mailboxes)
//! - Not a full object-capability OS (native quotas / Cap attenuation come later)
//!
//! Host owns I/O and policy. Byteflow owns cheap concurrency and hop delivery.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod bytecode;
pub mod log;
pub mod natives;
pub mod samples;
pub mod scheduler;
pub mod vm;

/// Long-form design notes shipped inside the crate (also under `docs/` on GitHub).
///
/// These modules exist so [docs.rs](https://docs.rs/byteflow-actors) shows the
/// same guides as the repository, not only API rustdoc.
pub mod docs {
    /// Atomic Hop: Message-only Send, FlowCap addressing, Ask, selective receive.
    #[doc = include_str!("../docs/atomic-hop.md")]
    pub mod atomic_hop {}

    /// Security model: authenticated sender, FlowCap, invariants S1–S7.
    #[doc = include_str!("../docs/security.md")]
    pub mod security {}

    /// Fail-closed error taxonomy and mutex policy.
    #[doc = include_str!("../docs/error-model.md")]
    pub mod error_model {}

    /// Bounded mailbox: capacity contract, overflow, anti lost-wakeup.
    #[doc = include_str!("../docs/mailbox.md")]
    pub mod mailbox {}
}

pub use bytecode::{
    asm_macros, decode, disassemble, encode, verify, Chunk, ChunkBuilder, FormatError,
    FunctionDef, Instruction, Label, Message, Opcode, Value, VerifyError, ABI_VERSION, MAGIC,
};
pub use natives::{std_native_map, std_native_table, std_natives};
pub use scheduler::{
    fault_count, next_flow_id, flow_id_from_u64, report_fault, CapId, CapRights, ChildSpec,
    Delivery, Mailbox, MailboxCapacity, MailboxConfig, MailboxFull, MailboxStats, OverflowPolicy,
    Flow, FlowHandle, FlowId, FlowMetrics, FlowOutcome, FlowState,
    RestartPolicy, Runtime, RuntimeConfig, RuntimeError, RuntimeMetrics,
    RuntimeMetricsSnapshot, RuntimeSpawner, SendError, SpawnError, Supervisor,
    SupervisorConfig, DEFAULT_QUANTUM,
};
pub use vm::{
    expect_arg, expect_bool, expect_int, expect_message, expect_u64, Fault, NativeFn,
    NativeResult, NativeTable, NativeTableBuilder, NativeTableError, Vm, VmResult, MAX_CALL_DEPTH,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_native_map_has_stable_indices() {
        let map = std_native_map();
        assert_eq!(map.get("print"), Some(&0));
        assert_eq!(map.get("now_ms"), Some(&1));
        assert_eq!(map.get("make_msg"), Some(&2));
        assert_eq!(map.get("msg_payload"), Some(&6));
        assert_eq!(map.get("msg_reply_cap"), Some(&7));
    }

    #[test]
    fn chunk_builder_with_std_natives() -> Result<(), Box<dyn std::error::Error>> {
        let mut b = ChunkBuilder::new("std-natives-demo");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 42);
        b.emit_call_native(0, 0, 1);
        b.emit_call_native(1, 1, 0);
        b.emit_return(1);

        let chunk = b.finish();
        verify(&chunk)?;

        let rt = Runtime::with_natives(chunk, std_native_table())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();

        match outcome {
            FlowOutcome::Completed(Value::Int(ms)) => {
                assert!(ms >= 0);
                Ok(())
            }
            other => Err(format!("expected Completed(Value::Int(_)), got {other:?}").into()),
        }
    }
}
