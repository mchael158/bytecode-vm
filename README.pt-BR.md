# Byteflow

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)

[English](README.md) · [Português (Brasil)](README.pt-BR.md)

Concorrência estilo **Erlang** embutível em Rust: bytecode, processos virtuais, mailboxes e supervisor — **uma única crate**.

```toml
[dependencies]
byteflow-actors = "0.3"
```

```rust
use byteflow::{ChunkBuilder, Runtime, Value};
```

Documentação completa: [crates.io/crates/byteflow-actors](https://crates.io/crates/byteflow-actors) e [docs.rs](https://docs.rs/byteflow-actors).

```text
cargo run -p byteflow-actors --example ping_pong
cargo install byteflow-actors
```

**Não** substitui Tokio / **não** é OTP distribuído. O Rust hospedeiro fica com o I/O.

Request-reply atômico (`Value::Message`): `cargo run -p byteflow-actors --example atomic_actors` — ver `crates/byteflow/docs/atomic-actors.md`. Use `BYTEFLOW_LOG=info` para logs do scheduler no stderr.

Licença: MIT OR Apache-2.0 · [github.com/mchael158/bytecode-vm](https://github.com/mchael158/bytecode-vm)
