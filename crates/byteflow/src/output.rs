//! Host-side output for the `print` native.
//!
//! Bytecode must not own stdout. Embedders inject an [`OutputSink`]; the
//! default is [`NullSink`] so a library load is silent. CLI demos pass
//! [`StdoutSink`].

use crate::bytecode::Value;

/// Receives `print` native arguments. Must not panic or block.
pub trait OutputSink: Send + Sync + std::fmt::Debug + 'static {
    fn write(&self, values: &[Value]);
}

/// Discard output (default for embedders and tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl OutputSink for NullSink {
    fn write(&self, _values: &[Value]) {}
}

/// Space-separated `Display` forms plus a newline on stdout.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    fn write(&self, values: &[Value]) {
        let mut first = true;
        for value in values {
            if !first {
                print!(" ");
            }
            print!("{value}");
            first = false;
        }
        println!();
    }
}
