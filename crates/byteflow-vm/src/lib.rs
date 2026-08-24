//! `byteflow-vm` — a register-based bytecode interpreter for exactly one
//! virtual process at a time.
//!
//! Deliberately dumb about everything outside its own call stack: no
//! threads, no mailboxes, no timers, no knowledge that other processes
//! exist. See [`VmResult`] for how it hands control back to
//! `byteflow-scheduler` for anything that needs that broader context. This
//! is what makes `Vm` `Send` and freely movable between worker threads —
//! there is nothing thread-affine about it.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![allow(missing_docs)] // relaxed for v0; re-enable once the ISA settles.

mod fault;
mod frame;
mod native;
mod result;
mod vm;

pub use fault::Fault;
pub use native::{expect_arg, expect_int, NativeFn, NativeResult, NativeTable, NativeTableBuilder};
pub use result::VmResult;
pub use vm::{Vm, MAX_CALL_DEPTH};

// Re-export so downstream crates depend on one fewer path for the value
// type that flows through `Vm`'s public API.
pub use byteflow_bytecode::Value;