use std::fmt;

/// A dynamically-tagged runtime value.
///
/// Kept deliberately small (16 bytes: 8-byte payload + tag, thanks to niche
/// optimisation the enum itself is 16 bytes on x86_64) so that a 32-register
/// frame (`[Value; 32]`) is 512 bytes — one page holds 8 full register files,
/// which matters when we have hundreds of thousands of suspended processes
/// each carrying one.
///
/// This is intentionally *not* `Copy` for `Pid`/`Bytes` in the long run (a
/// future revision adds reference-counted heap values for strings/blobs) but
/// for the v0 scalar-only ISA every variant is cheap to clone.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A process identifier, as produced by `Opcode::Spawn`.
    Pid(u64),
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

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Unit => "unit",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Pid(_) => "pid",
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
