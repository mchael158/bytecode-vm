//! `byteflow-bytecode` — instruction set, module format, assembler and
//! static verifier for the Byteflow virtual-process VM.
//!
//! This crate has **no I/O, no threading, no allocator tricks** — it is
//! pure data + pure functions, deliberately, so it can be reused by tooling
//! (`byteflow-cli disasm`, a compiler backend, the debugger in
//! design notes §27) without dragging in the scheduler or runtime.
#![forbid(unsafe_code)]

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
