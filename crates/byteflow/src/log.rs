//! Host-side scheduler diagnostics on stderr, gated by `BYTEFLOW_LOG`.
//!
//! # Why this is separate from the `print` native
//!
//! Actor-visible logging (`CallNative print`) writes to **stdout** and is part
//! of the program's observable behaviour — samples and demos use it to show
//! envelopes crossing the mailbox. Scheduler diagnostics (spawn / send /
//! park / finish) are an **operator** concern: they must not pollute stdout
//! when an embedder pipes Flow output, and they must stay off by default
//! so a production run is silent unless asked.
//!
//! ```text
//! BYTEFLOW_LOG unset / 0 / off / false  → silent
//! BYTEFLOW_LOG=1 / info / true          → spawn, send, receive, finish
//! BYTEFLOW_LOG=debug / 2                → also park, deliver queued/handoff
//! ```
//!
//! # Lazy init
//!
//! The level is read from the environment once (first call) and cached in
//! atomics. That avoids re-parsing `std::env` on every send in a hot worker
//! loop. Tests can override via [`set_level_for_tests`].

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const OFF: u8 = 0;
const INFO: u8 = 1;
const DEBUG: u8 = 2;

static LEVEL: AtomicU8 = AtomicU8::new(OFF);
static INIT: AtomicU8 = AtomicU8::new(0);

fn level() -> u8 {
    if INIT.load(Ordering::Acquire) == 0 {
        let parsed = match std::env::var("BYTEFLOW_LOG") {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                match v.as_str() {
                    "" | "0" | "off" | "false" | "no" => OFF,
                    "debug" | "trace" | "2" => DEBUG,
                    // "1", "info", "true", "yes", or any other non-empty token
                    _ => INFO,
                }
            }
            Err(_) => OFF,
        };
        LEVEL.store(parsed, Ordering::Release);
        INIT.store(1, Ordering::Release);
    }
    LEVEL.load(Ordering::Acquire)
}

fn stamp_ms() -> u64 {
    // Logging must never panic: a broken clock just prints epoch-ish 0.
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => 0,
    }
}

/// Override the cached level (tests only). `0` off, `1` info, `2` debug.
pub fn set_level_for_tests(level: u8) {
    LEVEL.store(level.min(DEBUG), Ordering::Release);
    INIT.store(1, Ordering::Release);
}

/// INFO-level line when [`BYTEFLOW_LOG`](crate::log) is at least `info`.
#[inline]
pub fn info(msg: impl std::fmt::Display) {
    if level() >= INFO {
        eprintln!("[{}] byteflow INFO  {}", stamp_ms(), msg);
    }
}

/// DEBUG-level line when [`BYTEFLOW_LOG`](crate::log) is `debug`.
#[inline]
pub fn debug(msg: impl std::fmt::Display) {
    if level() >= DEBUG {
        eprintln!("[{}] byteflow DEBUG {}", stamp_ms(), msg);
    }
}
