use crate::{Chunk, ChunkBuilder, Opcode};

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
/// Protocol (scalar messages only):
/// 1. main reads its own Pid (`SelfPid`) and spawns `pong`
/// 2. main sends that Pid, then the integer `1`
/// 3. pong receives both, increments, sends `2` back
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
        decode, encode, verify, ProcessOutcome, Runtime, RuntimeConfig, Value,
    };

    fn tiny(chunk: Chunk) -> Runtime {
        Runtime::with_config(
            chunk,
            RuntimeConfig {
                workers: 1,
                quantum: 10_000,
            },
        )
    }

    #[test]
    fn add_forty_two_joins_42() {
        let rt = tiny(add_forty_two());
        let idx = rt.function_index("main").unwrap();
        let outcome = rt.spawn(idx, &[]).join();
        rt.shutdown();
        assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(42))));
    }

    #[test]
    fn ping_pong_joins_2() {
        let chunk = ping_pong();
        assert!(verify(&chunk).is_ok());
        let bytes = encode(&chunk);
        let chunk = decode(&bytes).unwrap();
        let rt = tiny(chunk);
        let idx = rt.function_index("main").unwrap();
        let outcome = rt.spawn(idx, &[]).join();
        let sent = rt.metrics().messages_sent;
        rt.shutdown();
        assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(2))));
        assert!(sent >= 2);
    }
}
