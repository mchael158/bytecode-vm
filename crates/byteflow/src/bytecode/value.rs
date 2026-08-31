use std::fmt;
use std::sync::Arc;

/// Reserved Atomic Hop tag for monitor `DOWN` events (not an application tag).
pub const TAG_SYS_DOWN: u16 = 0xFF01;
/// Reserved Atomic Hop tag for linked-exit notices.
pub const TAG_SYS_EXIT: u16 = 0xFF02;

/// Fixed-size envelope carried in mailboxes and registers (**Atomic Hop**).
///
/// # Why this exists (request-reply / typed protocols)
///
/// `Receive` delivers a single [`Value`] — not `{sender, Value}`. Without an
/// envelope, a server flow cannot learn who sent a request, and two clients
/// cannot safely share a `request_id` space.
///
/// # Security
///
/// - **`sender`**: FlowId stamped by the scheduler on bytecode `Send` / `Ask`
///   (invariant **S1**). Not a capability.
/// - **`reply_cap`**: CapId minted at the same boundary with **SEND**-only
///   rights so the recipient can answer without ambient Pid addressing
///   (phase 2). Zero means “no reply grant” (host-injected hops may omit it).
///
/// See `docs/security.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Message {
    /// Authenticated origin FlowId (`0` = trusted host / non-flow).
    pub sender: u64,
    /// Capability granting **SEND** back to [`Self::sender`], or `0`.
    pub reply_cap: u64,
    /// Client correlation token; echoed on replies (`Ask` / **S2**).
    pub request_id: u64,
    /// Protocol discriminator (opaque to the VM).
    pub tag: u16,
    /// Small protocol payload.
    pub payload: u64,
}

impl Message {
    /// Build an envelope. `sender` / `reply_cap` are placeholders until a
    /// bytecode hop is authenticated by the scheduler (`reply_cap` typically
    /// `0` here).
    pub const fn new(sender: u64, request_id: u64, tag: u16, payload: u64) -> Self {
        Self {
            sender,
            reply_cap: 0,
            request_id,
            tag,
            payload,
        }
    }

    /// Outgoing hop from host Rust (`sender` / `reply_cap` filled on delivery).
    pub const fn request(request_id: u64, tag: u16, payload: u64) -> Self {
        Self::new(0, request_id, tag, payload)
    }

    /// Reply envelope echoing `request_id` from a received hop (host path).
    pub fn reply_to(req: &Self, tag: u16, payload: u64) -> Self {
        Self::new(0, req.request_id, tag, payload)
    }

    /// Runtime lifecycle hop: monitor `DOWN` (`tag == `[`TAG_SYS_DOWN`]).
    ///
    /// `sender` is the dead flow's identity (not a Cap). `request_id` is the
    /// [`crate::MonitorRef`]. `payload` is [`crate::FlowExitReason`] as `u64`.
    pub const fn down(monitor: u64, target_flow: u64, reason: u64) -> Self {
        Self {
            sender: target_flow,
            reply_cap: 0,
            request_id: monitor,
            tag: TAG_SYS_DOWN,
            payload: reason,
        }
    }

    /// Runtime lifecycle hop: Ask target exited (`tag == `[`TAG_SYS_EXIT`]).
    ///
    /// Written into the Ask dest register when the callee dies before
    /// replying. `payload` is [`crate::FlowExitReason`].
    pub const fn linked_exit(target_flow: u64, reason: u64) -> Self {
        Self {
            sender: target_flow,
            reply_cap: 0,
            request_id: 0,
            tag: TAG_SYS_EXIT,
            payload: reason,
        }
    }

    #[inline]
    pub const fn is_down(self) -> bool {
        self.tag == TAG_SYS_DOWN
    }

    #[inline]
    pub const fn is_exit(self) -> bool {
        self.tag == TAG_SYS_EXIT
    }

    /// Stamp origin FlowId and attach a reply capability (scheduler only).
    #[inline]
    pub(crate) fn authenticate(mut self, sender: u64, reply_cap: u64) -> Self {
        self.sender = sender;
        self.reply_cap = reply_cap;
        self
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "msg{{from=flow#{}, reply=cap#{}, id={}, tag={}, payload={}}}",
            self.sender, self.reply_cap, self.request_id, self.tag, self.payload
        )
    }
}

/// A dynamically-tagged runtime value.
///
/// [`Value::Cap`] is an unforgeable address for `Send` / `Ask` (phase 2).
/// [`Value::Pid`] remains for **identity** inside authenticated messages
/// (`Message.sender` / `msg_sender`), not for ambient addressing.
///
/// [`Value::Str`] / [`Value::Bytes`] are heap payloads shared via [`Arc`] so
/// register moves and mailbox hops clone the handle, not the buffer.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Internal / message identity (FlowId as `u64`). **Not** a Send target.
    Pid(u64),
    /// Atomic Hop envelope. See [`Message`].
    Message(Message),
    /// Unforgeable capability (`CapId` as `u64`). Required for `Send` / `Ask`.
    Cap(u64),
    /// UTF-8 text (constant pool, natives, host).
    Str(Arc<str>),
    /// Opaque byte buffer (constant pool, natives, host).
    Bytes(Arc<[u8]>),
}

