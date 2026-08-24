//! Byteflow — a register-based virtual-process runtime.
//!
//! This crate re-exports the public surface of the workspace:
//! - [`byteflow_bytecode`]: ISA, assembler (`ChunkBuilder`), `.bf` codec and verifier
//! - [`byteflow_vm`]: per-process interpreter + `NativeTable`
//! - [`byteflow_scheduler`]: processes, mailboxes and the M:N runtime
//!
//! Programs are assembled in Rust with [`ChunkBuilder`] — there is no
//! separate source language.

pub mod natives;
pub mod samples;

pub use byteflow_bytecode::{
    decode, disassemble, encode, verify, Chunk, ChunkBuilder, FormatError, FunctionDef,
    Instruction, Label, Opcode, Value, VerifyError, ABI_VERSION, MAGIC,
};
pub use byteflow_scheduler::{
    next_process_id, pid_from_u64, ChildSpec, Delivery, Mailbox, Process, ProcessHandle,
    ProcessId, ProcessMetrics, ProcessOutcome, ProcessState, RestartPolicy, Runtime,
    RuntimeConfig, RuntimeMetrics, RuntimeMetricsSnapshot, RuntimeSpawner, SendError,
    Supervisor, SupervisorConfig, DEFAULT_QUANTUM,
};
pub use byteflow_vm::{
    expect_arg, expect_int, Fault, NativeFn, NativeResult, NativeTable, NativeTableBuilder, Vm,
    VmResult, MAX_CALL_DEPTH,
};

pub use natives::{std_native_map, std_native_table, std_natives};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_native_map_has_stable_indices() {
        let map = std_native_map();
        assert_eq!(map["print"], 0);
        assert_eq!(map["now_ms"], 1);
    }

    /// ChunkBuilder + std natives: print 42, return now_ms.
    #[test]
    fn chunk_builder_with_std_natives() {
        let mut b = ChunkBuilder::new("std-natives-demo");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 42);
        b.emit_call_native(0, 0, 1); // print(r0)
        b.emit_call_native(1, 1, 0); // r1 = now_ms()
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
