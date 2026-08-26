//! Byteflow — embeddable register-based **flow** runtime.
//!
//! # Overview
//!
//! Assemble programs with [`ChunkBuilder`] in host Rust (no separate source
//! language). Spawn lightweight **flows** on an M:N scheduler; they communicate
//! through FIFO mailboxes via **Atomic Hops** ([`Value::Message`] only on
//! `Send` / `Ask`) and can be supervised on failure.
//!
//! The crates.io package is **`byteflow-actors`**; this library crate is named
//! `byteflow`, so dependents write `use byteflow::...`.
//!
//! # Security (authenticated hops + FlowCap)
//!
//! Structural typing (`Send` requires [`Value::Message`]) is **not**
//! authentication or authorization. Bytecode may forge `Message.sender` via
//! `make_msg`; the scheduler **overwrites** that field and mints a
//! **SEND**-only `reply_cap` before delivery. `Send` / `Ask` targets must be
//! [`Value::Cap`] — raw [`Value::Pid`] is identity only.
//!
//! Full threat model, invariants **S1–S7**, and the capability roadmap:
//! `docs/security.md` in the crate sources (also shipped on docs.rs when
//! `docs/` is included in the package).
//!
//! # Quick example
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
//! Host owns I/O. Byteflow owns cheap concurrency.
#![forbid(unsafe_code)]
// Tests may use unwrap/expect for brevity; production paths must not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bytecode;
pub mod log;
pub mod natives;
pub mod samples;
pub mod scheduler;
pub mod vm;

pub use bytecode::{
    asm_macros, decode, disassemble, encode, verify, Chunk, ChunkBuilder, FormatError,
    FunctionDef, Instruction, Label, Message, Opcode, Value, VerifyError, ABI_VERSION, MAGIC,
};
pub use natives::{std_native_map, std_native_table, std_natives};
pub use scheduler::{
    fault_count, next_flow_id, flow_id_from_u64, report_fault, CapId, CapRights, ChildSpec,
    Delivery, Mailbox, Flow, FlowHandle, FlowId, FlowMetrics, FlowOutcome, FlowState,
    RestartPolicy, Runtime, RuntimeConfig, RuntimeError, RuntimeMetrics,
    RuntimeMetricsSnapshot, RuntimeSpawner, SendError, SpawnError, Supervisor,
    SupervisorConfig, DEFAULT_QUANTUM,
};
pub use vm::{
    expect_arg, expect_bool, expect_int, expect_message, expect_u64, Fault, NativeFn,
    NativeResult, NativeTable, NativeTableBuilder, Vm, VmResult, MAX_CALL_DEPTH,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_native_map_has_stable_indices() {
        let map = std_native_map();
        assert_eq!(map["print"], 0);
        assert_eq!(map["now_ms"], 1);
        assert_eq!(map["make_msg"], 2);
        assert_eq!(map["msg_payload"], 6);
        assert_eq!(map["msg_reply_cap"], 7);
    }

    #[test]
    fn chunk_builder_with_std_natives() {
        let mut b = ChunkBuilder::new("std-natives-demo");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 42);
        b.emit_call_native(0, 0, 1);
        b.emit_call_native(1, 1, 0);
        b.emit_return(1);

        let chunk = b.finish();
        verify(&chunk).expect("verify");

        let rt = Runtime::with_natives(chunk, std_native_table()).expect("runtime");
        let outcome = rt.spawn(0, &[]).expect("spawn").join();
        rt.shutdown();

        match outcome {
            FlowOutcome::Completed(Value::Int(ms)) => assert!(ms >= 0),
            other => panic!("expected Completed(Value::Int(_)), got {other:?}"),
        }
    }
}
