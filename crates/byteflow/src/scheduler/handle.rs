use super::oneshot;
use super::process::{FlowId, FlowOutcome};

/// A reference to a spawned **flow**, returned by
/// [`super::runtime::Runtime::spawn`].
///
/// Holding a `FlowHandle` does not keep the flow alive — it is a way to
/// (a) read its [`FlowId`] for addressing with `Send`, and (b) block the
/// *calling native thread* until it finishes via [`FlowHandle::join`].
/// Dropping without joining is fine (fire-and-forget).
pub struct FlowHandle {
    pub(crate) id: FlowId,
    pub(crate) receiver: oneshot::Receiver<FlowOutcome>,
}

impl FlowHandle {
    pub fn id(&self) -> FlowId {
        self.id
    }

    /// Block the current (native) thread until the flow terminates.
    /// **Never call from inside a worker / from bytecode.**
    pub fn join(self) -> FlowOutcome {
        match self.receiver.join() {
            Ok(outcome) => outcome,
            Err(e) => FlowOutcome::Failed(e.to_string()),
        }
    }
}
