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
//! | [`ask_timeout_expires`] | `AskTimeout` writes `Unit` when the server stays silent |
//! | [`ask_target_exits`] | `Ask` dest is `TAG_SYS_EXIT` when the server dies first |
//! | [`server_loop`] | BEAM-style receive → handle → reply loop |
//! | [`forged_sender_send`] / [`forged_sender_ask`] | S1: forged `make_msg` sender dies |
//! | [`boom`] | Immediate trap (supervisor demos) |
//! | [`monitor_down`] | Monitor → [`crate::TAG_SYS_DOWN`] on child exit |
//! | [`waiting_send`] | `WAITING_SEND`: second hop parks until the first is received |
//!
//! Hop samples require [`crate::std_native_table`].

use crate::{Chunk, Program};
use crate::natives::std_native;

/// Native indices (must match [`crate::std_native_map`]).
const N_PRINT: u32 = std_native::PRINT;
const N_MAKE_MSG: u32 = std_native::MAKE_MSG;

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
    let pong = p.function("pong", 0, |f| {
        let msg = f.receive();
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_PONG, payload);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let child = f.spawn(pong, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(1);
        let req = f.hop(req_id, TAG_PING, payload);
        f.send(child, req);
        let reply = f.receive();
        let out = f.hop_payload(reply);
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
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_REP, payload);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(41);
        let req = f.hop(req_id, TAG_REQ, payload);
        f.native1_on(req, N_PRINT);
        f.send(server_cap, req);
        let reply = f.receive();
        f.native1_on(reply, N_PRINT);
        let out = f.hop_payload(reply);
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
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_REP, payload);
        let junk = f.receive();
        let tag = f.hop_tag(junk);
        let is_junk = f.eq_imm(tag, TAG_JUNK);
        let trap_lbl = f.label();
        f.branch_if_falsy(is_junk, trap_lbl);
        f.exit(payload);
        f.bind(trap_lbl);
        f.trap(2);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let junk = f.hop(req_id, TAG_JUNK, zero);
        f.send(server_cap, junk);
        let payload = f.load_i32(41);
        let req = f.hop(req_id, TAG_REQ, payload);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Atomic request/reply via `Ask` (RPC hop).
pub fn ask_reply() -> Chunk {
    let mut p = Program::new("ask-reply");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, TAG_REP, payload);
        f.exit(payload);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(41);
        let req = f.hop(req_id, TAG_REQ, payload);
        let reply = f.ask(server_cap, req);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// `AskTimeout` against a server that never replies → `Unit`.
pub fn ask_timeout_expires() -> Chunk {
    let mut p = Program::new("ask-timeout");
    let server = p.function("server", 0, |f| {
        let _msg = f.receive();
        let ms = f.load_i32(10_000);
        f.sleep(ms);
        let zero = f.load_i32(0);
        f.return_(zero);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(0);
        let req = f.hop(req_id, TAG_REQ, payload);
        let ms = f.load_i32(40);
        let reply = f.ask_timeout(server_cap, req, ms);
        f.return_(reply);
    });
    p.build()
}

/// Server takes the request and exits; client `Ask` must not hang.
pub fn ask_target_exits() -> Chunk {
    let mut p = Program::new("ask-target-exits");
    let server = p.function("server", 0, |f| {
        let _msg = f.receive();
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(0);
        let req = f.hop(req_id, TAG_REQ, payload);
        let reply = f.ask(server_cap, req);
        let tag = f.hop_tag(reply);
        f.return_(tag);
    });
    p.build()
}

