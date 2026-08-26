//! Thin, deliberately **non-duplicating** `macro_rules!` sugar over
//! [`crate::ChunkBuilder`] methods that emit more than one instruction for
//! a single conceptual operation.
//!
//! # Why a macro on top of a method that already does the job
//!
//! It doesn't add capability — [`ChunkBuilder::emit_native1_from`] and
//! [`ChunkBuilder::emit_native_n`] are already complete methods, and calling
//! them directly (`b.emit_native1_from(1, 0, native)`) works fine. The macros
//! exist only for call sites that read better as a flat list of emit-macro
//! invocations — e.g. unpacking four fields from one message register back
//! to back in [`crate::samples::atomic_request_reply`].
//!
//! Every macro here is a **one-line forward** to the method of the same
//! name. Neither contains logic, a register calculation, or an opcode
//! literal of its own — if the method and the macro ever disagreed, that
//! would be a bug in this file. See
//! [`ChunkBuilder::emit_native1_from`] for the ISA contract (`CallNative`
//! clobbers `r[a]`); this file only explains *why a macro form exists*.
//!
//! Styled after the crate-internal `trap!` pattern in `Vm::run`: small,
//! named after the one thing it does, no general assembler DSL.

/// `emit_native1_from!(builder, dest, src, native)` — sugar for
/// [`crate::ChunkBuilder::emit_native1_from`].
///
/// # Example
/// ```
/// use byteflow::{emit_native1_from, ChunkBuilder};
///
/// let mut b = ChunkBuilder::new("example");
/// b.begin_function("main", 0, 8);
/// b.emit_receive(0);
/// emit_native1_from!(b, 1, 0, /* msg_sender */ 3);
/// emit_native1_from!(b, 2, 0, /* msg_tag */ 5);
/// b.emit_return(0);
/// let chunk = b.finish();
/// assert_eq!(chunk.code.len(), 6); // Receive + (Move,CallNative)*2 + Return
/// ```
#[macro_export]
macro_rules! emit_native1_from {
    ($builder:expr, $dest:expr, $src:expr, $native:expr) => {
        $builder.emit_native1_from($dest, $src, $native)
    };
}

/// `emit_native_n!(builder, base, native, argc)` — sugar for
/// [`crate::ChunkBuilder::emit_native_n`].
///
/// # Example
/// ```
/// use byteflow::{emit_native_n, ChunkBuilder};
///
/// let mut b = ChunkBuilder::new("example");
/// b.begin_function("main", 0, 8);
/// b.emit_load_imm(1, 7);
/// b.emit_load_imm(2, 1);
/// b.emit_load_imm(3, 0);
/// b.emit_load_imm(4, 42);
/// emit_native_n!(b, 1, /* make_msg */ 2, 4);
/// b.emit_return(1);
/// let chunk = b.finish();
/// assert_eq!(chunk.code.len(), 6);
/// ```
#[macro_export]
macro_rules! emit_native_n {
    ($builder:expr, $base:expr, $native:expr, $argc:expr) => {
        $builder.emit_native_n($base, $native, $argc)
    };
}

/// Namespaced re-exports (`byteflow::asm_macros::emit_native1_from!`, …)
/// for callers who prefer an explicit path over crate-root
/// `#[macro_export]` placement.
pub mod asm_macros {
    pub use crate::emit_native1_from;
    pub use crate::emit_native_n;
}
