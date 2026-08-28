use super::chunk::Chunk;
use super::opcode::Opcode;
use std::fmt;

/// Why a [`Chunk`] failed verification.
///
/// The verifier's job is to make every fact the interpreter's hot loop
/// relies on (jump targets in range, constant/function indices in range)
/// true *before* a single instruction runs, so `byteflow-vm` never has to
/// re-check them per-step. Skipping this on trusted, compiler-generated
/// bytecode is fine; it is mandatory before loading anything that crossed a
/// trust boundary (a plugin, a network-fetched module, see design notes
/// §24 capability/sandboxing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    UnknownOpcode { at: usize, byte: u8 },
    ConstOutOfRange { at: usize, index: u32, len: usize },
    FunctionOutOfRange { at: usize, index: u32, len: usize },
    JumpOutOfRange { at: usize, target: i64, len: usize },
    EmptyFunctionTable,
    EntryOutOfRange { function: usize, entry: u32, len: usize },
    /// A function declares more parameters than it has registers to hold
    /// them. The VM loads `r0..arity` on entry, so this makes the very first
    /// thing a call does — copying arguments in — reach past the register
    /// file. Cheap to settle here: it is a static property of the function
    /// table, one comparison per function, and no amount of runtime checking
    /// makes such a function callable.
    ArityExceedsRegisters {
        function: usize,
        arity: u8,
        num_registers: u8,
    },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::UnknownOpcode { at, byte } => {
                write!(f, "unknown opcode 0x{byte:02X} at instruction {at}")
            }
            VerifyError::ConstOutOfRange { at, index, len } => write!(
                f,
                "instruction {at} references constant {index}, pool has {len} entries"
            ),
            VerifyError::FunctionOutOfRange { at, index, len } => write!(
                f,
                "instruction {at} references function {index}, table has {len} entries"
            ),
            VerifyError::JumpOutOfRange { at, target, len } => write!(
                f,
                "instruction {at} jumps to {target}, out of code bounds (len={len})"
            ),
            VerifyError::EmptyFunctionTable => write!(f, "chunk has no entry function"),
            VerifyError::EntryOutOfRange { function, entry, len } => write!(
                f,
                "function {function} entry point {entry} is out of code bounds (len={len})"
            ),
            VerifyError::ArityExceedsRegisters { function, arity, num_registers } => write!(
                f,
                "function {function} declares arity {arity} but only {num_registers} registers"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify structural invariants of `chunk`. See [`VerifyError`] for what is
/// checked. This does **not** perform full dataflow/register-liveness
/// verification (unlike, say, the JVM verifier) — v0 trades that off
/// against implementation complexity, and instead the VM bounds-checks
/// register indices at runtime (cheap: it's an array index against a fixed
/// small register file, not worth statically proving away yet).
///
/// The one register fact that *is* settled statically is
/// [`VerifyError::ArityExceedsRegisters`]. It belongs here rather than in
/// the VM because it is a property of the function table, not of an
/// execution: such a function cannot be entered at all, so letting it reach
/// the interpreter only moves the same rejection later and per call.
///
/// `Opcode::CallNative` targets are deliberately **not** range-checked
/// here: native functions live in a `byteflow_vm::NativeTable` supplied by
/// the embedder at `Vm` construction time, entirely outside this crate's
/// (and this `Chunk`'s) knowledge. An out-of-range `CallNative` is instead
/// caught at runtime as `Fault::BadNative`.
pub fn verify(chunk: &Chunk) -> Result<(), VerifyError> {
    if chunk.functions.is_empty() {
        return Err(VerifyError::EmptyFunctionTable);
    }

    let len = chunk.code.len();

    // `enumerate` rather than looking the index back up by name: two
    // functions may share a name, and a search would then report the wrong
    // one (and cost O(n²) doing it).
    for (function, def) in chunk.functions.iter().enumerate() {
        if def.entry as usize >= len {
            return Err(VerifyError::EntryOutOfRange {
                function,
                entry: def.entry,
                len,
            });
        }
        if def.arity > def.num_registers {
            return Err(VerifyError::ArityExceedsRegisters {
                function,
                arity: def.arity,
                num_registers: def.num_registers,
            });
        }
    }

    for (at, instr) in chunk.code.iter().enumerate() {
        match instr.op {
            Opcode::LoadConst => {
                let idx = instr.imm as u32;
                if idx as usize >= chunk.constants.len() {
                    return Err(VerifyError::ConstOutOfRange {
                        at,
                        index: idx,
                        len: chunk.constants.len(),
                    });
                }
            }
            Opcode::Spawn | Opcode::Call => {
                let idx = instr.imm as u32;
                if idx as usize >= chunk.functions.len() {
                    return Err(VerifyError::FunctionOutOfRange {
                        at,
                        index: idx,
                        len: chunk.functions.len(),
                    });
                }
            }
            // `CallNative` targets a `byteflow_vm::NativeTable` supplied at
            // runtime, outside this chunk — see this function's doc
            // comment. Not checked here; checked as `Fault::BadNative` at
            // call time instead.
            Opcode::CallNative => {}
            Opcode::Jump | Opcode::Branch => {
                let target = at as i64 + 1 + instr.imm as i64;
                if target < 0 || target as usize > len {
                    // == len is allowed: jumping to "one past the end" is a
                    // valid way to fall off the end of a function body.
                    return Err(VerifyError::JumpOutOfRange { at, target, len });
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;
    use crate::bytecode::opcode::Opcode;


    #[test]
    fn rejects_empty_function_table() {
        let chunk = Chunk::default();
        assert_eq!(verify(&chunk), Err(VerifyError::EmptyFunctionTable));
    }

    #[test]
    fn accepts_well_formed_chunk() {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 0, 2);
        let k = b.const_(crate::bytecode::value::Value::Int(41));
        b.emit_load_const(0, k);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    /// A function the VM cannot even enter: entry copies `r0..arity`, but
    /// the frame only has `num_registers` slots. Used to be accepted here and
    /// then panic with an out-of-bounds index inside `Vm::new` / `Call`.
    #[test]
    fn rejects_arity_larger_than_the_register_file() {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 3, 1);
        b.emit_return(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::ArityExceedsRegisters {
                function: 0,
                arity: 3,
                num_registers: 1,
            })
        );
    }

    #[test]
    fn accepts_arity_equal_to_the_register_file() {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 2, 2);
        b.emit_return(0);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    #[test]
    fn rejects_out_of_range_jump() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::only_imm(Opcode::Jump, 999));
        assert!(matches!(verify(&chunk), Err(VerifyError::JumpOutOfRange { .. })));
    }
}