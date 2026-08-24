//! The Byteflow instruction set architecture (ISA v0).
//!
//! Byteflow is register-based (à la Lua 5.x / Dalvik) rather than stack-based
//! (à la JVM/CPython). Register machines need roughly 40-50% fewer dispatched
//! instructions than an equivalent stack machine because they avoid PUSH/POP
//! traffic for every intermediate value, at the cost of slightly larger
//! instruction words. For an interpreter whose steady-state cost is dominated
//! by dispatch (branch prediction + icache misses), fewer instructions per
//! logical operation wins.
//!
//! Every opcode fits in a single byte so a `Vec<Instruction>` is dense and
//! the dispatch table (see `byteflow-vm::interp`) can be a flat jump table
//! indexed directly by discriminant, with no bounds check in release builds
//! (enforced instead at decode/verification time, see [`crate::verify`]).

/// A single Byteflow opcode.
///
/// Numeric values are part of the stable on-disk ABI (`byteflow-bytecode`
/// module format, see [`super::chunk::MAGIC`]) — never renumber an existing
/// variant, only append.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Opcode {
    /// Stop the current process's VM loop. Terminal state.
    Halt = 0x00,

    // ---- data movement ----------------------------------------------------
    /// `LoadConst ra, kb`  →  `r[a] = constants[b]`
    LoadConst = 0x01,
    /// `Move ra, rb`  →  `r[a] = r[b]`
    Move = 0x02,
    /// `LoadImm ra, imm`  →  `r[a] = imm as i64` (fast path, skips const pool)
    LoadImm = 0x03,

    // ---- arithmetic (integer; float variants share the same encoding
    // ---- but operate on Value::Float, selected by the operand's runtime tag)
    Add = 0x10,
    Sub = 0x11,
    Mul = 0x12,
    Div = 0x13,
    Mod = 0x14,
    Neg = 0x15,

    // ---- comparison → writes a Value::Bool into ra
    Eq = 0x18,
    Lt = 0x19,
    Le = 0x1A,

    // ---- control flow -------------------------------------------------
    /// `Jump imm` → unconditional relative jump (imm = signed offset in
    /// instructions from the *next* pc).
    Jump = 0x20,
    /// `Branch ra, imm` → jump by `imm` iff `r[a]` is falsy (Bool(false),
    /// Unit, or Int(0)). This is the only conditional branch; `if/else` and
    /// loops both lower to Branch + Jump, keeping the interpreter's branch
    /// predictor state small.
    Branch = 0x21,

    // ---- procedure calls (native Rust functions registered via FFI, or
    // ---- other bytecode functions in the same chunk) ------------------
    /// `Call ra, fb, nc` → call function `fb` with `nc` arguments taken from
    /// `r[a..a+nc]`, result written back into `r[a]`.
    Call = 0x30,
    /// `Return ra` → return `r[a]` to the caller frame (or complete the
    /// process if this is the outermost frame).
    Return = 0x31,
    /// `CallNative ra, fb, nc` → like `Call` but `fb` indexes the native
    /// function table instead of the bytecode function table.
    CallNative = 0x32,

    // ---- process model --------------------------------------------------
    /// `Spawn ra, fb, nc` → create a new virtual process starting at
    /// function `fb`, passing `nc` arguments taken from `r[a+1..a+1+nc]`
    /// (deliberately *not* overlapping `r[a]` itself, which is where the
    /// new process's `Value::Pid` is written once the scheduler has
    /// created it — see `byteflow_vm::VmResult::Spawn`).
    Spawn = 0x40,
    /// `Yield` → cooperative yield. Control returns to the scheduler, the
    /// process is re-enqueued as `Ready` and may resume on any worker.
    Yield = 0x41,
    /// `Sleep ra` → suspend until `r[a]` (interpreted as milliseconds,
    /// Value::Int) has elapsed. Registered on the timer wheel.
    Sleep = 0x42,
    /// `Exit ra` → terminate the process, `r[a]` is delivered to `.join()`.
    Exit = 0x43,
    /// `SelfPid ra` → `r[a] =` the running process's `Value::Pid`.
    /// The VM does not store its own id (it has no scheduler state); this
    /// is a scheduler effect, same class as `Spawn`/`Receive`.
    SelfPid = 0x44,

    // ---- messaging --------------------------------------------------------
    /// `Send ra, rb` → send `r[b]` to the mailbox of the process whose Pid is
    /// in `r[a]`. Never blocks (mailboxes are unbounded by default, see
    /// `ProcessLimits::max_mailbox` for the bounded variant).
    Send = 0x50,
    /// `Receive ra` → pop the next message into `r[a]`; if the mailbox is
    /// empty, suspends the process in `Waiting` state until a message
    /// arrives.
    Receive = 0x51,
    /// `ReceiveTimeout ra, rb` → like `Receive` but gives up after `r[b]`
    /// milliseconds, writing `Value::Unit` into `r[a]` on timeout.
    ReceiveTimeout = 0x52,

    // ---- diagnostics / safety ------------------------------------------
    /// `Trap imm` → deliberate fault (assertion failure, div-by-zero, bad
    /// opcode encountered by a corrupt/foreign module, capability
    /// violation). Propagates to the process supervisor as `ProcessState::Failed`.
    Trap = 0x60,
    /// `Nop` → no-op, used by the assembler to pad jump targets.
    Nop = 0x61,
}

