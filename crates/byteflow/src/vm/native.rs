//! Native (FFI) table for `Opcode::CallNative`.
//!
//! # The one rule: never block
//!
//! `CallNative` runs **inline** on the worker thread. A native that blocks
//! stalls every other Flow on that worker. Slow I/O belongs in a dedicated
//! Flow (`Send`/`Receive`), not here.

use std::sync::Arc;

use crate::bytecode::Value;

use super::fault::Fault;

/// Result type for a native function.
pub type NativeResult = Result<Value, Fault>;

/// A host-side function callable from bytecode via `Opcode::CallNative`.
pub type NativeFn = Arc<dyn Fn(&[Value]) -> NativeResult + Send + Sync>;

/// Immutable, indexable set of natives. Gaps left by [`NativeTableBuilder::register_at`]
/// are `None` — calling them faults with [`Fault::BadNative`].
pub struct NativeTable {
    entries: Vec<Option<(String, NativeFn)>>,
}

impl NativeTable {
    pub fn builder() -> NativeTableBuilder {
        NativeTableBuilder {
            entries: Vec::new(),
        }
    }

    /// Empty table — valid for chunks that never emit `CallNative`.
    pub fn empty() -> Arc<NativeTable> {
        Arc::new(NativeTable {
            entries: Vec::new(),
        })
    }

    #[inline]
    pub fn get(&self, index: u32) -> Option<&NativeFn> {
        self.entries
            .get(index as usize)
            .and_then(|slot| slot.as_ref())
            .map(|(_, f)| f)
    }

    pub fn index_of(&self, name: &str) -> Option<u32> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.as_ref().is_some_and(|(n, _)| n == name))
            .map(|(i, _)| i as u32)
    }

    /// Slot count including reserved-but-empty gaps (one past the highest
    /// touched index), not the count of registered functions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(n, _)| n.as_str()))
    }
}

/// Fluent builder for a [`NativeTable`].
///
/// - [`Self::register`] — next sequential slot (host builds table + chunk together).
/// - [`Self::register_at`] — fixed ABI slot (MCU / separately flashed bytecode).
pub struct NativeTableBuilder {
    entries: Vec<Option<(String, NativeFn)>>,
}

impl NativeTableBuilder {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register `f` under `name` at the next sequential slot.
    /// Panics on duplicate `name`.
    pub fn register<F>(self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&[Value]) -> NativeResult + Send + Sync + 'static,
    {
        let index = self.entries.len() as u32;
        self.register_at(index, name, f)
    }

    /// Register `f` under `name` at explicit `index`, padding lower gaps as
    /// unregistered (`get` → `None` → [`Fault::BadNative`]).
    ///
    /// Panics if the slot is occupied or `name` is already registered.
    pub fn register_at<F>(mut self, index: u32, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(&[Value]) -> NativeResult + Send + Sync + 'static,
    {
        let name = name.into();
        assert!(
            !self
                .entries
                .iter()
                .any(|slot| slot.as_ref().is_some_and(|(n, _)| n == &name)),
            "byteflow: duplicate native function registered: '{name}'"
        );
        let index = index as usize;
        if index >= self.entries.len() {
            self.entries.resize_with(index + 1, || None);
        }
        assert!(
            self.entries[index].is_none(),
            "byteflow: native slot {index} already occupied (registering '{name}')"
        );
        self.entries[index] = Some((name, Arc::new(f)));
        self
    }

    pub fn build(self) -> Arc<NativeTable> {
        Arc::new(NativeTable {
            entries: self.entries,
        })
    }
}

impl Default for NativeTableBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Require `args[index]` to exist.
pub fn expect_arg<'a>(
    args: &'a [Value],
    index: usize,
    fn_name: &str,
) -> Result<&'a Value, Fault> {
    args.get(index).ok_or_else(|| {
        Fault::NativeError(format!("{fn_name}: missing argument {index}"))
    })
}

/// Require `args[index]` to coerce to an int (`Value::as_int`).
pub fn expect_int(args: &[Value], index: usize, fn_name: &str) -> Result<i64, Fault> {
    expect_arg(args, index, fn_name)?
        .as_int()
        .ok_or_else(|| {
            Fault::NativeError(format!("{fn_name}: argument {index} is not an int"))
        })
}

/// Require `args[index]` to be a bool (ints: nonzero = true).
pub fn expect_bool(args: &[Value], index: usize, fn_name: &str) -> Result<bool, Fault> {
    match expect_arg(args, index, fn_name)? {
        Value::Bool(b) => Ok(*b),
        Value::Int(i) => Ok(*i != 0),
        other => Err(Fault::NativeError(format!(
            "{fn_name}: argument {index} is not a bool/int (got {})",
            other.type_name()
        ))),
    }
}

/// Require `args[index]` to be a [`crate::Message`].
///
/// Used by the std `msg_*` natives. A wrong type becomes
/// [`Fault::NativeError`] (category B — Flow fault), not a host panic.
pub fn expect_message(
    args: &[Value],
    index: usize,
    fn_name: &str,
) -> Result<crate::Message, Fault> {
    expect_arg(args, index, fn_name)?
        .as_message()
        .ok_or_else(|| {
            Fault::NativeError(format!("{fn_name}: argument {index} is not a message"))
        })
}

/// Coerce `args[index]` to `u64` from `Int` (≥ 0), `Pid`, `Cap`, or `Bool`.
///
/// `make_msg` accepts these so bytecode can pass a `SelfPid` / Spawn Cap
/// result, a `Pid` identity, or a `LoadImm` without an extra conversion.
/// Negative ints are rejected — envelope fields are unsigned on the wire.
pub fn expect_u64(args: &[Value], index: usize, fn_name: &str) -> Result<u64, Fault> {
    match expect_arg(args, index, fn_name)? {
        Value::Pid(p) => Ok(*p),
        Value::Cap(c) => Ok(*c),
        Value::Int(i) if *i >= 0 => Ok(*i as u64),
        Value::Bool(b) => Ok(u64::from(*b)),
        other => Err(Fault::NativeError(format!(
            "{fn_name}: argument {index} is not a non-negative int/pid/cap (got {})",
            other.type_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_at_leaves_holes_as_none() {
        let table = NativeTable::builder()
            .register_at(10, "answer", |_| Ok(Value::Int(42)))
            .build();
        assert_eq!(table.len(), 11);
        assert_eq!(table.index_of("answer"), Some(10));
        assert!(table.get(10).unwrap()(&[]).unwrap() == Value::Int(42));
        assert!(table.get(2).is_none());
    }

    #[test]
    #[should_panic(expected = "already occupied")]
    fn register_at_panics_on_duplicate_slot() {
        let _ = NativeTable::builder()
            .register_at(3, "a", |_| Ok(Value::Unit))
            .register_at(3, "b", |_| Ok(Value::Unit));
    }

    #[test]
    #[should_panic(expected = "duplicate native")]
    fn register_panics_on_duplicate_name() {
        let _ = NativeTable::builder()
            .register("x", |_| Ok(Value::Unit))
            .register("x", |_| Ok(Value::Unit));
    }
}
