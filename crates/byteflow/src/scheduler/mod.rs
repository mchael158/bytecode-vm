//! M:N **flows**, mailboxes, timer, supervisor and [`Runtime`].
//!
//! A flow is Byteflow's unit of concurrent work. Flows exchange **Atomic Hops**
//! ([`crate::Value::Message`]) — never bare scalars on `Send` / `Ask`.
//!
//! Outgoing bytecode hops are **sender-authenticated** and grant a **reply
//! Cap** (`Message.reply_cap`) before mailbox delivery. `Send` / `Ask` targets
//! must be [`crate::Value::Cap`] — raw [`crate::Value::Pid`] is identity only.
//! See `docs/security.md`.

mod capability;
mod directory;
mod error;
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

pub use capability::{CapId, CapRights};
pub use error::{fault_count, report_fault, RuntimeError, SpawnError};
pub use handle::FlowHandle;
pub use mailbox::{Delivery, Mailbox};
pub use metrics::{RuntimeMetrics, RuntimeMetricsSnapshot};
pub use process::{
    next_flow_id, Flow, FlowId, FlowMetrics, FlowOutcome, FlowState, RestartPolicy,
};
pub use runtime::{
    flow_id_from_u64, Runtime, RuntimeConfig, RuntimeSpawner, SendError, DEFAULT_QUANTUM,
};
pub use supervisor::{ChildSpec, Supervisor, SupervisorConfig};
