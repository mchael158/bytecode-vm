//! Atomic Hop: one [`byteflow::Value::Message`] per request/reply hop.
//!
//! This is the runnable counterpart of `samples::atomic_request_reply` and
//! `docs/atomic-hop.md`. It wires the std native table (needed for
//! `make_msg` / `msg_*` / `print`) and joins until the client returns
//! payload `42`.
//!
//! ```text
//! cargo run -p byteflow-actors --example atomic_actors
//!
//! # scheduler stderr (spawn/send/recv/finish) — separate from print's stdout
//! $env:BYTEFLOW_LOG="info"
//! cargo run -p byteflow-actors --example atomic_actors
//! ```
//!
//! Failures from `Runtime::…` / `spawn` are printed and exit non-zero —
//! matching the fail-closed host API (no `.expect` on the happy path).

use byteflow::{samples, std_native_table, FlowOutcome, Runtime, RuntimeConfig, Value};

fn main() {
    let chunk = samples::atomic_request_reply();
    let rt = match Runtime::with_natives_and_config(
        chunk,
        std_native_table(),
        RuntimeConfig {
            workers: 2,
            quantum: 10_000,
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            std::process::exit(1);
        }
    };
    let Some(main) = rt.function_index("main") else {
        eprintln!("missing main");
        std::process::exit(1);
    };
    let handle = match rt.spawn(main, &[]) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("spawn: {e}");
            std::process::exit(1);
        }
    };
    let outcome = handle.join();
    let metrics = rt.metrics();
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Int(42)) => {
            println!("atomic request-reply ok: payload=42");
            println!("{metrics}");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
