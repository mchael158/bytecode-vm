//! Thin `macro_rules!` sugar over crate-internal [`ChunkBuilder`] helpers.
//!
//! Prefer [`crate::Fn::native1_from`] and [`crate::Fn::native_n`] in new code.
//! These macros remain for internal tests that emit bytecode directly.

/// `emit_native1_from!(builder, dest, src, native)` — see [`Fn::native1_from`].
///
/// # Example
/// ```
/// use byteflow::Program;
///
/// let mut p = Program::new("example");
/// p.function("main", 0, |f| {
///     let msg = f.receive();
///     let sender = f.native1_from(msg, 3);
///     f.return_(sender);
/// });
/// let chunk = p.build();
/// assert!(chunk.code.len() >= 4);
/// ```
#[macro_export]
macro_rules! emit_native1_from {
    ($builder:expr, $dest:expr, $src:expr, $native:expr) => {
        $builder.emit_native1_from($dest, $src, $native)
    };
}

/// `emit_native_n!(builder, base, native, argc)` — see [`Fn::native_n`].
#[macro_export]
macro_rules! emit_native_n {
    ($builder:expr, $base:expr, $native:expr, $argc:expr) => {
        $builder.emit_native_n($base, $native, $argc)
    };
}

/// Namespaced re-exports for callers who prefer an explicit path.
pub mod asm_macros {
    pub use crate::emit_native1_from;
    pub use crate::emit_native_n;
}
