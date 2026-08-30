//! Native exit codes returned by compiled traces.

/// Why a compiled trace stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Keep executing natively at `pc`.
    Continue { pc: u32 },
    /// Delegate to the interpreter / scheduler at `pc` (Send, Ask, …).
    Effect { pc: u32 },
    /// Outermost `Return` — value lives in `return_reg`.
    Return { return_reg: u8 },
    /// Instruction quantum exhausted mid-trace.
    Budget { pc: u32 },
    /// Semantic fault (divide-by-zero, explicit trap, …).
    Trap { pc: u32 },
    /// Type guard failed — resume in the interpreter.
    Deopt { pc: u32 },
}

/// Wire format returned across the native boundary.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitReturn {
    pub kind: u32,
    pub pc: u32,
    pub return_reg: u32,
}

pub const JIT_CONTINUE: u32 = 0;
pub const JIT_EFFECT: u32 = 1;
pub const JIT_RETURN: u32 = 2;
pub const JIT_BUDGET: u32 = 3;
pub const JIT_TRAP: u32 = 4;
pub const JIT_DEOPT: u32 = 5;

impl JitReturn {
    pub fn from_reason(reason: ExitReason) -> Self {
        match reason {
            ExitReason::Continue { pc } => Self {
                kind: JIT_CONTINUE,
                pc,
                return_reg: 0,
            },
            ExitReason::Effect { pc } => Self {
                kind: JIT_EFFECT,
                pc,
                return_reg: 0,
            },
            ExitReason::Return { return_reg } => Self {
                kind: JIT_RETURN,
                pc: 0,
                return_reg: u32::from(return_reg),
            },
            ExitReason::Budget { pc } => Self {
                kind: JIT_BUDGET,
                pc,
                return_reg: 0,
            },
            ExitReason::Trap { pc } => Self {
                kind: JIT_TRAP,
                pc,
                return_reg: 0,
            },
            ExitReason::Deopt { pc } => Self {
                kind: JIT_DEOPT,
                pc,
                return_reg: 0,
            },
        }
    }

    pub fn into_reason(self) -> ExitReason {
        match self.kind {
            JIT_CONTINUE => ExitReason::Continue { pc: self.pc },
            JIT_EFFECT => ExitReason::Effect { pc: self.pc },
            JIT_RETURN => ExitReason::Return {
                return_reg: self.return_reg as u8,
            },
            JIT_BUDGET => ExitReason::Budget { pc: self.pc },
            JIT_TRAP => ExitReason::Trap { pc: self.pc },
            JIT_DEOPT => ExitReason::Deopt { pc: self.pc },
            _ => ExitReason::Deopt { pc: self.pc },
        }
    }
}
