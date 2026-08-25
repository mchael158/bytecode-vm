//! Two virtual processes, one mailbox round-trip.
//!
//! ```text
//! cargo run -p byteflow-actors --example ping_pong
//! ```

use byteflow::{samples, ProcessOutcome, Runtime, RuntimeConfig, Value};

fn main() {
    let chunk = samples::ping_pong();
    let rt = match Runtime::with_config(
        chunk,
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
        ProcessOutcome::Completed(Value::Int(2)) => {
            println!("pong replied 2");
            println!("{metrics}");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
