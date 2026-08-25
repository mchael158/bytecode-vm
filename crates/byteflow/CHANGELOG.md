# Changelog

All notable changes to **byteflow-actors** are documented here.

## [0.3.0] — 2026-08-25

### Breaking
- `Runtime::new` / `with_*` return `Result<Runtime, SpawnError>` (no panic on verify / thread spawn).
- `Runtime::spawn`, `RuntimeSpawner::spawn`, `Supervisor::{new,with_config,start_child}` return `Result`.
- Std native table grew Message helpers at frozen slots **2–6** (`make_msg`, `msg_*`). Slots **0–1** unchanged.

### Added
- `Value::Message` / ABI v2 actor envelopes (`sender`, `request_id`, `tag`, `payload`).
- `samples::atomic_request_reply` + example `atomic_actors`.
- Scheduler logs via `BYTEFLOW_LOG` (`crate::log`).
- Fail-closed mutex helpers + `RuntimeError` / `SpawnError` (`docs/error-model.md`).
- Clippy: `unwrap_used` and `expect_used` denied in non-test code.

### Docs
- `docs/atomic-actors.md`, `docs/error-model.md`.

## [0.2.1] — prior

Initial crates.io line: ISA, VM, M:N scheduler, mailboxes, supervisor, std natives (`print`, `now_ms`), CLI.
