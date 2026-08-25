//! Built-in demo chunks assembled with [`crate::ChunkBuilder`].
//!
//! These are the runtime's "hello world" suite: scalar arithmetic, a
//! multi-message ping-pong, an atomic [`crate::Value::Message`]
//! request-reply, and a deliberate `Trap` for supervisor demos. They are
//! also the encode/decode round-trip fixtures in the unit tests below.

use crate::{Chunk, ChunkBuilder, Opcode};

/// Native indices (must match [`crate::std_native_map`]).
///
/// Hard-coded here so the sample stays self-contained without looking up
/// the map at assembly time — the stability test in `natives` guards drift.
const N_PRINT: u32 = 0;
const N_MAKE_MSG: u32 = 2;
const N_MSG_SENDER: u32 = 3;
const N_MSG_REQUEST_ID: u32 = 4;
const N_MSG_TAG: u32 = 5;
const N_MSG_PAYLOAD: u32 = 6;

/// Protocol tags for the atomic request-reply sample.
///
/// Opaque to the VM; only this sample (and its clients) interpret them.
pub const TAG_REQ: i32 = 1;
pub const TAG_REP: i32 = 2;

/// `r0 = 41 + 1; return r0` — the 60-second sanity chunk.
pub fn add_forty_two() -> Chunk {
    let mut b = ChunkBuilder::new("add-forty-two");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 41);
    b.emit_load_imm(1, 1);
    b.emit_binop(Opcode::Add, 0, 0, 1);
    b.emit_return(0);
    b.finish()
}

/// Two processes, one mailbox round-trip: main sends `1` to `pong`,
/// `pong` replies `2`, main returns it.
///
/// Protocol (scalar messages only — pre-`Message` style):
/// 1. main reads its own Pid (`SelfPid`) and spawns `pong`
/// 2. main sends that Pid, then the integer `1` as **two** mailbox values
/// 3. pong receives both, increments, sends `2` back
///
/// Prefer [`atomic_request_reply`] for anything that needs correlation or
/// a single hop; this sample exists to keep the historical two-send
/// handshake working and documented.
pub fn ping_pong() -> Chunk {
    let mut b = ChunkBuilder::new("ping-pong");

    let pong = b.begin_function("pong", 0, 3);
    b.emit_receive(0);
    b.emit_receive(1);
    b.emit_load_imm(2, 1);
    b.emit_binop(Opcode::Add, 1, 1, 2);
    b.emit_send(0, 1);
    b.emit_exit(1);

    b.begin_function("main", 0, 4);
    b.emit_self_pid(1);
    b.emit_spawn(0, pong, 0);
    b.emit_send(0, 1);
    b.emit_load_imm(2, 1);
    b.emit_send(0, 2);
    b.emit_receive(3);
    b.emit_return(3);

    b.finish()
}

