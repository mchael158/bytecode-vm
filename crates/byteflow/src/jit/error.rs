use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("cranelift backend: {0}")]
    Backend(String),
    #[error("trace empty at function {function} pc {pc}")]
    EmptyTrace { function: u32, pc: u32 },
    #[error("undefined register {reg} at pc {pc}")]
    UndefinedRegister { reg: u8, pc: u32 },
    #[error("unsupported opcode {opcode:?} at pc {pc}")]
    UnsupportedOpcode {
        opcode: crate::Opcode,
        pc: u32,
    },
    #[error("trace longer than {limit} at pc {pc}")]
    TraceTooLong { pc: u32, limit: usize },
    #[error("module error: {0}")]
    Module(String),
}
