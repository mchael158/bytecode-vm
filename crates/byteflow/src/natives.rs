//! Standard native (FFI) table shipped with the facade.
//!
//! Indices are part of the embed contract: keep [`std_native_map`] and
//! [`std_native_table`] in lockstep. Never iterate a `HashMap` to decide
//! registration order — `HashMap` is unordered.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bytecode::Value;
use crate::vm::{Fault, NativeTable, NativeTableBuilder};

/// Name → CallNative index for documentation / host-side lookups.
/// Kept in lockstep with [`std_native_table`].
///
/// Stable indices: `print = 0`, `now_ms = 1`.
/// Use these with [`crate::bytecode::ChunkBuilder::emit_call_native`].
pub fn std_native_map() -> HashMap<String, u32> {
    HashMap::from([("print".to_owned(), 0), ("now_ms".to_owned(), 1)])
}

/// Default FFI table: index 0 = `print`, index 1 = `now_ms`.
pub fn std_native_table() -> Arc<NativeTable> {
    let mut builder = NativeTableBuilder::new();

    // 0 — print
    builder.register(|args| {
        let mut first = true;
        for value in args {
            if !first {
                print!(" ");
            }
            print!("{value}");
            first = false;
        }
        println!();
        Ok(Value::Unit)
    });

    // 1 — now_ms
    builder.register(|_| {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Fault::NativeError(format!("system clock error: {e}")))?;
        let millis = i64::try_from(duration.as_millis())
            .map_err(|_| Fault::NativeError("system clock value exceeds i64".into()))?;
        Ok(Value::Int(millis))
    });

    builder.build()
}

/// Build the default native table and its matching compiler map together.
pub fn std_natives() -> (Arc<NativeTable>, HashMap<String, u32>) {
    (std_native_table(), std_native_map())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_native_indices_are_stable() {
        let map = std_native_map();
        assert_eq!(map["print"], 0);
        assert_eq!(map["now_ms"], 1);
    }

    #[test]
    fn std_native_table_matches_map() {
        let (table, map) = std_natives();
        assert_eq!(table.len(), 2);
        assert_eq!(map["print"], 0);
        assert_eq!(map["now_ms"], 1);
    }

    #[test]
    fn now_ms_returns_non_negative_int() {
        let table = std_native_table();
        let now_ms = table.get(1).expect("now_ms native");
        let value = now_ms(&[]).expect("clock read");
        assert!(matches!(value, Value::Int(ms) if ms >= 0));
    }

    #[test]
    fn print_accepts_all_current_values() {
        let table = std_native_table();
        let print = table.get(0).expect("print native");
        let values = [
            Value::Unit,
            Value::Bool(true),
            Value::Int(42),
            Value::Float(1.5),
            Value::Pid(7),
        ];
        assert_eq!(print(&values).unwrap(), Value::Unit);
    }

    #[test]
    fn print_with_no_args_still_returns_unit() {
        let table = std_native_table();
        let print = table.get(0).expect("print native");
        assert_eq!(print(&[]).unwrap(), Value::Unit);
    }
}