/// BEAM-style server loop: `receive_match` → handle → `send_reply` → repeat.
pub fn server_loop() -> Chunk {
    let mut p = Program::new("server-loop");
    let server = p.function("server", 0, |f| {
        let loop_lbl = f.label();
        f.bind(loop_lbl);
        let req = f.receive_match_imm(TAG_REQ as u16);
        let payload = f.hop_payload(req);
        f.add_imm(payload, 1);
        f.send_reply(req, TAG_REP, payload);
        f.jump(loop_lbl);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let req_id = f.load_i32(1);
        let payload = f.load_i32(41);
        let req = f.hop(req_id, TAG_REQ, payload);
        f.send(server_cap, req);
        let reply = f.receive_match_imm(TAG_REP as u16);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Security regression: forged `make_msg` sender must not survive `Send`.
pub fn forged_sender_send() -> Chunk {
    let mut p = Program::new("forged-sender-send");
    let server = p.function("server", 0, |f| {
        let msg = f.receive();
        let sender = f.hop_sender(msg);
        f.send_reply(msg, TAG_REP, sender);
        f.exit(sender);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let forged = f.load_i32(999);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let req = f.make_msg_legacy_sender(N_MAKE_MSG, forged, req_id, TAG_REQ, zero);
        f.send(server_cap, req);
        let reply = f.receive();
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Same security property as [`forged_sender_send`], via `Ask`.
pub fn forged_sender_ask() -> Chunk {
    let mut p = Program::new("forged-sender-ask");
    let server = p.function("server", 0, |f| {
        let msg = f.receive_match_imm(TAG_REQ as u16);
        let sender = f.hop_sender(msg);
        f.send_reply(msg, TAG_REP, sender);
        f.exit(sender);
    });
    p.function("main", 0, |f| {
        let server_cap = f.spawn(server, 0);
        let forged = f.load_i32(999);
        let req_id = f.load_i32(1);
        let zero = f.load_i32(0);
        let req = f.make_msg_legacy_sender(N_MAKE_MSG, forged, req_id, TAG_REQ, zero);
        let reply = f.ask(server_cap, req);
        let out = f.hop_payload(reply);
        f.return_(out);
    });
    p.build()
}

/// Child sleeps then exits; parent monitors and returns the `DOWN` reason (`0` = normal).
pub fn monitor_down() -> Chunk {
    let mut p = Program::new("monitor-down");
    let child = p.function("child", 0, |f| {
        let ms = f.load_i32(40);
        f.sleep(ms);
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.function("main", 0, |f| {
        let cap = f.spawn(child, 0);
        let mon = f.monitor(cap);
        let msg = f.receive_match_imm(crate::TAG_SYS_DOWN);
        let id = f.hop_request_id(msg);
        let ok = f.eq(id, mon);
        let trap = f.label();
        f.branch_if_falsy(ok, trap);
        let out = f.hop_payload(msg);
        f.return_(out);
        f.bind(trap);
        f.trap(3);
    });
    p.build()
}

/// Client arity 1 (`r0` = server Cap) sends two hops; server drains both.
/// With mailbox capacity 1 the second send parks (`WAITING_SEND`).
pub fn waiting_send() -> Chunk {
    let mut p = Program::new("waiting-send");
    p.function("server", 0, |f| {
        let ms = f.load_i32(50);
        f.sleep(ms);
        let _first = f.receive();
        let second = f.receive();
        let out = f.hop_payload(second);
        f.return_(out);
    });
    p.function("client", 1, |f| {
        let server = f.reg(0);
        let req_id = f.load_i32(1);
        let one = f.load_i32(1);
        let first = f.hop(req_id, TAG_REQ, one);
        f.send(server, first);
        let forty_two = f.load_i32(42);
        let second = f.hop(req_id, TAG_REQ, forty_two);
        f.send(server, second);
        f.exit(second);
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
        bytecode::CapId, decode, encode, std_native_table, verify, FlowOutcome, Runtime,
        RuntimeConfig, Value,
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
    fn ask_timeout_writes_unit() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ask_timeout_expires();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Unit)),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn ask_target_exit_writes_sys_exit() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = ask_target_exits();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(
                outcome,
                FlowOutcome::Completed(Value::Int(n)) if n == i64::from(crate::TAG_SYS_EXIT)
            ),
            "got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn server_loop_joins_42() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = server_loop();
        assert!(verify(&chunk).is_ok());
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
    fn send_overwrites_forged_sender() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = forged_sender_send();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Pid(n)) => {
                assert_ne!(n, 999, "forged make_msg sender must not survive Send");
                assert!(n >= 1, "authenticated sender must be a live flow id");
                Ok(())
            }
            other => Err(format!("expected Completed(Pid), got {other:?}").into()),
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
            FlowOutcome::Completed(Value::Pid(n)) => {
                assert_ne!(n, 999, "forged make_msg sender must not survive Ask");
                assert!(n >= 1, "authenticated sender must be a live flow id");
                Ok(())
            }
            other => Err(format!("expected Completed(Pid), got {other:?}").into()),
        }
    }

    #[test]
    fn send_scalar_target_traps() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("bad-cap-target");
        p.function("main", 0, |f| {
            let bad_cap = f.load_i32(99);
            let req_id = f.load_i32(1);
            let payload = f.load_i32(1);
            let msg = f.hop(req_id, TAG_PING, payload);
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

    #[test]
    fn monitor_down_joins_normal_reason() -> Result<(), Box<dyn std::error::Error>> {
        let chunk = monitor_down();
        assert!(verify(&chunk).is_ok());
        let rt = tiny_natives(chunk)?;
        let idx = rt.function_index("main").ok_or("main")?;
        let outcome = rt.spawn(idx, &[])?.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(0))),
            "DOWN reason should be Normal (0), got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn waiting_send_second_hop_arrives() -> Result<(), Box<dyn std::error::Error>> {
        let cap = crate::MailboxCapacity::new(1).ok_or("cap")?;
        let rt = Runtime::with_natives_and_config(
            waiting_send(),
            std_native_table(),
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
                mailbox: crate::MailboxConfig::new(cap, crate::OverflowPolicy::Reject),
                ..Default::default()
            },
        )?;
        let server = rt.function_index("server").ok_or("server")?;
        let client = rt.function_index("client").ok_or("client")?;
        let server_h = rt.spawn(server, &[])?;
        let server_cap = rt.mint_cap(server_h.id())?;
        rt.spawn(client, &[Value::Cap(server_cap)])?;
        let outcome = server_h.join();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Completed(Value::Int(42))),
            "second hop should be admitted after the first pop, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn link_kills_peer_on_fault() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("link-kill");
        p.function("park", 0, |f| {
            let _ = f.receive();
            f.trap(9);
        });
        p.function("boom", 0, |f| f.trap(1));
        let rt = tiny(p.build())?;
        let park = rt.function_index("park").ok_or("park")?;
        let boom = rt.function_index("boom").ok_or("boom")?;
        let parked = rt.spawn(park, &[])?;
        let killer = rt.spawn(boom, &[])?;
        rt.link(parked.id(), killer.id())?;
        let boom_out = killer.join();
        let park_out = parked.join();
        rt.shutdown();
        assert!(matches!(boom_out, FlowOutcome::Failed(_)), "{boom_out:?}");
        assert!(
            matches!(park_out, FlowOutcome::Failed(_)),
            "linked peer must die on fault, got {park_out:?}"
        );
        Ok(())
    }

    #[test]
    fn linked_exit_down_carries_link_reason() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("link-down-reason");
        p.function("watcher", 0, |f| {
            let msg = f.receive_match_imm(crate::TAG_SYS_DOWN);
            let out = f.hop_payload(msg);
            f.return_(out);
        });
        p.function("park", 0, |f| {
            let _ = f.receive();
            f.trap(9);
        });
        p.function("boom", 0, |f| f.trap(1));
        let rt = tiny_natives(p.build())?;
        let watcher = rt.spawn(rt.function_index("watcher").ok_or("watcher")?, &[])?;
        let parked = rt.spawn(rt.function_index("park").ok_or("park")?, &[])?;
        let killer = rt.spawn(rt.function_index("boom").ok_or("boom")?, &[])?;
        rt.monitor(watcher.id(), parked.id())?;
        rt.link(parked.id(), killer.id())?;
        let _ = killer.join();
        let watched = watcher.join();
        let _ = parked.join();
        rt.shutdown();
        assert!(
            matches!(
                watched,
                FlowOutcome::Completed(Value::Int(n)) if n == crate::FlowExitReason::Link.as_u64() as i64
            ),
            "DOWN payload must be Link, got {watched:?}"
        );
        Ok(())
    }

    #[test]
    fn monitor_dead_owner_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny(add_forty_two())?;
        let first = rt.spawn(0, &[])?;
        let dead = first.id();
        let done = first.join();
        assert!(matches!(done, FlowOutcome::Completed(_)));
        let live = rt.spawn(0, &[])?;
        let err = rt.monitor(dead, live.id());
        live.join();
        rt.shutdown();
        assert!(
            matches!(err, Err(crate::LifecycleError::NoSuchFlow(_))),
            "{err:?}"
        );
        Ok(())
    }

    #[test]
    fn forged_cap_cannot_be_registered() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny(add_forty_two())?;
        let err = rt.register_name("svc", CapId::from_raw(99_999));
        rt.shutdown();
        assert_eq!(err, Err(crate::LifecycleError::InvalidCapability));
        Ok(())
    }

    #[test]
    fn registry_clears_on_exit() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = Program::new("reg");
        p.function("main", 0, |f| {
            let ms = f.load_i32(80);
            f.sleep(ms);
            let z = f.load_i32(1);
            f.return_(z);
        });
        let rt = tiny(p.build())?;
        let h = rt.spawn(0, &[])?;
        let cap = rt.mint_cap(h.id())?;
        rt.register_name("svc", cap)?;
        assert_eq!(rt.whereis("svc")?, Some(cap));
        let _ = h.join();
        assert_eq!(rt.whereis("svc")?, None);
        rt.shutdown();
        Ok(())
    }
}