//! Built-in demo chunks assembled with [`crate::Program`].
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

use crate::{Chunk, Program};

/// Native indices (must match [`crate::std_native_map`]).
const N_PRINT: u32 = 0;
const N_MAKE_MSG: u32 = 2;
const N_MSG_SENDER: u32 = 3;
const N_MSG_REQUEST_ID: u32 = 4;
const N_MSG_TAG: u32 = 5;
const N_MSG_PAYLOAD: u32 = 6;
const N_MSG_REPLY_CAP: u32 = 7;

/// Protocol tags for Atomic Hop samples.
pub const TAG_REQ: i32 = 1;
pub const TAG_REP: i32 = 2;
pub const TAG_PING: i32 = 10;
pub const TAG_PONG: i32 = 11;
/// Decoy hop for [`selective_receive`] — must be skipped by `ReceiveMatch`.
pub const TAG_JUNK: i32 = 99;

/// `41 + 1; return` — the 60-second sanity chunk.
pub fn add_forty_two() -> Chunk {
    let mut p = Program::new("add-forty-two");
    p.function("main", 0, |f| {
        let a = f.load_i32(41);
        let b = f.load_i32(1);
        let sum = f.add(a, b);
        f.return_(sum);
    });
    p.build()
}

/// Two flows, one **Atomic Hop** round-trip: `main` sends a `Message` to
/// `pong`, `pong` replies with payload+1 via `msg_reply_cap`, `main` returns
/// that payload (`2`).
pub fn ping_pong() -> Chunk {
    let mut p = Program::new("ping-pong");
    let pong =     p.function("pong", 0, |f| {
        let msg = f.receive();
        let reply_cap = f.native1_from(msg, N_MSG_REPLY_CAP);
        let req_id = f.native1_from(msg, N_MSG_REQUEST_ID);
        let payload = f.native1_from(msg, N_MSG_PAYLOAD);
        f.add_imm(payload, 1);
        let self_cap = f.self_cap();
        let reply = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_PONG, payload);
        f.send(reply_cap, reply);
        f.exit(reply);
    });
    p.function("main", 0, |f| {
        let self_cap = f.self_cap();
        let child = f.spawn(pong, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(1);
        let req = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_PING, payload);
        f.send(child, req);
        let reply = f.receive();
        let out = f.native1_from(reply, N_MSG_PAYLOAD);
        f.return_(out);
    });
    p.build()
}

/// Atomic request-reply with [`crate::Value::Message`] (one envelope per hop).
pub fn atomic_request_reply() -> Chunk {
    let mut p = Program::new("atomic-request-reply");
    let server = p.function("server", 0, |f| {
        let msg = f.receive();
        f.native1_on(msg, N_PRINT);
        let reply_cap = f.native1_from(msg, N_MSG_REPLY_CAP);
        let req_id = f.native1_from(msg, N_MSG_REQUEST_ID);
        let _tag = f.native1_from(msg, N_MSG_TAG);
        let payload = f.native1_from(msg, N_MSG_PAYLOAD);
        f.add_imm(payload, 1);
        let self_cap = f.self_cap();
        let reply = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REP, payload);
        f.native1_on(reply, N_PRINT);
        f.send(reply_cap, reply);
        f.exit(reply);
    });
    p.function("main", 0, |f| {
        let self_cap = f.self_cap();
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(41);
        let req = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REQ, payload);
        f.native1_on(req, N_PRINT);
        f.send(server_cap, req);
        let reply = f.receive();
        f.native1_on(reply, N_PRINT);
        let out = f.native1_from(reply, N_MSG_PAYLOAD);
        f.return_(out);
    });
    p.build()
}

/// Selective Atomic Hop: server waits for `TAG_REQ` while a `TAG_JUNK` hop
/// sits ahead in the mailbox (FIFO skip, not drop).
pub fn selective_receive() -> Chunk {
    let mut p = Program::new("selective-receive");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let reply_cap = f.native1_from(msg, N_MSG_REPLY_CAP);
        let req_id = f.native1_from(msg, N_MSG_REQUEST_ID);
        let payload = f.native1_from(msg, N_MSG_PAYLOAD);
        f.add_imm(payload, 1);
        let self_cap = f.self_cap();
        let reply = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REP, payload);
        f.send(reply_cap, reply);
        let junk = f.receive();
        let tag = f.native1_from(junk, N_MSG_TAG);
        let is_junk = f.eq_imm(tag, TAG_JUNK);
        let trap_lbl = f.label();
        f.branch_if_falsy(is_junk, trap_lbl);
        f.exit(reply);
        f.bind(trap_lbl);
        f.trap(2);
    });
    p.function("main", 0, |f| {
        let self_cap = f.self_cap();
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let junk = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_JUNK, zero);
        f.send(server_cap, junk);
        let payload = f.load_i32(41);
        let req = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REQ, payload);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.native1_from(reply, N_MSG_PAYLOAD);
        f.return_(out);
    });
    p.build()
}

