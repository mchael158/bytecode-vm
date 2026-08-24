//! M:N processes, mailboxes, timer, supervisor and [`Runtime`].

mod directory;
mod handle;
mod mailbox;
mod metrics;
pub(crate) mod oneshot;
mod process;
mod runtime;
mod supervisor;
mod sync_lock;
mod timer;
mod worker;

pub use handle::ProcessHandle;
pub use mailbox::{Delivery, Mailbox};
pub use metrics::{RuntimeMetrics, RuntimeMetricsSnapshot};
pub use process::{
    next_process_id, Process, ProcessId, ProcessMetrics, ProcessOutcome, ProcessState,
    RestartPolicy,
};
pub use runtime::{
    pid_from_u64, Runtime, RuntimeConfig, RuntimeSpawner, SendError, DEFAULT_QUANTUM,
};
pub use supervisor::{ChildSpec, Supervisor, SupervisorConfig};
