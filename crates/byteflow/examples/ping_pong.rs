//! Two flows, one Atomic Hop round-trip (`Value::Message`).
//!
//! ```text
//! cargo run -p byteflow-actors --example ping_pong
//! ```

use byteflow::{samples, std_native_table, FlowOutcome, Runtime, RuntimeConfig, Value};

fn main() {
    let chunk = samples::ping_pong();
    let rt = match Runtime::with_natives_and_config(
        chunk,
        std_native_table(),
        RuntimeConfig {
            workers: 1,
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
        FlowOutcome::Completed(Value::Int(2)) => {
            println!("pong replied 2 (Atomic Hop)");
            println!("{metrics}");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
