//! Built-in demo chunks assembled with [`crate::ChunkBuilder`].
//!
//! Use these as runnable specs of the messaging contract (and as regression
//! tests). Prefer copying a sample over inventing hop register layouts from
//! scratch.
//!
//! | Sample | Shows |
//! |--------|--------|
//! | [`add_forty_two`] | Scalar VM path (no natives) |
//! | [`ping_pong`] | Cap spawn + Atomic Hop round-trip |
//! | [`atomic_request_reply`] | Tagged REQ/REP + `print` |
//! | [`selective_receive`] | `ReceiveMatch` FIFO skip |
//! | [`ask_reply`] | `Ask` RPC hop |
//! | [`forged_sender_send`] / [`forged_sender_ask`] | S1: forged `make_msg` sender dies |
//! | [`boom`] | Immediate trap (supervisor demos) |
//!
//! Hop samples require [`crate::std_native_table`].

use crate::{emit_native1_from, emit_native_n, Chunk, ChunkBuilder, Opcode};

/// Native indices (must match [`crate::std_native_map`]).
///
/// Hard-coded here so the sample stays self-contained without looking up
/// the map at assembly time ÔÇö the stability test in `natives` guards drift.
const N_PRINT: u32 = 0;
const N_MAKE_MSG: u32 = 2;
const N_MSG_SENDER: u32 = 3;
const N_MSG_REQUEST_ID: u32 = 4;
const N_MSG_TAG: u32 = 5;
const N_MSG_PAYLOAD: u32 = 6;
const N_MSG_REPLY_CAP: u32 = 7;

/// Protocol tags for Atomic Hop samples.
///
/// Opaque to the VM; only this sample (and its clients) interpret them.
pub const TAG_REQ: i32 = 1;
pub const TAG_REP: i32 = 2;
pub const TAG_PING: i32 = 10;
pub const TAG_PONG: i32 = 11;
/// Decoy hop for [`selective_receive`] ÔÇö must be skipped by `ReceiveMatch`.
pub const TAG_JUNK: i32 = 99;

/// `r0 = 41 + 1; return r0` ÔÇö the 60-second sanity chunk.
pub fn add_forty_two() -> Chunk {
    let mut b = ChunkBuilder::new("add-forty-two");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 41);
    b.emit_load_imm(1, 1);
    b.emit_binop(Opcode::Add, 0, 0, 1);
    b.emit_return(0);
    b.finish()
}

/// Two flows, one **Atomic Hop** round-trip: `main` sends a `Message` to
/// `pong`, `pong` replies with payload+1 via `msg_reply_cap`, `main` returns
/// that payload (`2`).
///
/// Requires [`crate::std_native_table`] (`make_msg` / `msg_*`).
///
/// Every `Send` carries exactly one [`crate::Value::Message`] and targets a
/// [`crate::Value::Cap`] ÔÇö bare ints / pids trap (Atomic Hop + FlowCap).
pub fn ping_pong() -> Chunk {
    let mut b = ChunkBuilder::new("ping-pong");

    // pong: receive Message, reply payload+1 via reply_cap
    // r0 = request Message
    // r1 = reply Cap
    // r2 = request_id
    // r3 = payload (+1), then make_msg result
    // r4..r7 = make_msg arg window
    let pong = b.begin_function("pong", 0, 8);
    b.emit_receive(0);
    emit_native1_from!(b, 1, 0, N_MSG_REPLY_CAP);
    emit_native1_from!(b, 2, 0, N_MSG_REQUEST_ID);
    emit_native1_from!(b, 3, 0, N_MSG_PAYLOAD);
    b.emit_load_imm(7, 1);
    b.emit_binop(Opcode::Add, 3, 3, 7);
    b.emit_self_pid(4);
    b.emit_move(5, 2);
    b.emit_load_imm(6, TAG_PONG);
    b.emit_move(7, 3);
    emit_native_n!(b, 4, N_MAKE_MSG, 4);
    b.emit_send(1, 4);
    b.emit_exit(4);

    // main: spawn pong (Cap), hop Message{tag=PING, payload=1}, return reply payload
    b.begin_function("main", 0, 8);
    b.emit_self_pid(1);
    b.emit_spawn(0, pong, 0);
    b.emit_move(2, 1);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_PING);
    b.emit_load_imm(5, 1);
    emit_native_n!(b, 2, N_MAKE_MSG, 4);
    b.emit_send(0, 2);
    b.emit_receive(6);
    emit_native1_from!(b, 4, 6, N_MSG_PAYLOAD);
    b.emit_return(4);

    b.finish()
}

