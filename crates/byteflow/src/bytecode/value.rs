use std::fmt;

/// Fixed-size actor envelope carried in mailboxes and registers.
///
/// # Why this exists (request-reply / typed protocols)
///
/// `Receive` delivers a single [`Value`] — not `{sender, Value}`. Without an
/// envelope, a server process cannot learn who sent a request, and two clients
/// cannot safely share a `request_id` space. Packing that into one `i64` would
/// artificially cap PIDs and payloads; a heap-backed blob would force
/// allocation on every message in a runtime meant for hundreds of thousands
/// of cheap processes.
///
/// So the core grows **one** generic protocol-agnostic variant:
/// [`Value::Message`]. Satellite crates (or host code) interpret `tag` /
/// `payload`; the VM/scheduler never do.
///
/// Layout is fixed and `Copy` so cloning a register file stays cheap and the
/// eventual `no_std` MCU path does not need an allocator for messaging.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Message {
    /// Originating process id (`ProcessId` as `u64`), or `0` for host-injected
    /// events (e.g. a future IRQ pump that is not itself a bytecode process).
    pub sender: u64,
    /// Client-chosen correlation token. Unique per outstanding request *for
    /// that sender*; the server echoes it on the reply.
    pub request_id: u64,
    /// Protocol discriminator. Core treats this as opaque; the application
    /// (or a satellite crate) owns the registry of tag meanings.
    pub tag: u16,
    /// Protocol payload (ids, flags, small readings, error codes, …).
    /// Wide enough for a `u64` Pid or a packed small struct; not a substitute
    /// for large blobs (those stay out of v0).
    pub payload: u64,
}

impl Message {
    pub const fn new(sender: u64, request_id: u64, tag: u16, payload: u64) -> Self {
        Self {
            sender,
            request_id,
            tag,
            payload,
        }
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "msg{{from=pid#{}, id={}, tag={}, payload={}}}",
            self.sender, self.request_id, self.tag, self.payload
        )
    }
}

/// A dynamically-tagged runtime value.
///
/// Historically scalar-only (`Unit`/`Bool`/`Int`/`Float`/`Pid`) so a
/// 32-register frame stayed small. [`Value::Message`] adds a fixed 24-byte
/// envelope (plus discriminant/padding) — still no heap, still `Clone` by
/// bitwise copy of the fields. That trade is intentional: device-actor
/// request/reply needs correlation without packing into `i64`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A process identifier, as produced by `Opcode::Spawn`.
    Pid(u64),
    /// Actor mailbox / request-reply envelope. See [`Message`].
    Message(Message),
}

impl Value {
    /// Truthiness used by `Opcode::Branch`: everything is truthy except
    /// `Unit`, `Bool(false)` and `Int(0)`. Matches the "zero/nil is falsy"
    /// convention shared by Lua and Erlang guards, which the ISA otherwise
    /// takes cues from.
    #[inline]
    pub fn is_truthy(&self) -> bool {
        !matches!(self, Value::Unit | Value::Bool(false) | Value::Int(0))
    }

    #[inline]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    #[inline]
    pub fn as_pid(&self) -> Option<u64> {
        match self {
            Value::Pid(p) => Some(*p),
            _ => None,
        }
    }

    #[inline]
    pub fn as_message(&self) -> Option<Message> {
        match self {
            Value::Message(m) => Some(*m),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Unit => "unit",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Pid(_) => "pid",
            Value::Message(_) => "message",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => write!(f, "()"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Pid(p) => write!(f, "pid#{p}"),
            Value::Message(m) => write!(f, "{m}"),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<Message> for Value {
    fn from(m: Message) -> Self {
        Value::Message(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_is_truthy_and_round_trips_helpers() {
        let m = Message::new(7, 99, 10, 1);
        let v = Value::Message(m);
        assert!(v.is_truthy());
        assert_eq!(v.as_message(), Some(m));
        assert_eq!(v.type_name(), "message");
    }
}