/// Atomic request/reply via `Ask` (RPC hop).
pub fn ask_reply() -> Chunk {
    let mut p = Program::new("ask-reply");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let reply_cap = f.native1_from(msg, N_MSG_REPLY_CAP);
        let req_id = f.native1_from(msg, N_MSG_REQUEST_ID);
        let payload = f.native1_from(msg, N_MSG_PAYLOAD);
        f.add_imm(payload, 1);
        let self_cap = f.self_cap();
        let reply = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REP, payload);
        f.send(reply_cap, reply);
        f.exit(reply);
    });
    p.function("main", 0, |f| {
        let self_cap = f.self_cap();
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(41);
        let req = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REQ, payload);
        let reply = f.ask(server_cap, req);
        let out = f.native1_from(reply, N_MSG_PAYLOAD);
        f.return_(out);
    });
    p.build()
}

/// Security regression: forged `make_msg` sender must not survive `Send`.
pub fn forged_sender_send() -> Chunk {
    let mut p = Program::new("forged-sender-send");
    let server = p.function("server", 0, |f| {
        let msg = f.receive();
        let reply_cap = f.native1_from(msg, N_MSG_REPLY_CAP);
        let req_id = f.native1_from(msg, N_MSG_REQUEST_ID);
        let sender = f.native1_from(msg, N_MSG_SENDER);
        let self_cap = f.self_cap();
        let reply = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REP, sender);
        f.send(reply_cap, reply);
        f.exit(reply);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let forged = f.load_i32(999);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let req = f.make_msg(N_MAKE_MSG, forged, req_id, TAG_REQ, zero);
        f.send(server_cap, req);
        let reply = f.receive();
        let out = f.native1_from(reply, N_MSG_PAYLOAD);
        f.return_(out);
    });
    p.build()
}

/// Same security property as [`forged_sender_send`], via `Ask`.
pub fn forged_sender_ask() -> Chunk {
    let mut p = Program::new("forged-sender-ask");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let reply_cap = f.native1_from(msg, N_MSG_REPLY_CAP);
        let req_id = f.native1_from(msg, N_MSG_REQUEST_ID);
        let sender = f.native1_from(msg, N_MSG_SENDER);
        let self_cap = f.self_cap();
        let reply = f.make_msg(N_MAKE_MSG, self_cap, req_id, TAG_REP, sender);
        f.send(reply_cap, reply);
        f.exit(reply);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let forged = f.load_i32(999);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let req = f.make_msg(N_MAKE_MSG, forged, req_id, TAG_REQ, zero);
        let reply = f.ask(server_cap, req);
        let out = f.native1_from(reply, N_MSG_PAYLOAD);
        f.return_(out);
    });
    p.build()
}

/// Immediate `Trap` — used to show [`crate::Supervisor`] restart.
pub fn boom() -> Chunk {
    let mut p = Program::new("boom");
    p.function("boom", 0, |f| f.trap(1));
    p.build()
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
                ..Default::default()
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
                ..Default::default()
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
        assert!(sent >= 2);
        Ok(())
    }

    #[test]
    fn send_overwrites_forged_sender() -> Result<(), Box<dyn std::error::Error>> {
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
        let mut p = Program::new("bad-cap-target");
        p.function("main", 0, |f| {
            let bad_cap = f.load_i32(99);
            let sender = f.load_i32(0);
            let req_id = f.load_i32(1);
            let payload = f.load_i32(1);
            let msg = f.make_msg(N_MAKE_MSG, sender, req_id, TAG_PING, payload);
            f.send(bad_cap, msg);
            f.return_(msg);
        });
        let rt = tiny_natives(p.build())?;
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
        let mut p = Program::new("bad-hop");
        p.function("main", 0, |f| {
            let cap = f.self_cap();
            let scalar = f.load_i32(99);
            f.send(cap, scalar);
            f.return_(scalar);
        });
        let rt = tiny(p.build())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "scalar Send must trap, got {outcome:?}"
        );
        Ok(())
    }
}