impl Value {
    /// Build a [`Value::Str`] from anything string-like.
    #[inline]
    pub fn str(s: impl AsRef<str>) -> Self {
        Value::Str(Arc::from(s.as_ref()))
    }

    /// Build a [`Value::Bytes`] from a byte slice.
    #[inline]
    pub fn bytes(b: impl AsRef<[u8]>) -> Self {
        Value::Bytes(Arc::from(b.as_ref()))
    }

    /// Truthiness used by `Opcode::Branch`: falsy are `Unit`, `Bool(false)`,
    /// `Int(0)`, empty [`Value::Str`], and empty [`Value::Bytes`].
    #[inline]
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Unit | Value::Bool(false) | Value::Int(0) => false,
            Value::Str(s) if s.is_empty() => false,
            Value::Bytes(b) if b.is_empty() => false,
            _ => true,
        }
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
    pub fn as_cap(&self) -> Option<u64> {
        match self {
            Value::Cap(c) => Some(*c),
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

    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    #[inline]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b.as_ref()),
            Value::Str(s) => Some(s.as_bytes()),
            _ => None,
        }
    }

    /// Bytes this value is **charged** for against a mailbox byte budget
    /// (see [`crate::MailboxBytes`]).
    ///
    /// # This is a charge model, not an RSS measurement
    ///
    /// [`Value::Str`] / [`Value::Bytes`] are `Arc`-shared: the same buffer
    /// cloned into N mailboxes exists once in memory, but each mailbox is
    /// charged the full length. That over-counts on purpose — a budget that
    /// under-counts shared payloads is not a bound at all, since a single
    /// producer could fan one large `Arc` out to every inbox and stay
    /// "within budget" everywhere while the host pays once per distinct
    /// buffer it keeps alive.
    ///
    /// The inline `size_of::<Value>()` term is included so a flood of
    /// scalar hops is also bounded, not just blob hops.
    #[inline]
    pub fn memory_size(&self) -> usize {
        std::mem::size_of::<Self>() + self.heap_size()
    }

    /// Heap bytes owned (transitively) by this value, excluding the enum
    /// itself. Zero for every scalar variant.
    #[inline]
    pub fn heap_size(&self) -> usize {
        match self {
            Value::Str(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Pid(_)
            | Value::Message(_)
            | Value::Cap(_) => 0,
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
            Value::Cap(_) => "cap",
            Value::Str(_) => "str",
            Value::Bytes(_) => "bytes",
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
            Value::Pid(p) => write!(f, "flow#{p}"),
            Value::Message(m) => write!(f, "{m}"),
            Value::Cap(c) => write!(f, "cap#{c}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "bytes[{}]", b.len()),
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
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::str(s)
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(Arc::from(s))
    }
}
impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Self {
        Value::bytes(b)
    }
}
impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(Arc::from(b))
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

    #[test]
    fn authenticate_stamps_sender_and_reply_cap() {
        let m = Message::new(999, 1, 2, 3).authenticate(42, 7);
        assert_eq!(m.sender, 42);
        assert_eq!(m.reply_cap, 7);
        assert_eq!(m.request_id, 1);
    }

    #[test]
    fn cap_is_truthy() {
        assert!(Value::Cap(1).is_truthy());
        assert_eq!(Value::Cap(3).as_cap(), Some(3));
    }

    #[test]
    fn str_and_bytes_helpers() {
        let s = Value::str("hi");
        assert_eq!(s.as_str(), Some("hi"));
        assert_eq!(s.type_name(), "str");
        assert!(s.is_truthy());
        assert!(!Value::str("").is_truthy());

        let b = Value::bytes([1u8, 2, 3]);
        assert_eq!(b.as_bytes(), Some(&[1, 2, 3][..]));
        assert_eq!(b.type_name(), "bytes");
        assert!(b.is_truthy());
        assert!(!Value::bytes([]).is_truthy());

        // Str also exposes UTF-8 bytes via as_bytes.
        assert_eq!(s.as_bytes(), Some(b"hi".as_slice()));
    }

    #[test]
    fn str_eq_compares_content() {
        assert_eq!(Value::str("a"), Value::from("a".to_owned()));
        assert_ne!(Value::str("a"), Value::str("b"));
        assert_eq!(Value::bytes([9]), Value::from(vec![9u8]));
    }
}
