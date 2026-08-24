//! Instruction set, `.bf` (BFV0) format, assembler (`ChunkBuilder`) and verifier.
//! Pure data — no I/O, no threads.

mod builder;
mod chunk;
mod disasm;
mod format;
mod instruction;
mod opcode;
mod value;
mod verify;

pub use builder::{ChunkBuilder, Label};
pub use chunk::{Chunk, FunctionDef, ABI_VERSION, MAGIC};
pub use disasm::disassemble;
pub use format::{decode, encode, FormatError};
pub use instruction::Instruction;
pub use opcode::Opcode;
pub use value::Value;
pub use verify::{verify, VerifyError};
