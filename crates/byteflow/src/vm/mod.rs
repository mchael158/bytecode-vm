//! Register-based interpreter for one virtual process.
//! Scheduler effects return as [`VmResult`].

mod fault;
mod frame;
mod machine;
mod native;
mod result;

pub use fault::Fault;
pub use machine::{Vm, MAX_CALL_DEPTH};
pub use native::{expect_arg, expect_int, NativeFn, NativeResult, NativeTable, NativeTableBuilder};
pub use result::VmResult;
pub use crate::bytecode::Value;
