# Byteflow

[English](README.md) · [Português (Brasil)](README.pt-BR.md)

Um Erlang minúsculo dentro de um binário Rust: **bytecode em registradores**, **processos virtuais**, **mailboxes** e um **supervisor**. Programas se montam com [`ChunkBuilder`](crates/byteflow-bytecode/src/builder.rs) em Rust — **sem linguagem-fonte separada**. Dezenas de milhares de atores compartilham poucas threads OS. Um processo que dá trap não derruba o worker.

```text
cargo run -p byteflow --example ping_pong
# pong replied 2
```

```text
cargo run -p byteflow-cli -- demo ping-pong
# 2
```

## Números (medidos, não inventados)

Nesta máquina — **Intel Core i5-9400F @ 2.90 GHz** (6C/6T), Windows 10 Pro, `rustc 1.97.0`, build release, 6 workers:

```text
cargo run -p byteflow --example throughput --release -- 100000
# processes=100000/100000
# elapsed_ms≈173
# spawns_per_sec≈579000
```

Rode aí e cite *os seus* números, não estes.

## O que é

| Camada | Crate | Função |
|---|---|---|
| ISA + `.bf` + verificador | `byteflow-bytecode` | Só dados. `ChunkBuilder` é o assembler. |
| Interpretador + tabela FFI | `byteflow-vm` | Um processo. `CallNative` → `NativeTable`. |
| Scheduler | `byteflow-scheduler` | Workers M:N, mailboxes, timer, supervisor. |
| Fachada | `byteflow` | API de embed, natives std, samples. |
| CLI | `byteflow-cli` | `verify` / `disasm` / `run` / `pack` / `demo`. |

**Não** é cluster, nem substituto do Tokio, nem JVM. O Rust hospedeiro fica com o I/O. O Byteflow fica com a concorrência barata. A lógica é Rust que emite bytecode — a linguagem do host continua sendo Rust.

## Embed em 60 segundos (`ChunkBuilder`)

```rust
use byteflow::{ChunkBuilder, Opcode, ProcessOutcome, Runtime, Value};

fn main() {
    let mut b = ChunkBuilder::new("demo");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 41);
    b.emit_load_imm(1, 1);
    b.emit_binop(Opcode::Add, 0, 0, 1);
    b.emit_return(0);

    let rt = Runtime::new(b.finish());
    let outcome = rt.spawn(0, &[]).join();
    rt.shutdown();

    assert!(matches!(outcome, ProcessOutcome::Completed(Value::Int(42))));
}
```

## Natives std (`print`, `now_ms`)

Índices estáveis: **`print = 0`**, **`now_ms = 1`**. Pareie a tabela com `emit_call_native`:

```rust
use byteflow::{std_native_table, ChunkBuilder, Runtime};

let mut b = ChunkBuilder::new("clock");
b.begin_function("main", 0, 2);
b.emit_load_imm(0, 42);
b.emit_call_native(0, 0, 1); // print(r0)
b.emit_call_native(1, 1, 0); // r1 = now_ms()
b.emit_return(1);

let rt = Runtime::with_natives(b.finish(), std_native_table());
```

Ou `std_natives()` → `(tabela, mapa nome→índice)` para host e bytecode ficarem alinhados.

Demos:

```text
cargo run -p byteflow --example ping_pong
cargo run -p byteflow --example crash_and_restart
cargo run -p byteflow --example throughput --release
```

## CLI e o formato `.bf`

Módulos em disco começam com magic `BFV0`. Encode/decode ficam em `byteflow-bytecode` (a CLI faz o `read`/`write`).

```text
cargo run -p byteflow-cli -- pack ping-pong ping.bf
cargo run -p byteflow-cli -- verify ping.bf
cargo run -p byteflow-cli -- disasm ping.bf
cargo run -p byteflow-cli -- run ping.bf main
```

Arquivos não confiáveis passam por `Opcode::from_u8` + `verify` antes de rodar.

## Como um processo vive

1. Um worker roda no máximo `quantum` instruções (padrão 10 000).
2. `Yield` / orçamento → de volta à fila (roubável).
3. `Sleep` → thread de timer, depois injector.
4. `Receive` com mailbox vazia → o processo **estaciona na própria mailbox**. O próximo `Send` acorda sob o mesmo lock.
5. `Fault` / `Trap` → `ProcessState::Failed`, entregue ao [`Supervisor`](crates/byteflow-scheduler/src/supervisor.rs).

`join()` é só para a thread nativa do embedder.

## Layout

```text
crates/byteflow-bytecode   ISA, ChunkBuilder, BFV0, verificador
crates/byteflow-vm         interpretador + NativeTable
crates/byteflow-scheduler  processos, mailboxes, runtime, supervisor
crates/byteflow            fachada + samples + natives std
crates/byteflow-cli        binário `byteflow`
```

## Status (v0)

**Pronto**

- ISA, assembler (`ChunkBuilder`), verificador
- `.bf` (BFV0) + CLI
- VM por processo, scheduler M:N, mailboxes, timer, supervisor
- FFI real + natives std (`print`, `now_ms`)
- Spawn, `SelfPid`, exemplos ping-pong / crash-restart / throughput

**Ainda não / próximo**

- Strings / bytes em `Value`
- Mailbox limitada + backpressure
- Benches de msgs/s
- Timing wheel, JIT, distribuição, capabilities

Licença: MIT OR Apache-2.0.
