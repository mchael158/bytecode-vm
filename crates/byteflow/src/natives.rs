//! Standard native (FFI) table shipped with the facade.
//!
//! # Why Message make/unpack lives here (not as new opcodes)
//!
//! [`crate::Value::Message`] is already a first-class wire/register value
//! (ABI v2). Building and tearing it apart in bytecode could be ISA ops
//! (`MakeMessage`, `MsgSender`, …), but that would burn opcode space and
//! force every embedder that never does request-reply to carry the
//! dispatch cost. Natives keep the envelope **generic in the core** while
//! the std table opts into the helpers — same pattern as `print` / `now_ms`
//! (design notes §30-31): host Rust owns the operation, bytecode only
//! picks a slot.
//!
//! # Stable indices are an embed contract
//!
//! Flash / separately-built `.bf` modules hard-code `CallNative` indices.
//! Renumbering an existing slot is a **breaking** change. Keep
//! [`std_native_map`] and [`std_native_table`] in lockstep; append only.
//!
//! | Index | Name | Role |
//! |------:|------|------|
//! | 0 | `print` | host stdout log line (flow-visible) |
//! | 1 | `now_ms` | wall-clock millis as `Value::Int` |
//! | 2 | `make_msg` | build [`crate::Message`] from four scalars (**untrusted** `sender`) |
//! | 3 | `msg_sender` | extract `sender` → `Value::Pid` (authenticated **after** delivery) |
//! | 4 | `msg_request_id` | extract `request_id` → `Value::Int` |
//! | 5 | `msg_tag` | extract `tag` → `Value::Int` |
//! | 6 | `msg_payload` | extract `payload` → `Value::Int` |
//! | 7 | `msg_reply_cap` | extract `reply_cap` → `Value::Cap` (SEND grant) |
//!
//! # `CallNative` argument layout
//!
//! `CallNative ra, fb, nc` takes args from `r[a .. a+nc]` and writes the
//! result back into `r[a]` — so a call **destroys** the first argument
//! register. Samples that need to keep a `Message` while unpacking must
//! `Move` it into the dest register first (see
//! [`crate::samples::atomic_request_reply`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bytecode::{Message, Value};
use crate::vm::{expect_message, expect_u64, Fault, NativeTable};

/// Stable `CallNative` indices for [`std_native_table`]. Bytecode embeds these
/// once assembled — append only; never renumber.
pub mod std_native {
    pub const PRINT: u32 = 0;
    pub const NOW_MS: u32 = 1;
    pub const MAKE_MSG: u32 = 2;
    pub const MSG_SENDER: u32 = 3;
    pub const MSG_REQUEST_ID: u32 = 4;
    pub const MSG_TAG: u32 = 5;
    pub const MSG_PAYLOAD: u32 = 6;
    pub const MSG_REPLY_CAP: u32 = 7;
}

/// Name → `CallNative` index for documentation / host-side lookups.
///
/// Prefer this (or [`std_natives`]) over hard-coding integers in host code
/// so renames stay discoverable; bytecode itself still embeds the numeric
/// slot once assembled.
pub fn std_native_map() -> HashMap<String, u32> {
    HashMap::from([
        ("print".to_owned(), 0),
        ("now_ms".to_owned(), 1),
        ("make_msg".to_owned(), 2),
        ("msg_sender".to_owned(), 3),
        ("msg_request_id".to_owned(), 4),
        ("msg_tag".to_owned(), 5),
        ("msg_payload".to_owned(), 6),
        ("msg_reply_cap".to_owned(), 7),
    ])
}