/// Atomic request-reply with [`crate::Value::Message`] (one envelope per hop).
///
/// Requires [`crate::std_native_table`] (`make_msg` / `msg_*` / `print`).
///
/// # Why "atomic"
///
/// Correlation (`request_id`) and reply routing (`sender`) travel in a
/// **single** mailbox value. Contrast [`ping_pong`], which needs two
/// receives before it even knows who to answer. That two-message dance is
/// easy to get wrong under concurrency; an envelope makes the hop one
/// critical section on the receiver's mailbox (see [`crate::Mailbox`]).
///
/// # Protocol
///
/// 1. `main` spawns `server`, builds
///    `Message { sender=self, id=1, tag=REQ, payload=41 }`
/// 2. `server` receives, logs via `print`, replies
///    `tag=REP, payload=42` to `sender` (echoing `request_id`)
/// 3. `main` returns the reply payload `42`
///
/// # Register discipline (`CallNative` clobbers `r[a]`)
///
/// `CallNative ra, …, nc` reads args from `r[a..a+nc]` and writes the
/// result into `r[a]`. Keeping the original `Message` in `r0` therefore
/// means every unpack is `Move ri, r0` then `CallNative ri, msg_*, 1`.
/// `make_msg` needs four **contiguous** arg registers; the server rearranges
/// into `r4..r7` before the call.
pub fn atomic_request_reply() -> Chunk {
    let mut b = ChunkBuilder::new("atomic-request-reply");

    // --- server -----------------------------------------------------------
    // r0  = request Message (never overwritten until Exit)
    // r1  = client Pid (from msg_sender)
    // r2  = request_id
    // r3  = tag (unused after unpack; kept for symmetry / future guards)
    // r4  = payload, then make_msg result (reply Message)
    // r5  = self Pid, then make_msg arg slot
    // r6  = TAG_REP for make_msg
    // r7  = scratch (print copy, +1 imm, payload for make_msg)
    let server = b.begin_function("server", 0, 8);
    b.emit_receive(0);
    // Log without destroying r0: copy then print (print clobbers its dest).
    b.emit_move(7, 0);
    b.emit_call_native(7, N_PRINT, 1);
    b.emit_move(1, 0);
    b.emit_call_native(1, N_MSG_SENDER, 1);
    b.emit_move(2, 0);
    b.emit_call_native(2, N_MSG_REQUEST_ID, 1);
    b.emit_move(3, 0);
    b.emit_call_native(3, N_MSG_TAG, 1);
    b.emit_move(4, 0);
    b.emit_call_native(4, N_MSG_PAYLOAD, 1);
    // payload + 1 → reply body
    b.emit_load_imm(7, 1);
    b.emit_binop(Opcode::Add, 4, 4, 7);
    b.emit_self_pid(5);
    // Contiguous make_msg args at r4..r7: self, request_id, TAG_REP, payload+1
    b.emit_move(7, 4);
    b.emit_move(4, 5);
    b.emit_move(5, 2);
    b.emit_load_imm(6, TAG_REP);
    b.emit_call_native(4, N_MAKE_MSG, 4);
    b.emit_move(7, 4);
    b.emit_call_native(7, N_PRINT, 1);
    b.emit_send(1, 4);
    b.emit_exit(4);

    // --- main -------------------------------------------------------------
    // r0 = server Pid
    // r1 = self Pid
    // r2..r5 = make_msg(self, 1, TAG_REQ, 41) → r2 becomes the request Message
    // r6 = reply Message
    // r7 = scratch for print / unpack
    b.begin_function("main", 0, 8);
    b.emit_self_pid(1);
    b.emit_spawn(0, server, 0);
    b.emit_move(2, 1);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_REQ);
    b.emit_load_imm(5, 41);
    b.emit_call_native(2, N_MAKE_MSG, 4);
    b.emit_move(7, 2);
    b.emit_call_native(7, N_PRINT, 1);
    b.emit_send(0, 2);
    b.emit_receive(6);
    b.emit_move(7, 6);
    b.emit_call_native(7, N_PRINT, 1);
    b.emit_move(4, 6);
    b.emit_call_native(4, N_MSG_PAYLOAD, 1);
    b.emit_return(4);

    b.finish()
}

/// Immediate `Trap` — used to show [`crate::Supervisor`] restart.
pub fn boom() -> Chunk {
    let mut b = ChunkBuilder::new("boom");
    b.begin_function("boom", 0, 1);
    b.emit_trap(1);
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decode, encode, std_native_table, verify, ProcessOutcome, Runtime, RuntimeConfig, Value,
    };

    fn tiny(chunk: Chunk) -> Runtime {
        Runtime::with_config(
            chunk,
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
            },
        )
        .expect("runtime")
    }

    fn tiny_natives(chunk: Chunk) -> Runtime {
        Runtime::with_natives_and_config(
            chunk,
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
            },
        )
        .expect("runtime")
    }

    #[test]
    fn add_forty_two_joins_42() {
        let rt = tiny(add_forty_two());
        let idx = rt.function_index("main").expect("main");
        let outcome = rt.spawn(idx, &[]).expect("spawn").join();
        rt.shutdown();
        assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(42))));
    }

    #[test]
    fn ping_pong_joins_2() {
        let chunk = ping_pong();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes).expect("decode");
        let rt = tiny(chunk);
        let idx = rt.function_index("main").expect("main");
        let outcome = rt.spawn(idx, &[]).expect("spawn").join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(2))));
        assert!(sent >= 2);
    }

    #[test]
    fn atomic_request_reply_joins_42() {
        let chunk = atomic_request_reply();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes).expect("decode");
        let rt = tiny_natives(chunk);
        let idx = rt.function_index("main").expect("main");
        let outcome = rt.spawn(idx, &[]).expect("spawn").join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(
            matches!(outcome, ProcessOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        assert!(sent >= 1);
    }
}
