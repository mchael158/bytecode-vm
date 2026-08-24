//! Byteflow — embeddable register-based virtual-process runtime.
//!
//! # Overview
//!
//! Assemble programs with [`ChunkBuilder`] in host Rust (no separate source
//! language). Spawn lightweight processes on an M:N scheduler; they communicate
//! through FIFO mailboxes and can be supervised on failure.
//!
//! The crates.io package is **`byteflow-actors`**; this library crate is named
//! `byteflow`, so dependents write `use byteflow::...`.
//!
//! # Quick example
//!
//! ```
//! use byteflow::{ChunkBuilder, Opcode, ProcessOutcome, Runtime, Value};
//!
//! let mut b = ChunkBuilder::new("demo");
//! b.begin_function("main", 0, 2);
//! b.emit_load_imm(0, 41);
//! b.emit_load_imm(1, 1);
//! b.emit_binop(Opcode::Add, 0, 0, 1);
//! b.emit_return(0);
//!
//! let rt = Runtime::new(b.finish());
//! let outcome = rt.spawn(0, &[]).join();
//! rt.shutdown();
//! assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(42))));
//! ```
//!
//! Host owns I/O. Byteflow owns cheap concurrency.
#![forbid(unsafe_code)]

pub mod bytecode;
pub mod natives;
pub mod samples;
pub mod scheduler;
pub mod vm;

pub use bytecode::{
    decode, disassemble, encode, verify, Chunk, ChunkBuilder, FormatError, FunctionDef,
    Instruction, Label, Opcode, Value, VerifyError, ABI_VERSION, MAGIC,
};
pub use natives::{std_native_map, std_native_table, std_natives};
pub use scheduler::{
    next_process_id, pid_from_u64, ChildSpec, Delivery, Mailbox, Process, ProcessHandle,
    ProcessId, ProcessMetrics, ProcessOutcome, ProcessState, RestartPolicy, Runtime,
    RuntimeConfig, RuntimeMetrics, RuntimeMetricsSnapshot, RuntimeSpawner, SendError,
    Supervisor, SupervisorConfig, DEFAULT_QUANTUM,
};
pub use vm::{
    expect_arg, expect_int, Fault, NativeFn, NativeResult, NativeTable, NativeTableBuilder, Vm,
    VmResult, MAX_CALL_DEPTH,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_native_map_has_stable_indices() {
        let map = std_native_map();
        assert_eq!(map["print"], 0);
        assert_eq!(map["now_ms"], 1);
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

        let rt = Runtime::with_natives(chunk, std_native_table());
        let outcome = rt.spawn(0, &[]).join();
        rt.shutdown();

        match outcome {
            ProcessOutcome::Completed(Value::Int(ms)) => assert!(ms >= 0),
            other => panic!("expected Completed(Value::Int(_)), got {other:?}"),
        }
    }
}