/// Default FFI table: print, clock, and Message make/unpack.
///
/// Pass this to [`crate::Runtime::with_natives`] (or
/// `with_natives_and_config`) whenever the chunk emits the Message helpers
/// or `print` / `now_ms`. Chunks that never `CallNative` can keep
/// [`crate::NativeTable::empty`].
pub fn std_native_table() -> Arc<NativeTable> {
    let built = NativeTable::builder()
        .register("print", |args| {
            // Space-separated Display forms, trailing newline — mirrors a
            // tiny "println!("{:?}", …)" for bytecode without allocating a
            // format string in the VM.
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
        })
        .and_then(|b| {
            b.register("now_ms", |_| {
                let duration = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| Fault::NativeError(format!("system clock error: {e}")))?;
                let millis = i64::try_from(duration.as_millis())
                    .map_err(|_| Fault::NativeError("system clock value exceeds i64".into()))?;
                Ok(Value::Int(millis))
            })
        })
        .and_then(|b| {
            b.register("make_msg", |args| {
                // Args: sender, request_id, tag, payload — each Int≥0, Pid, Cap, or Bool.
                // Tag must fit `u16` (protocol discriminator width on the wire).
                //
                // # Security (crates.io contract)
                //
                // The first argument is retained so existing `.bf` modules and
                // samples keep a stable CallNative layout (indices 0–6 frozen;
                // 7 = msg_reply_cap appended). It is **not** an authentication
                // primitive:
                //
                // - Before `Send` / `Ask`, `sender` is ordinary register data.
                // - At delivery, the worker stamps `Message.sender` and mints
                //   `reply_cap` (`Message::authenticate`).
                // - After a hop is received, `msg_sender` / `msg_reply_cap`
                //   reflect runtime identity and the SEND grant (S1 + FlowCap).
                //
                // Host code that only builds messages in memory (never sends)
                // still sees the constructed field unchanged.
                let sender = expect_u64(args, 0, "make_msg")?;
                let request_id = expect_u64(args, 1, "make_msg")?;
                let tag = expect_u64(args, 2, "make_msg")?;
                let payload = expect_u64(args, 3, "make_msg")?;
                let tag = u16::try_from(tag).map_err(|_| {
                    Fault::NativeError(format!("make_msg: tag {tag} does not fit in u16"))
                })?;
                Ok(Value::Message(Message::new(sender, request_id, tag, payload)))
            })
        })
        .and_then(|b| {
            b.register("msg_sender", |args| {
                // After mailbox delivery this is the runtime-stamped origin.
                // Identity only — not a Send/Ask address (use `msg_reply_cap`).
                Ok(Value::Pid(expect_message(args, 0, "msg_sender")?.sender))
            })
        })
        .and_then(|b| {
            b.register("msg_request_id", |args| {
                Ok(Value::Int(
                    expect_message(args, 0, "msg_request_id")?.request_id as i64,
                ))
            })
        })
        .and_then(|b| {
            b.register("msg_tag", |args| {
                Ok(Value::Int(i64::from(expect_message(args, 0, "msg_tag")?.tag)))
            })
        })
        .and_then(|b| {
            b.register("msg_payload", |args| {
                Ok(Value::Int(
                    expect_message(args, 0, "msg_payload")?.payload as i64,
                ))
            })
        })
        .and_then(|b| {
            b.register("msg_reply_cap", |args| {
                Ok(Value::Cap(
                    expect_message(args, 0, "msg_reply_cap")?.reply_cap,
                ))
            })
        });
    // Unique sequential names cannot hit DuplicateName / SlotOccupied.
    // Empty table on Err: fail-closed (CallNative → BadNative), never panic.
    match built {
        Ok(b) => b.build(),
        Err(_) => NativeTable::empty(),
    }
}

/// Build the default native table and its matching name map together.
///
/// Prefer this when the host both registers natives and resolves names to
/// indices for `ChunkBuilder` — one source of truth, no drift between map
/// and table.
pub fn std_natives() -> (Arc<NativeTable>, HashMap<String, u32>) {
    let table = std_native_table();
    let map: HashMap<String, u32> = table
        .names()
        .filter_map(|n| table.index_of(n).map(|i| (n.to_string(), i)))
        .collect();
    (table, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_native_indices_are_stable() {
        let map = std_native_map();
        assert_eq!(map.get("print"), Some(&0));
        assert_eq!(map.get("now_ms"), Some(&1));
        assert_eq!(map.get("make_msg"), Some(&2));
        assert_eq!(map.get("msg_sender"), Some(&3));
        assert_eq!(map.get("msg_request_id"), Some(&4));
        assert_eq!(map.get("msg_tag"), Some(&5));
        assert_eq!(map.get("msg_payload"), Some(&6));
        assert_eq!(map.get("msg_reply_cap"), Some(&7));
    }

    #[test]
    fn std_native_table_matches_map() {
        let (table, map) = std_natives();
        assert_eq!(table.len(), 8);
        for (name, idx) in &map {
            assert_eq!(table.index_of(name), Some(*idx));
        }
    }

    #[test]
    fn now_ms_returns_non_negative_int() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table();
        let now_ms = table.get(1).ok_or("now_ms native")?;
        let value = now_ms(&[])?;
        assert!(matches!(value, Value::Int(ms) if ms >= 0));
        Ok(())
    }

    #[test]
    fn make_msg_and_unpack_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table();
        let make = table.get(2).ok_or("make_msg")?;
        let msg = make(&[
            Value::Pid(9),
            Value::Int(3),
            Value::Int(7),
            Value::Int(42),
        ])?;
        assert_eq!(msg.as_message(), Some(Message::new(9, 3, 7, 42)));
        let sender = table.get(3).ok_or("msg_sender")?;
        let req = table.get(4).ok_or("msg_request_id")?;
        let tag = table.get(5).ok_or("msg_tag")?;
        let payload = table.get(6).ok_or("msg_payload")?;
        let cap = table.get(7).ok_or("msg_reply_cap")?;
        assert_eq!(sender(std::slice::from_ref(&msg))?, Value::Pid(9));
        assert_eq!(req(std::slice::from_ref(&msg))?, Value::Int(3));
        assert_eq!(tag(std::slice::from_ref(&msg))?, Value::Int(7));
        assert_eq!(payload(std::slice::from_ref(&msg))?, Value::Int(42));
        assert_eq!(cap(std::slice::from_ref(&msg))?, Value::Cap(0));
        Ok(())
    }

    #[test]
    fn print_accepts_all_current_values() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table();
        let print = table.get(0).ok_or("print native")?;
        let values = [
            Value::Unit,
            Value::Bool(true),
            Value::Int(42),
            Value::Float(1.5),
            Value::Pid(7),
            Value::Message(Message::new(1, 2, 3, 4)),
            Value::Cap(9),
            Value::str("hello"),
            Value::bytes([1u8, 2, 3]),
        ];
        assert_eq!(print(&values)?, Value::Unit);
        Ok(())
    }

    #[test]
    fn print_with_no_args_still_returns_unit() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table();
        let print = table.get(0).ok_or("print native")?;
        assert_eq!(print(&[])?, Value::Unit);
        Ok(())
    }
}
