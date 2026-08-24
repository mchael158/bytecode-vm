use std::sync::Arc;

use crate::bytecode::Value;

use super::fault::Fault;

/// Result type for a native (FFI) function.
pub type NativeResult = Result<Value, Fault>;

/// A host-side function callable from bytecode via `Opcode::CallNative`.
///
/// Must not block: it runs inline on a worker thread. Long I/O belongs in
/// the embedder outside the quantum, not here.
pub type NativeFn = Arc<dyn Fn(&[Value]) -> NativeResult + Send + Sync>;

/// Table of native functions indexed by `CallNative`'s `imm` operand.
///
/// Shared across every process spawned from the same [`crate::Vm`] /
/// runtime so a child sees the same FFI surface as its parent.
#[derive(Clone, Default)]
pub struct NativeTable {
    fns: Vec<NativeFn>,
}

impl NativeTable {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self { fns: Vec::new() })
    }

    pub fn len(&self) -> usize {
        self.fns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fns.is_empty()
    }

    pub fn get(&self, index: u32) -> Option<&NativeFn> {
        self.fns.get(index as usize)
    }
}

/// Builder for a [`NativeTable`]. Indices are assigned in registration order.
#[derive(Default)]
pub struct NativeTableBuilder {
    fns: Vec<NativeFn>,
}

impl NativeTableBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `f` and return its stable index for `emit_call_native`.
    pub fn register<F>(&mut self, f: F) -> u32
    where
        F: Fn(&[Value]) -> NativeResult + Send + Sync + 'static,
    {
        let idx = self.fns.len() as u32;
        self.fns.push(Arc::new(f));
        idx
    }

    pub fn build(self) -> Arc<NativeTable> {
        Arc::new(NativeTable { fns: self.fns })
    }
}

/// Helper: require `args[i]` to exist.
pub fn expect_arg(args: &[Value], i: usize) -> Result<&Value, Fault> {
    args.get(i).ok_or_else(|| {
        Fault::NativeError(format!("expected argument at index {i}, got {} args", args.len()))
    })
}

/// Helper: require `args[i]` to be an `Int`.
pub fn expect_int(args: &[Value], i: usize) -> Result<i64, Fault> {
    match expect_arg(args, i)? {
        Value::Int(n) => Ok(*n),
        other => Err(Fault::TypeMismatch {
            expected: "int",
            got: other.type_name(),
        }),
    }
}