/// Atomic request-reply with [`crate::Value::Message`] (one envelope per hop).
///
/// Requires [`crate::std_native_table`] (`make_msg` / `msg_*` / `print`).
///
/// # Why "Atomic Hop"
///
/// Correlation (`request_id`) and reply routing (`sender`) travel in a
/// **single** mailbox value. Classic actor runtimes often allow any scalar
/// on `Send`; Byteflow rejects that ÔÇö every hop is a typed envelope.
///
/// # Protocol
///
/// 1. `main` spawns `server`, builds
///    `Message { id=1, tag=REQ, payload=41 }` (sender placeholder ignored)
/// 2. `server` receives, logs via `print`, replies
///    `tag=REP, payload=42` via `msg_reply_cap` (echoing `request_id`)
/// 3. `main` returns the reply payload `42`
///
/// # Register discipline (`CallNative` clobbers `r[a]`)
///
/// `CallNative ra, ÔÇª, nc` reads args from `r[a..a+nc]` and writes the
/// result into `r[a]`. Keeping the original `Message` in `r0` therefore
/// means every unpack is `Move ri, r0` then `CallNative ri, msg_*, 1`.
/// `make_msg` needs four **contiguous** arg registers; the server rearranges
/// into `r4..r7` before the call.
pub fn atomic_request_reply() -> Chunk {
    let mut b = ChunkBuilder::new("atomic-request-reply");

    // --- server -----------------------------------------------------------
    // r0  = request Message (never overwritten until Exit)
    // r1  = reply Cap (from msg_reply_cap)
    // r2  = request_id
    // r3  = tag
    // r4  = payload, then make_msg result (reply Message)
    // r5  = self Cap, then make_msg arg slot
    // r6  = TAG_REP for make_msg
    // r7  = scratch (print copy, +1 imm, payload for make_msg)
    let server = b.begin_function("server", 0, 8);
    b.emit_receive(0);
    emit_native1_from!(b, 7, 0, N_PRINT);
    emit_native1_from!(b, 1, 0, N_MSG_REPLY_CAP);
    emit_native1_from!(b, 2, 0, N_MSG_REQUEST_ID);
    emit_native1_from!(b, 3, 0, N_MSG_TAG);
    emit_native1_from!(b, 4, 0, N_MSG_PAYLOAD);
    b.emit_load_imm(7, 1);
    b.emit_binop(Opcode::Add, 4, 4, 7);
    b.emit_self_pid(5);
    b.emit_move(7, 4);
    b.emit_move(4, 5);
    b.emit_move(5, 2);
    b.emit_load_imm(6, TAG_REP);
    emit_native_n!(b, 4, N_MAKE_MSG, 4);
    emit_native1_from!(b, 7, 4, N_PRINT);
    b.emit_send(1, 4);
    b.emit_exit(4);

    // --- main -------------------------------------------------------------
    // r0 = server Cap
    // r1 = self Cap
    // r2..r5 = make_msg(self, 1, TAG_REQ, 41) ÔåÆ r2 becomes the request Message
    // r6 = reply Message
    // r7 = scratch for print / unpack
    b.begin_function("main", 0, 8);
    b.emit_self_pid(1);
    b.emit_spawn(0, server, 0);
    b.emit_move(2, 1);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_REQ);
    b.emit_load_imm(5, 41);
    emit_native_n!(b, 2, N_MAKE_MSG, 4);
    emit_native1_from!(b, 7, 2, N_PRINT);
    b.emit_send(0, 2);
    b.emit_receive(6);
    emit_native1_from!(b, 7, 6, N_PRINT);
    emit_native1_from!(b, 4, 6, N_MSG_PAYLOAD);
    b.emit_return(4);

    b.finish()
}

