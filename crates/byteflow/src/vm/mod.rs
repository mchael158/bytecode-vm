//! Register-based interpreter for one virtual process.
//! Scheduler effects return as [`VmResult`].

mod fault;
mod frame;
mod machine;
mod native;
mod result;

pub use fault::Fault;
pub use machine::{Vm, MAX_CALL_DEPTH};
pub use native::{
    expect_arg, expect_bool, expect_int, expect_message, expect_u64, NativeFn, NativeResult,
    NativeTable, NativeTableBuilder,
};
pub use result::VmResult;
pub use crate::bytecode::Value;
