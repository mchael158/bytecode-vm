use std::fmt;

/// Anything that can go wrong *inside* a running process's VM.
///
/// A `Fault` is never a Rust panic — panics are reserved for genuine host
/// bugs and are caught at the worker boundary (see
/// `byteflow-scheduler::worker::run_worker`) precisely so that one process's
/// bug (division by zero, a corrupt jump target that slipped past the
/// verifier, an out-of-range register) can never take down a worker thread,
/// let alone the whole runtime. A `Fault` instead becomes
/// `ProcessState::Failed` and is handed to the process's supervisor, which
/// decides whether to restart it (design notes §15-16).
#[derive(Clone, Debug, PartialEq)]
pub enum Fault {
    DivideByZero,
    RegisterOutOfRange { reg: u8, frame_size: u8 },
    BadConstant { index: u32, pool_size: u32 },
    BadFunction { index: u32, table_size: u32 },
    BadOpcodeByte(u8),
    /// Function call nesting exceeded `Vm::MAX_CALL_DEPTH`. Bytecode has no
    /// native stack overflow (frames are heap-allocated `Vec<Value>`s), so
    /// this is a deliberate, checked limit rather than a segfault.
    CallStackOverflow { depth: usize },
    TypeMismatch { expected: &'static str, got: &'static str },
    /// `CallNative` referenced a slot outside the runtime's registered
    /// native function table (design notes §30-31). Distinct from
    /// `BadFunction`, which is about the *bytecode* function table baked
    /// into the chunk — natives are supplied by the embedder at `Vm`
    /// construction time and can't be range-checked by
    /// `crate::bytecode::verify`, which has no visibility into them.
    BadNative { index: u32, table_size: u32 },
    /// A native function returned an error (host-side failure — I/O,
    /// invalid argument the Rust side rejected, capability denied, etc).
    /// The message is native-function-defined.
    NativeError(String),
    /// Explicit `Trap` opcode, e.g. an assertion emitted by a compiler.
    Explicit(i32),
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::DivideByZero => write!(f, "division by zero"),
            Fault::RegisterOutOfRange { reg, frame_size } => {
                write!(f, "register r{reg} out of range (frame has {frame_size} registers)")
            }
            Fault::BadConstant { index, pool_size } => {
                write!(f, "constant index {index} out of range (pool size {pool_size})")
            }
            Fault::BadFunction { index, table_size } => {
                write!(f, "function index {index} out of range (table size {table_size})")
            }
            Fault::BadOpcodeByte(b) => write!(f, "unknown opcode byte 0x{b:02X}"),
            Fault::CallStackOverflow { depth } => write!(f, "call stack overflow at depth {depth}"),
            Fault::TypeMismatch { expected, got } => {
                write!(f, "type mismatch: expected {expected}, got {got}")
            }
            Fault::BadNative { index, table_size } => {
                write!(f, "native function index {index} out of range (table size {table_size})")
            }
            Fault::NativeError(msg) => write!(f, "native function error: {msg}"),
            Fault::Explicit(code) => write!(f, "explicit trap (code {code})"),
        }
    }
}

impl std::error::Error for Fault {}