/// Selective Atomic Hop: server waits for `TAG_REQ` while a `TAG_JUNK` hop
/// sits ahead in the mailbox (FIFO skip, not drop).
///
/// Requires [`crate::std_native_table`].
///
/// 1. `main` sends junk (`tag=TAG_JUNK`), then request (`tag=TAG_REQ`, payload=41)
/// 2. `server` does `ReceiveMatchImm TAG_REQ` ÔÇö must see payload 41, not junk
/// 3. replies `TAG_REP` / 42; then classic `Receive` drains the leftover junk
/// 4. `main` returns reply payload `42`
pub fn selective_receive() -> Chunk {
    let mut b = ChunkBuilder::new("selective-receive");

    // server: match TAG_REQ, reply 42 via reply_cap, then drain junk
    let server = b.begin_function("server", 0, 8);
    b.emit_receive_match_imm(0, TAG_REQ as u16);
    emit_native1_from!(b, 1, 0, N_MSG_REPLY_CAP);
    emit_native1_from!(b, 2, 0, N_MSG_REQUEST_ID);
    emit_native1_from!(b, 4, 0, N_MSG_PAYLOAD);
    b.emit_load_imm(7, 1);
    b.emit_binop(Opcode::Add, 4, 4, 7);
    b.emit_self_pid(5);
    b.emit_move(7, 4);
    b.emit_move(4, 5);
    b.emit_move(5, 2);
    b.emit_load_imm(6, TAG_REP);
    emit_native_n!(b, 4, N_MAKE_MSG, 4);
    b.emit_send(1, 4);
    // leftover TAG_JUNK must still be waiting
    b.emit_receive(0);
    emit_native1_from!(b, 3, 0, N_MSG_TAG);
    b.emit_load_imm(7, TAG_JUNK);
    b.emit_binop(Opcode::Eq, 3, 3, 7);
    // Branch jumps when falsy: not-equal ÔåÆ trap
    let trap_lbl = b.new_label();
    b.emit_branch(3, trap_lbl);
    b.emit_exit(4);
    b.bind_label(trap_lbl);
    b.emit_trap(2);

    b.begin_function("main", 0, 8);
    b.emit_self_pid(1);
    b.emit_spawn(0, server, 0);
    // junk hop first
    b.emit_move(2, 1);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_JUNK);
    b.emit_load_imm(5, 0);
    emit_native_n!(b, 2, N_MAKE_MSG, 4);
    b.emit_send(0, 2);
    // real request
    b.emit_move(2, 1);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_REQ);
    b.emit_load_imm(5, 41);
    emit_native_n!(b, 2, N_MAKE_MSG, 4);
    b.emit_send(0, 2);
    b.emit_receive_match_imm(6, TAG_REP as u16);
    emit_native1_from!(b, 4, 6, N_MSG_PAYLOAD);
    b.emit_return(4);

    b.finish()
}

/// Atomic request/reply via [`Opcode::Ask`] (RPC hop).
///
/// Requires [`crate::std_native_table`].
///
/// 1. `main` builds `Message { id=1, tag=REQ, payload=41 }` and `Ask`s the server Cap
/// 2. `server` `ReceiveMatchImm TAG_REQ`, replies via `msg_reply_cap` with
///    `TAG_REP` / payload 42 and the same `request_id`
/// 3. `Ask` resumes with the reply; `main` returns payload `42`
pub fn ask_reply() -> Chunk {
    let mut b = ChunkBuilder::new("ask-reply");

    let server = b.begin_function("server", 0, 8);
    b.emit_receive_match_imm(0, TAG_REQ as u16);
    emit_native1_from!(b, 1, 0, N_MSG_REPLY_CAP);
    emit_native1_from!(b, 2, 0, N_MSG_REQUEST_ID);
    emit_native1_from!(b, 4, 0, N_MSG_PAYLOAD);
    b.emit_load_imm(7, 1);
    b.emit_binop(Opcode::Add, 4, 4, 7);
    b.emit_self_pid(5);
    b.emit_move(7, 4);
    b.emit_move(4, 5);
    b.emit_move(5, 2);
    b.emit_load_imm(6, TAG_REP);
    emit_native_n!(b, 4, N_MAKE_MSG, 4);
    b.emit_send(1, 4);
    b.emit_exit(4);

    // main: Ask r6, r0 (server Cap), r2 (request Message)
    b.begin_function("main", 0, 8);
    b.emit_self_pid(1);
    b.emit_spawn(0, server, 0);
    b.emit_move(2, 1);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_REQ);
    b.emit_load_imm(5, 41);
    emit_native_n!(b, 2, N_MAKE_MSG, 4);
    b.emit_ask(6, 0, 2);
    emit_native1_from!(b, 4, 6, N_MSG_PAYLOAD);
    b.emit_return(4);

    b.finish()
}

