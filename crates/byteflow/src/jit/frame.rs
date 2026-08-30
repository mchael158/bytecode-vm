//! Execution frame passed into compiled traces.

/// Byte offsets for [`JitFrame`] fields (must match `repr(C)` layout).
pub const OFF_REGISTER_COUNT: i32 = 8;
pub const OFF_PC: i32 = 12;
pub const OFF_BUDGET: i32 = 16;
pub const OFF_FUNCTION: i32 = 20;
pub const OFF_EXIT_KIND: i32 = 24;
pub const OFF_RETURN_REG: i32 = 28;
pub const OFF_CALL_DEPTH: i32 = 32;
pub const OFF_CALL_STACK: i32 = 40;

/// Maximum nested intra-chunk calls handled inside one compiled trace.
pub const MAX_JIT_CALL_DEPTH: usize = 32;

/// Saved caller state for one `Opcode::Call`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct JitCallRecord {
    pub return_pc: u32,
    pub return_function: u32,
    pub dest_reg: u32,
    pub caller_register_count: u32,
}

/// Mutable state for one JIT invocation on the current bytecode frame.
///
/// v1 keeps scalar work in a parallel `i64` slot table (Int specialization).
/// The dispatcher copies [`crate::Value::Int`] in before the call and
/// writes results back after `Return`.
#[repr(C)]
pub struct JitFrame {
    pub slots: *mut i64,
    pub register_count: u32,
    pub pc: u32,
    pub budget: u32,
    pub function: u32,
    pub exit_kind: u32,
    pub return_reg: u32,
    pub call_depth: u32,
    pub call_stack: *mut JitCallRecord,
}

pub type JitEntry = unsafe extern "C" fn(*mut JitFrame);