impl Opcode {
    /// Decode a raw byte into an `Opcode`, used when loading foreign/untrusted
    /// modules. Rejects anything outside the currently defined ISA rather
    /// than transmuting garbage into a jump-table index (which is exactly
    /// the class of bug that turns a VM into a code-execution primitive).
    #[inline]
    pub fn from_u8(byte: u8) -> Option<Opcode> {
        use Opcode::*;
        Some(match byte {
            0x00 => Halt,
            0x01 => LoadConst,
            0x02 => Move,
            0x03 => LoadImm,
            0x10 => Add,
            0x11 => Sub,
            0x12 => Mul,
            0x13 => Div,
            0x14 => Mod,
            0x15 => Neg,
            0x18 => Eq,
            0x19 => Lt,
            0x1A => Le,
            0x20 => Jump,
            0x21 => Branch,
            0x30 => Call,
            0x31 => Return,
            0x32 => CallNative,
            0x40 => Spawn,
            0x41 => Yield,
            0x42 => Sleep,
            0x43 => Exit,
            0x44 => SelfPid,
            0x50 => Send,
            0x51 => Receive,
            0x52 => ReceiveTimeout,
            0x60 => Trap,
            0x61 => Nop,
            _ => return None,
        })
    }

    #[inline]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl std::fmt::Display for Opcode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Opcode::Halt => "Halt",
            Opcode::LoadConst => "LoadConst",
            Opcode::Move => "Move",
            Opcode::LoadImm => "LoadImm",
            Opcode::Add => "Add",
            Opcode::Sub => "Sub",
            Opcode::Mul => "Mul",
            Opcode::Div => "Div",
            Opcode::Mod => "Mod",
            Opcode::Neg => "Neg",
            Opcode::Eq => "Eq",
            Opcode::Lt => "Lt",
            Opcode::Le => "Le",
            Opcode::Jump => "Jump",
            Opcode::Branch => "Branch",
            Opcode::Call => "Call",
            Opcode::Return => "Return",
            Opcode::CallNative => "CallNative",
            Opcode::Spawn => "Spawn",
            Opcode::Yield => "Yield",
            Opcode::Sleep => "Sleep",
            Opcode::Exit => "Exit",
            Opcode::SelfPid => "SelfPid",
            Opcode::Send => "Send",
            Opcode::Receive => "Receive",
            Opcode::ReceiveTimeout => "ReceiveTimeout",
            Opcode::Trap => "Trap",
            Opcode::Nop => "Nop",
        };
        f.write_str(name)
    }
}