/// Security regression sample: forged `make_msg` sender must not survive `Send`.
///
/// # What this proves
///
/// Invariant **S1** from `docs/security.md`: structural Atomic Hop typing
/// alone cannot stop a module from writing `sender = 999` into a
/// [`crate::Message`]. The scheduler overwrites that field on bytecode
/// `Send`, so the serverÔÇÖs `msg_sender` / echoed payload reflects the **real**
/// client flow id.
///
/// # Protocol
///
/// 1. `main` builds a request with forged sender `999` and `Send`s it.
/// 2. `server` reads the delivered hop, puts authenticated `msg_sender` into
///    the reply `payload`, and answers.
/// 3. `main` returns that payload as `Int`.
///
/// Unit tests assert the returned id is not `999` (and is a plausible live
/// flow id). Requires [`crate::std_native_table`].
pub fn forged_sender_send() -> Chunk {
    let mut b = ChunkBuilder::new("forged-sender-send");

    let server = b.begin_function("server", 0, 8);
    b.emit_receive(0);
    emit_native1_from!(b, 1, 0, N_MSG_REPLY_CAP);
    emit_native1_from!(b, 2, 0, N_MSG_REQUEST_ID);
    emit_native1_from!(b, 3, 0, N_MSG_SENDER);
    // reply: payload = authenticated sender FlowId
    b.emit_self_pid(4);
    b.emit_move(5, 2);
    b.emit_load_imm(6, TAG_REP);
    b.emit_move(7, 3);
    emit_native_n!(b, 4, N_MAKE_MSG, 4);
    b.emit_send(1, 4);
    b.emit_exit(4);

    b.begin_function("main", 0, 8);
    b.emit_self_pid(1);
    b.emit_spawn(0, server, 0);
    // Deliberately forge sender = 999
    b.emit_load_imm(2, 999);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_REQ);
    b.emit_load_imm(5, 0);
    emit_native_n!(b, 2, N_MAKE_MSG, 4);
    b.emit_send(0, 2);
    b.emit_receive(6);
    emit_native1_from!(b, 4, 6, N_MSG_PAYLOAD);
    b.emit_return(4);

    b.finish()
}

/// Same security property as [`forged_sender_send`], via [`Opcode::Ask`].
///
/// Covers the request half of **S1** on the RPC path: a forged
/// `make_msg.sender` is stamped away before the server observes the hop.
/// Reply authenticity (**S2**, `sender == target`) is covered separately by
/// mailbox unit tests (`ask_requires_reply_from_target`).
pub fn forged_sender_ask() -> Chunk {
    let mut b = ChunkBuilder::new("forged-sender-ask");

    let server = b.begin_function("server", 0, 8);
    b.emit_receive_match_imm(0, TAG_REQ as u16);
    emit_native1_from!(b, 1, 0, N_MSG_REPLY_CAP);
    emit_native1_from!(b, 2, 0, N_MSG_REQUEST_ID);
    emit_native1_from!(b, 3, 0, N_MSG_SENDER);
    b.emit_self_pid(4);
    b.emit_move(5, 2);
    b.emit_load_imm(6, TAG_REP);
    b.emit_move(7, 3);
    emit_native_n!(b, 4, N_MAKE_MSG, 4);
    b.emit_send(1, 4);
    b.emit_exit(4);

    b.begin_function("main", 0, 8);
    b.emit_self_pid(1);
    b.emit_spawn(0, server, 0);
    b.emit_load_imm(2, 999);
    b.emit_load_imm(3, 1);
    b.emit_load_imm(4, TAG_REQ);
    b.emit_load_imm(5, 0);
    emit_native_n!(b, 2, N_MAKE_MSG, 4);
    b.emit_ask(6, 0, 2);
    emit_native1_from!(b, 4, 6, N_MSG_PAYLOAD);
    b.emit_return(4);

    b.finish()
}

