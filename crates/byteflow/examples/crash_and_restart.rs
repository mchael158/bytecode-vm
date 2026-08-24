//! A child that always traps; the supervisor brings it back until intensity
//! (2 restarts in 5s) is exceeded.
//!
//! ```text
//! cargo run -p byteflow --example crash_and_restart
//! ```

use std::thread;
use std::time::Duration;

use byteflow::{
    samples, ChildSpec, RestartPolicy, Runtime, RuntimeConfig, Supervisor, SupervisorConfig,
};

fn main() {
    let rt = Runtime::with_config(
        samples::boom(),
        RuntimeConfig {
            workers: 1,
            quantum: 1_000,
        },
    );
    let boom = rt.function_index("boom").expect("boom");
    let sup = Supervisor::with_config(
        rt.spawner(),
        SupervisorConfig {
            max_restarts: 2,
            max_period: Duration::from_secs(5),
        },
    );
    let _ = sup.start_child(ChildSpec::new("boom", boom).restart(RestartPolicy::OnFailure));

    let start = std::time::Instant::now();
    while !sup.intensity_exceeded() {
        if start.elapsed() > Duration::from_secs(2) {
            eprintln!("supervisor did not hit intensity in time");
            std::process::exit(1);
        }
        thread::sleep(Duration::from_millis(5));
    }

    let metrics = rt.metrics();
    println!("intensity exceeded after {} failures", metrics.processes_failed);
    println!("{metrics}");
    sup.shutdown();
    rt.shutdown();
}
