use crate::bytecode::Value;

/// One activation record. Register windows are per-frame (not a single
/// global register file), so recursive/re-entrant calls can't clobber a
/// caller's registers — the price is one heap allocation per call, which is
/// acceptable for v0 and is the first thing `byteflow-jit` would optimise
/// away (e.g. via a shared, growable register stack) in design notes §28-29.
#[derive(Debug)]
pub struct Frame {
    pub function: u32,
    pub pc: usize,
    pub registers: Vec<Value>,
    /// Register index in the *caller's* frame that will receive this
    /// frame's return value. `None` for the outermost frame, whose return
    /// value completes the process instead.
    pub dest_reg: Option<u8>,
}

impl Frame {
    pub fn new(function: u32, num_registers: u8, dest_reg: Option<u8>) -> Self {
        Frame {
            function,
            pc: 0,
            registers: vec![Value::Unit; num_registers as usize],
            dest_reg,
        }
    }
}