/// Immediate `Trap` ÔÇö used to show [`crate::Supervisor`] restart.
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
        decode, encode, std_native_table, verify, FlowOutcome, Runtime, RuntimeConfig, Value,
    };

    fn tiny(chunk: Chunk) -> Result<Runtime, crate::SpawnError> {
        Runtime::with_config(
            chunk,
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
            },
        )
    }

    fn tiny_natives(chunk: Chunk) -> Result<Runtime, crate::SpawnError> {
        Runtime::with_natives_and_config(
            chunk,
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::DEFAULT,
            },
        )
    }

    #[test]
    fn add_forty_two_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny(add_forty_two())?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
        Ok(())
    }

    #[test]
    fn ping_pong_joins_2() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ping_pong();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(2))));
        assert!(sent >= 2);
        Ok(())
    }

    #[test]
    fn atomic_request_reply_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = atomic_request_reply();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        assert!(sent >= 1);
        Ok(())
    }

    #[test]
    fn selective_receive_skips_junk_tag() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = selective_receive();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn ask_reply_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ask_reply();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes)?;
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "got {outcome:?}"
        );
        // request + reply
        assert!(sent >= 2);
        Ok(())
    }

    #[test]
    fn send_overwrites_forged_sender() -> Result<(), Box<dyn std::error::Error>> {
        // S1: make_msg(sender=999, …) + Send → receiver must not see 999.
        let chunk = forged_sender_send();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(n)) => {
                assert_ne!(n, 999, "forged make_msg sender must not survive Send");
                assert!(n >= 1, "authenticated sender must be a live flow id");
                Ok(())
            }
            other => Err(format!("expected Completed(Int), got {other:?}").into()),
        }
    }

    #[test]
    fn ask_overwrites_forged_request_sender() -> Result<(), Box<dyn std::error::Error>> {
        // S1 on the Ask request path (same forge, RPC hop).
        let chunk = forged_sender_ask();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(n)) => {
                assert_ne!(n, 999, "forged make_msg sender must not survive Ask");
                assert!(n >= 1, "authenticated sender must be a live flow id");
                Ok(())
            }
            other => Err(format!("expected Completed(Int), got {other:?}").into()),
        }
    }

    #[test]
    fn send_scalar_target_traps() -> Result<(), Box<dyn std::error::Error>> {
        let mut b = ChunkBuilder::new("bad-cap-target");
        b.begin_function("main", 0, 6);
        b.emit_load_imm(0, 99); // Int — not Cap
        b.emit_load_imm(1, 0);
        b.emit_load_imm(2, 1);
        b.emit_load_imm(3, TAG_PING);
        b.emit_load_imm(4, 1);
        emit_native_n!(b, 1, N_MAKE_MSG, 4);
        b.emit_send(0, 1);
        b.emit_return(1);
        let rt = tiny_natives(b.finish())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "non-Cap Send target must fail, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn send_scalar_is_not_an_atomic_hop() -> Result<(), Box<dyn std::error::Error>> {
        let mut b = ChunkBuilder::new("bad-hop");
        b.begin_function("main", 0, 2);
        b.emit_self_pid(0);
        b.emit_load_imm(1, 99);
        b.emit_send(0, 1);
        b.emit_return(1);
        let rt = tiny(b.finish())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "scalar Send must trap, got {outcome:?}"
        );
        Ok(())
    }
}
