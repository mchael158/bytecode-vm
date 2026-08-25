# Byteflow

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](LICENSE)

[English](README.md) · [Português (Brasil)](README.pt-BR.md)

Embeddable **Erlang-style** concurrency for Rust: register bytecode, virtual processes, mailboxes, and a supervisor — **one crate**.

```toml
[dependencies]
byteflow-actors = "0.3"
```

```rust
use byteflow::{ChunkBuilder, Runtime, Value};
```

Full documentation and examples: the crate README on [crates.io/crates/byteflow-actors](https://crates.io/crates/byteflow-actors) and [docs.rs](https://docs.rs/byteflow-actors).

```text
cargo run -p byteflow-actors --example ping_pong
cargo run -p byteflow-actors --bin byteflow -- demo ping-pong
cargo install byteflow-actors
```

**Not** a Tokio replacement / not distributed OTP. Host Rust owns I/O; Byteflow owns cheap actors.

Atomic request-reply (`Value::Message`): `cargo run -p byteflow-actors --example atomic_actors` — see `crates/byteflow/docs/atomic-actors.md`. Set `BYTEFLOW_LOG=info` for scheduler stderr logs.

License: MIT OR Apache-2.0 · [github.com/mchael158/bytecode-vm](https://github.com/mchael158/bytecode-vm)
