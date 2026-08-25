//! Measured demo for posts: spawn N trivial processes and join them all.
//!
//! ```text
//! cargo run -p byteflow-actors --example throughput --release
//! ```

use std::time::Instant;

use byteflow::{ChunkBuilder, ProcessOutcome, Runtime, RuntimeConfig, Value};

fn trivial_chunk() -> byteflow::Chunk {
    let mut b = ChunkBuilder::new("throughput");
    b.begin_function("worker", 0, 1);
    b.emit_load_imm(0, 1);
    b.emit_return(0);
    b.finish()
}

fn main() {
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);

    let workers = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);

    let rt = match Runtime::with_config(
        trivial_chunk(),
        RuntimeConfig {
            workers,
            quantum: 10_000,
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            std::process::exit(1);
        }
    };
    let Some(worker_fn) = rt.function_index("worker") else {
        eprintln!("missing worker");
        std::process::exit(1);
    };

    let start = Instant::now();
    let mut handles = Vec::with_capacity(n as usize);
    for _ in 0..n {
        match rt.spawn(worker_fn, &[]) {
            Ok(h) => handles.push(h),
            Err(e) => {
                eprintln!("spawn: {e}");
                std::process::exit(1);
            }
        }
    }
    let mut ok = 0u32;
    for h in handles {
        if matches!(h.join(), ProcessOutcome::Completed(Value::Int(1))) {
            ok += 1;
        }
    }
    let elapsed = start.elapsed();
    let metrics = rt.metrics();
    rt.shutdown();

    let secs = elapsed.as_secs_f64().max(1e-9);
    println!("processes={ok}/{n}");
    println!("workers={workers}");
    println!("elapsed_ms={:.2}", elapsed.as_secs_f64() * 1000.0);
    println!("spawns_per_sec={:.0}", ok as f64 / secs);
    println!("{metrics}");
}
