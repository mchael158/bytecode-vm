//! Two virtual processes, one mailbox round-trip.
//!
//! ```text
//! cargo run -p byteflow --example ping_pong
//! ```

use byteflow::{samples, ProcessOutcome, Runtime, RuntimeConfig, Value};

fn main() {
    let chunk = samples::ping_pong();
    let rt = Runtime::with_config(
        chunk,
        RuntimeConfig {
            workers: 1,
            quantum: 10_000,
        },
    );
    let main = rt.function_index("main").expect("main");
    let outcome = rt.spawn(main, &[]).join();
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
