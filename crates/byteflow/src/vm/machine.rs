use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{Chunk, Instruction, Opcode, Value};

use super::fault::Fault;
use super::frame::Frame;
use super::native::{check_native_gate, NativeGate, NativeTable};
use super::result::VmResult;

/// Hard limit on call nesting. Frames are heap-allocated, so
/// unbounded recursion would grow the Flow's memory instead of crashing
/// the worker thread's native stack — which is worse, not better, without a
/// limit. `4096` comfortably covers real recursive algorithms while keeping
/// a runaway `fn f() { f() }` a `Fault`, not an OOM.
pub const MAX_CALL_DEPTH: usize = 4096;

/// One virtual Flow's execution state: call stack + registers. Cheap
/// enough to construct that spawning a Flow is a handful of small heap
/// allocations, not a native thread/stack (contrast: a `std::thread` reserves
/// megabytes of stack whether it uses them or not).
///
/// `Vm` owns no scheduler, mailbox, or thread handle — see [`VmResult`] for
/// why that separation is the whole point.
pub struct Vm {
    chunk: Arc<Chunk>,
    natives: Arc<NativeTable>,
    native_gate: NativeGate,
    frames: Vec<Frame>,
    /// Lifetime instruction counter, exposed for `FlowMetrics` (design
    /// notes §26).
    instructions_executed: u64,
}

impl Vm {
    /// Construct a `Vm` ready to run `function` (an index into
    /// `chunk.functions`) with the given arguments loaded into `r0..argc`.
    /// `natives` is the FFI table `Opcode::CallNative` dispatches through —
    /// pass [`NativeTable::empty`] if the chunk never calls out to Rust.
    ///
    /// Native calls are **denied** until [`Self::with_native_gate`] installs
    /// an allowlist (the runtime does this from the flow's attenuated Cap).
    pub fn new(chunk: Arc<Chunk>, natives: Arc<NativeTable>, function: u32, args: &[Value]) -> Result<Self, Fault> {
        let gate = NativeGate::deny(natives.len());
        Self::with_native_gate(chunk, natives, gate, function, args)
    }

    pub fn with_native_gate(
        chunk: Arc<Chunk>,
        natives: Arc<NativeTable>,
        native_gate: NativeGate,
        function: u32,
        args: &[Value],
    ) -> Result<Self, Fault> {
        let def = chunk
            .function(function)
            .ok_or(Fault::BadFunction { index: function, table_size: chunk.functions.len() as u32 })?;
        let mut frame = Frame::new(function, def.num_registers, None);
        frame.pc = def.entry as usize;
        for (i, arg) in args.iter().enumerate().take(def.arity as usize) {
            match frame.registers.get_mut(i) {
                Some(slot) => *slot = arg.clone(),
                None => {
                    return Err(Fault::RegisterOutOfRange {
                        reg: i as u8,
                        frame_size: def.num_registers,
                    })
                }
            }
        }
        Ok(Vm {
            chunk,
            natives,
            native_gate,
            frames: vec![frame],
            instructions_executed: 0,
        })
    }

    pub fn instructions_executed(&self) -> u64 {
        self.instructions_executed
    }

    /// Index into `chunk.functions` for the active (top) call frame.
    /// Useful for diagnostics / supervisor logs when a Flow traps.
    pub fn current_function(&self) -> u32 {
        // Category D: frames must be non-empty while the VM is runnable.
        // We still avoid `.expect` — return 0 as a diagnostic fallback so a
        // broken invariant cannot panic a worker; the next `run` will trap.
        debug_assert!(!self.frames.is_empty(), "frames empty while running");
        match self.frames.last() {
            Some(frame) => frame.function,
            None => 0,
        }
    }

    /// A cheap `Arc` clone of the chunk this VM is executing. Used by the
    /// scheduler to construct a child `Vm` for `Opcode::Spawn` without
    /// needing to know anything about `Chunk`'s internals — every Flow
    /// spawned (transitively) from the same top-level `spawn()` call shares
    /// one immutable chunk in memory, never copies it.
    pub fn chunk_arc(&self) -> Arc<Chunk> {
        self.chunk.clone()
    }

    /// A cheap `Arc` clone of this VM's native function table, for the same
    /// reason as [`Vm::chunk_arc`]: a `Spawn`-created child must dispatch
    /// `CallNative` through the identical table its parent uses.
    pub fn natives_arc(&self) -> Arc<NativeTable> {
        self.natives.clone()
    }

    /// Deliver a value the scheduler produced on our behalf (a **Cap** from
    /// `Spawn` / `SelfPid`, or a dequeued mailbox message from a `Receive`)
    /// into the register the instruction that suspended us was targeting,
    /// ahead of the next [`Vm::run`] call. A no-op is never valid to skip:
    /// calling `run` without this after a `Spawn`/`Receive` result leaves the
    /// destination register holding its previous (stale) value.
    #[inline]
    pub fn resume_with(&mut self, dest_reg: u8, value: Value) -> Result<(), Fault> {
        self.set_reg(dest_reg, value)
    }

    /// Program counter of the active frame — used by the optional JIT hook.
    pub fn current_pc(&self) -> Option<usize> {
        self.frames.last().map(|f| f.pc)
    }

    /// Number of registers in the active frame.
    pub fn current_num_registers(&self) -> Option<u8> {
        self.frames.last().map(|f| f.registers.len() as u8)
    }

    /// Read-only view of the active register file.
    pub fn top_registers(&self) -> Option<&[Value]> {
        self.frames.last().map(|f| f.registers.as_slice())
    }

    /// Mutable view of the active register file (JIT sync path).
    pub fn top_registers_mut(&mut self) -> Option<&mut [Value]> {
        self.frames.last_mut().map(|f| f.registers.as_mut_slice())
    }

    /// Set the active frame's program counter.
    pub fn set_pc(&mut self, pc: usize) {
        if let Some(frame) = self.frames.last_mut() {
            frame.pc = pc;
        }
    }

    /// Write one register in the active frame (JIT sync path).
    pub fn set_register(&mut self, reg: u8, value: Value) -> Result<(), Fault> {
        self.set_reg(reg, value)
    }

    /// Deliver a return value through the call stack.
    pub fn return_value(&mut self, value: Value) -> Result<Option<VmResult>, Fault> {
        self.pop_frame(value)
    }

    /// Top call frame. Empty stack is a broken invariant (category D) —
    /// returned as [`Fault::Invariant`], never as `unwrap`/`expect`.
    #[inline]
    fn current(&mut self) -> Result<&mut Frame, Fault> {
        debug_assert!(!self.frames.is_empty(), "frames empty while running");
        self.frames
            .last_mut()
            .ok_or(Fault::Invariant("empty frame stack while running"))
    }

    #[inline]
    fn get_reg(&self, reg: u8) -> Result<Value, Fault> {
        let frame = self
            .frames
            .last()
            .ok_or(Fault::Invariant("empty frame stack while running"))?;
        frame
            .registers
            .get(reg as usize)
            .cloned()
            .ok_or(Fault::RegisterOutOfRange {
                reg,
                frame_size: frame.registers.len() as u8,
            })
    }

    #[inline]
    fn set_reg(&mut self, reg: u8, value: Value) -> Result<(), Fault> {
        let frame = self.current()?;
        let len = frame.registers.len() as u8;
        match frame.registers.get_mut(reg as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(Fault::RegisterOutOfRange { reg, frame_size: len }),
        }
    }

    /// Fetch the next instruction and advance `pc`.
    ///
    /// `Ok(None)` if control fell off the end of the function body without an
    /// explicit `Return`/`Halt` — treated as an implicit `Return Unit`
    /// (friendlier to hand-written bytecode that omits a trailing return).
    /// `Err` if the frame stack is empty (invariant break).
    ///
    /// Borrows are split on purpose: hold `pc`, look up `chunk.code`, then
    /// write `pc+1` — a single `&mut Frame` across `self.chunk` would not
    /// compile.
    #[inline]
    fn fetch(&mut self) -> Result<Option<Instruction>, Fault> {
        let pc = self.current()?.pc;
        let instr = self.chunk.code.get(pc).copied();
        if instr.is_some() {
            self.current()?.pc = pc + 1;
        }
        Ok(instr)
    }

    fn numeric_binop(&mut self, op: Opcode, dst: u8, lhs: u8, rhs: u8) -> Result<(), Fault> {
        let a = self.get_reg(lhs)?;
        let b = self.get_reg(rhs)?;
        let result = match (op, &a, &b) {
            (Opcode::Add, Value::Int(x), Value::Int(y)) => Value::Int(x.wrapping_add(*y)),
            (Opcode::Add, _, _) => Value::Float(as_f64(&a)? + as_f64(&b)?),
            (Opcode::Sub, Value::Int(x), Value::Int(y)) => Value::Int(x.wrapping_sub(*y)),
            (Opcode::Sub, _, _) => Value::Float(as_f64(&a)? - as_f64(&b)?),
            (Opcode::Mul, Value::Int(x), Value::Int(y)) => Value::Int(x.wrapping_mul(*y)),
            (Opcode::Mul, _, _) => Value::Float(as_f64(&a)? * as_f64(&b)?),
            (Opcode::Div, Value::Int(x), Value::Int(y)) => {
                if *y == 0 {
                    return Err(Fault::DivideByZero);
                }
                Value::Int(x.wrapping_div(*y))
            }
            (Opcode::Div, _, _) => {
                let denom = as_f64(&b)?;
                Value::Float(as_f64(&a)? / denom)
            }
            (Opcode::Mod, Value::Int(x), Value::Int(y)) => {
                if *y == 0 {
                    return Err(Fault::DivideByZero);
                }
                Value::Int(x.wrapping_rem(*y))
            }
            (Opcode::Eq, _, _) => Value::Bool(a == b),
            (Opcode::Lt, Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
            (Opcode::Lt, _, _) => Value::Bool(as_f64(&a)? < as_f64(&b)?),
            (Opcode::Le, Value::Int(x), Value::Int(y)) => Value::Bool(x <= y),
            (Opcode::Le, _, _) => Value::Bool(as_f64(&a)? <= as_f64(&b)?),
            _ => {
                return Err(Fault::Invariant(
                    "numeric_binop called with a non-arithmetic opcode",
                ))
            }
        };
        self.set_reg(dst, result)
    }

    /// Run at most `budget` instructions (cooperative-preemption quantum,
    /// design notes §10-11), or until the Flow completes / needs an
    /// effect the scheduler must perform / faults.
    ///
    /// Every exit path is captured by [`VmResult`] — this function itself
    /// never panics on malformed *verified* bytecode; faults are returned,
    /// not thrown, so a buggy Flow can't take a worker thread down.
    pub fn run(&mut self, budget: u32) -> VmResult {
        for _ in 0..budget {
            self.instructions_executed += 1;
            let instr = match self.fetch() {
                Ok(Some(i)) => i,
                Ok(None) => {
                    // Fell off the end of a function: implicit `return Unit`.
                    match self.pop_frame(Value::Unit) {
                        Ok(Some(result)) => return result,
                        Ok(None) => continue,
                        Err(fault) => return VmResult::Trap(fault),
                    }
                }
                Err(fault) => return VmResult::Trap(fault),
            };

            macro_rules! trap {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(fault) => return VmResult::Trap(fault),
                    }
                };
            }

            match instr.op {
                Opcode::Halt => {
                    let v = trap!(self.get_reg(0));
                    return VmResult::Complete(v);
                }
                Opcode::Nop => {}
                Opcode::LoadConst => {
                    let idx = instr.imm as u32;
                    let konst = match self.chunk.constant(idx) {
                        Some(v) => v.clone(),
                        None => {
                            return VmResult::Trap(Fault::BadConstant {
                                index: idx,
                                pool_size: self.chunk.constants.len() as u32,
                            })
                        }
                    };
                    trap!(self.set_reg(instr.a, konst));
                }
                Opcode::LoadImm => {
                    trap!(self.set_reg(instr.a, Value::Int(instr.imm as i64)));
                }
                Opcode::Move => {
                    let v = trap!(self.get_reg(instr.b));
                    trap!(self.set_reg(instr.a, v));
                }
                Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div | Opcode::Mod
                | Opcode::Eq | Opcode::Lt | Opcode::Le => {
                    trap!(self.numeric_binop(instr.op, instr.a, instr.b, instr.c));
                }
                Opcode::Neg => {
                    let v = trap!(self.get_reg(instr.b));
                    let negated = match v {
                        Value::Int(x) => Value::Int(-x),
                        Value::Float(x) => Value::Float(-x),
                        other => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "int or float",
                                got: other.type_name(),
                            })
                        }
                    };
                    trap!(self.set_reg(instr.a, negated));
                }
                Opcode::Jump => {
                    let frame = trap!(self.current());
                    let target = frame.pc as i64 + instr.imm as i64;
                    frame.pc = target as usize;
                }
                Opcode::Branch => {
                    let cond = trap!(self.get_reg(instr.a));
                    if !cond.is_truthy() {
                        let frame = trap!(self.current());
                        let target = frame.pc as i64 + instr.imm as i64;
                        frame.pc = target as usize;
                    }
                }
                Opcode::Call => {
                    let function = instr.imm as u32;
                    let argc = instr.b;
                    let dst = instr.a;
                    if self.frames.len() >= MAX_CALL_DEPTH {
                        return VmResult::Trap(Fault::CallStackOverflow { depth: self.frames.len() });
                    }
                    let def = match self.chunk.function(function) {
                        Some(d) => d.clone(),
                        None => {
                            return VmResult::Trap(Fault::BadFunction {
                                index: function,
                                table_size: self.chunk.functions.len() as u32,
                            })
                        }
                    };
                    let mut args = Vec::with_capacity(argc as usize);
                    for i in 0..argc {
                        args.push(trap!(self.get_reg(trap!(reg_at(dst, u16::from(i))))));
                    }
                    let mut new_frame = Frame::new(function, def.num_registers, Some(dst));
                    new_frame.pc = def.entry as usize;
                    // See `Vm::new` on why this is a checked write.
                    for (i, a) in args.into_iter().enumerate().take(def.arity as usize) {
                        match new_frame.registers.get_mut(i) {
                            Some(slot) => *slot = a,
                            None => {
                                return VmResult::Trap(Fault::RegisterOutOfRange {
                                    reg: i as u8,
                                    frame_size: def.num_registers,
                                })
                            }
                        }
                    }
                    self.frames.push(new_frame);
                }
                Opcode::CallNative => {
                    let native_index = instr.imm as u32;
                    let argc = instr.b;
                    let dst = instr.a;
                    if let Err(err) = check_native_gate(&self.native_gate, &self.natives, native_index)
                    {
                        return VmResult::Trap(match err {
                            crate::vm::native::NativeCallError::IndexOutOfRange(index) => {
                                Fault::BadNative {
                                    index,
                                    table_size: self.natives.len() as u32,
                                }
                            }
                            other => Fault::NativeDenied(other.to_string()),
                        });
                    }
                    let native_fn = match self.natives.get(native_index) {
                        Some(f) => f.clone(),
                        None => {
                            return VmResult::Trap(Fault::BadNative {
                                index: native_index,
                                table_size: self.natives.len() as u32,
                            })
                        }
                    };
                    let mut args = Vec::with_capacity(argc as usize);
                    for i in 0..argc {
                        args.push(trap!(self.get_reg(trap!(reg_at(dst, u16::from(i))))));
                    }
                    // Runs inline on this worker thread — see
                    // `NativeFn`'s doc comment on why natives must not
                    // block. This is the actual FFI boundary (design
                    // notes §30-31): plain Rust on one side, bytecode
                    // registers on the other, with `Fault::NativeError`
                    // as the only channel for a native-side failure to
                    // become a Flow fault instead of a host panic.
                    match native_fn(&args) {
                        Ok(value) => trap!(self.set_reg(dst, value)),
                        Err(fault) => return VmResult::Trap(fault),
                    }
                }
                Opcode::Return => {
                    let v = trap!(self.get_reg(instr.a));
                    match self.pop_frame(v) {
                        Ok(Some(result)) => return result,
                        Ok(None) => {}
                        Err(fault) => return VmResult::Trap(fault),
                    }
                }
                Opcode::Spawn => {
                    let argc = instr.b;
                    let mut args = Vec::with_capacity(argc as usize);
                    for i in 0..argc {
                        // Args live at r[a+1 .. a+1+argc] — deliberately
                        // offset from `a` itself, which the scheduler will
                        // overwrite with a Cap to the child once it exists
                        // (see Opcode::Spawn / FlowCap).
                        //
                        // The `+1` is why this one needs `reg_at` most: with
                        // `a = 255` the very first index already leaves the
                        // register space, before any bounds check gets a say.
                        args.push(trap!(
                            self.get_reg(trap!(reg_at(instr.a, u16::from(i) + 1)))
                        ));
                    }
                    return VmResult::Spawn {
                        function: instr.imm as u32,
                        args,
                        dest_reg: instr.a,
                        requested_rights: crate::bytecode::CapRights::from_u8(instr.c),
                    };
                }
                Opcode::Yield => return VmResult::Yield,
                Opcode::Sleep => {
                    let millis = trap!(self.get_reg(instr.a));
                    let ms = match millis.as_int() {
                        Some(ms) if ms >= 0 => ms as u64,
                        _ => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "non-negative int",
                                got: millis.type_name(),
                            })
                        }
                    };
                    return VmResult::Sleep(Duration::from_millis(ms));
                }
                Opcode::Exit => {
                    let v = trap!(self.get_reg(instr.a));
                    return VmResult::Complete(v);
                }
                Opcode::SelfPid => {
                    return VmResult::SelfPid { dest_reg: instr.a };
                }
                Opcode::Send => {
                    let target = trap!(self.get_reg(instr.a));
                    let message = trap!(self.get_reg(instr.b));
                    let cap = match target.as_cap() {
                        Some(c) => c,
                        None => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "cap",
                                got: target.type_name(),
                            })
                        }
                    };
                    if message.as_message().is_none() {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "message",
                            got: message.type_name(),
                        });
                    }
                    return VmResult::Send {
                        target_cap: cap,
                        message,
                    };
                }
                Opcode::Receive => {
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: None,
                    };
                }
                Opcode::ReceiveTimeout => {
                    let millis = trap!(self.get_reg(instr.b));
                    let ms = match millis.as_int() {
                        Some(n) if n >= 0 => n as u64,
                        _ => 0,
                    };
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: Some(Duration::from_millis(ms)),
                        match_tag: None,
                    };
                }
                Opcode::ReceiveMatch => {
                    let tag_v = trap!(self.get_reg(instr.b));
                    let tag = match tag_from_value(&tag_v) {
                        Ok(t) => t,
                        Err(f) => return VmResult::Trap(f),
                    };
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                    };
                }
                Opcode::ReceiveMatchImm => {
                    let tag = match u16::try_from(instr.imm) {
                        Ok(t) if instr.imm >= 0 => t,
                        _ => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "tag u16",
                                got: "imm-out-of-range",
                            })
                        }
                    };
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                    };
                }
                Opcode::Ask => {
                    let target = trap!(self.get_reg(instr.b));
                    let request = trap!(self.get_reg(instr.c));
                    let cap = match target.as_cap() {
                        Some(c) => c,
                        None => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "cap",
                                got: target.type_name(),
                            })
                        }
                    };
                    if request.as_message().is_none() {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "message",
                            got: request.type_name(),
                        });
                    }
                    return VmResult::Ask {
                        dest_reg: instr.a,
                        target_cap: cap,
                        request,
                        timeout: None,
                    };
                }
                Opcode::AskTimeout => {
                    let target = trap!(self.get_reg(instr.b));
                    let request = trap!(self.get_reg(instr.c));
                    let millis_reg = match u8::try_from(instr.imm) {
                        Ok(r) => r,
                        Err(_) => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "millis register",
                                got: "imm-out-of-range",
                            })
                        }
                    };
                    let millis = trap!(self.get_reg(millis_reg));
                    let ms = match millis.as_int() {
                        Some(n) if n >= 0 => n as u64,
                        _ => 0,
                    };
                    let cap = match target.as_cap() {
                        Some(c) => c,
                        None => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "cap",
                                got: target.type_name(),
                            })
                        }
                    };
                    if request.as_message().is_none() {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "message",
                            got: request.type_name(),
                        });
                    }
                    return VmResult::Ask {
                        dest_reg: instr.a,
                        target_cap: cap,
                        request,
                        timeout: Some(Duration::from_millis(ms)),
                    };
                }
                Opcode::Monitor => {
                    let target = trap!(self.get_reg(instr.b));
                    let cap = match target.as_cap() {
                        Some(c) => c,
                        None => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "cap",
                                got: target.type_name(),
                            })
                        }
                    };
                    return VmResult::Monitor {
                        dest_reg: instr.a,
                        target_cap: cap,
                    };
                }
                Opcode::Demonitor => {
                    return VmResult::Demonitor {
                        monitor_reg: instr.a,
                    };
                }
                Opcode::Link => {
                    let target = trap!(self.get_reg(instr.b));
                    let cap = match target.as_cap() {
                        Some(c) => c,
                        None => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "cap",
                                got: target.type_name(),
                            })
                        }
                    };
                    return VmResult::Link {
                        dest_reg: instr.a,
                        target_cap: cap,
                    };
                }
                Opcode::Unlink => {
                    return VmResult::Unlink {
                        link_reg: instr.a,
                    };
                }
                Opcode::Delegate => {
                    let src = trap!(self.get_reg(instr.b));
                    let src_cap = match src.as_cap() {
                        Some(c) => c,
                        None => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "cap",
                                got: src.type_name(),
                            })
                        }
                    };
                    let want_native_cap = if instr.c == 255 {
                        None
                    } else {
                        let v = trap!(self.get_reg(instr.c));
                        match v.as_cap() {
                            Some(c) => Some(c),
                            None => {
                                return VmResult::Trap(Fault::TypeMismatch {
                                    expected: "cap",
                                    got: v.type_name(),
                                })
                            }
                        }
                    };
                    return VmResult::Delegate {
                        dest_reg: instr.a,
                        src_cap,
                        want_rights: crate::bytecode::CapRights::from_bits(instr.imm as u32),
                        want_native_cap,
                    };
                }
                Opcode::Trap => return VmResult::Trap(Fault::Explicit(instr.imm)),
            }
        }
        VmResult::Yield
    }

    /// Pop the current frame, delivering `value` to the caller (or
    /// finishing the Flow if this was the outermost frame). Returns
    /// `Ok(Some(VmResult::Complete(_)))` only in the latter case.
    fn pop_frame(&mut self, value: Value) -> Result<Option<VmResult>, Fault> {
        let finished = self
            .frames
            .pop()
            .ok_or(Fault::Invariant("pop_frame on empty stack"))?;
        match finished.dest_reg {
            Some(dest) => {
                // Caller frame still on the stack; ignore an out-of-range
                // dest (shouldn't happen for verified bytecode emitted by
                // this crate's own builder, but we don't want to panic on
                // foreign bytecode) by trapping instead.
                if self.set_reg(dest, value).is_err() {
                    let frame_size = match self.frames.last() {
                        Some(f) => f.registers.len() as u8,
                        None => 0,
                    };
                    return Ok(Some(VmResult::Trap(Fault::RegisterOutOfRange {
                        reg: dest,
                        frame_size,
                    })));
                }
                Ok(None)
            }
            None => Ok(Some(VmResult::Complete(value))),
        }
    }
}

/// The register index `base + offset`, or [`Fault::RegisterIndexOverflow`]
/// if that sum leaves the register index space.
///
/// Every multi-operand opcode gathers its arguments from consecutive
/// registers. Written as a plain `base + offset` on `u8`, that addition
/// panics in debug and **wraps** in release — so a release build silently
/// reads the wrong register instead of failing, which is the worst of the
/// two outcomes and the one a debug-mode test suite never sees. The sum is
/// therefore computed in a wider type and narrowed explicitly.
#[inline]
fn reg_at(base: u8, offset: u16) -> Result<u8, Fault> {
    match u8::try_from(u32::from(base) + u32::from(offset)) {
        Ok(reg) => Ok(reg),
        Err(_) => Err(Fault::RegisterIndexOverflow {
            base,
            offset: match u8::try_from(offset) {
                Ok(o) => o,
                Err(_) => u8::MAX,
            },
        }),
    }
}

#[inline]
fn as_f64(v: &Value) -> Result<f64, Fault> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(Fault::TypeMismatch { expected: "int or float", got: other.type_name() }),
    }
}

/// Decode a Message tag from a register value (`Int` in `0..=u16::MAX`).
#[inline]
fn tag_from_value(v: &Value) -> Result<u16, Fault> {
    match v.as_int() {
        Some(i) if (0..=i64::from(u16::MAX)).contains(&i) => Ok(i as u16),
        Some(_) => Err(Fault::TypeMismatch {
            expected: "tag u16",
            got: "int-out-of-range",
        }),
        None => Err(Fault::TypeMismatch {
            expected: "int",
            got: v.type_name(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// `Spawn a=255` reads its arguments from `a+1`, so the first index
    /// already leaves the register space. This used to panic in debug and —
    /// far worse — wrap around to `r0` in release, silently spawning with the
    /// wrong argument.
    #[test]
    fn spawn_from_the_last_register_traps_instead_of_wrapping() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 2);
        b.emit_spawn(255, 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::RegisterIndexOverflow {
                base: 255,
                offset: 1,
            }) => Ok(()),
            other => Err(format!("expected RegisterIndexOverflow, got {other:?}").into()),
        }
    }

    /// `Vm::new` is public and reachable without `verify`, and its panic
    /// landed on the caller's thread — the embedder's on `Runtime::spawn`, or
    /// a worker's on bytecode `Spawn`, outside the `catch_unwind`.
    #[test]
    fn entering_a_function_with_too_few_registers_faults() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 3, 1);
        b.emit_return(0);
        let args = [Value::Int(1), Value::Int(2), Value::Int(3)];
        match Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &args) {
            Err(Fault::RegisterOutOfRange {
                reg: 1,
                frame_size: 1,
            }) => Ok(()),
            Err(e) => Err(format!("unexpected fault: {e}").into()),
            Ok(_) => Err("three arguments cannot be loaded into one register".into()),
        }
    }

    #[test]
    fn calling_a_function_with_too_few_registers_traps() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let callee = b.begin_function("callee", 3, 1);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 4);
        b.emit_load_imm(0, 7);
        b.emit_load_imm(1, 8);
        b.emit_load_imm(2, 9);
        b.emit_call(0, callee, 3);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
        match vm.run(50) {
            VmResult::Trap(Fault::RegisterOutOfRange {
                reg: 1,
                frame_size: 1,
            }) => Ok(()),
            other => Err(format!("expected RegisterOutOfRange, got {other:?}").into()),
        }
    }

    /// The boundary case that must keep working: gathering right up to the
    /// last register is legal, it is only going *past* it that faults.
    #[test]
    fn gathering_up_to_the_last_register_still_works() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let callee = b.begin_function("callee", 1, 1);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 255);
        b.emit_load_imm(254, 5);
        // Argument at r254 — the highest index a 255-register frame has.
        b.emit_call(254, callee, 1);
        b.emit_return(254);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
        match vm.run(100) {
            VmResult::Complete(_) => Ok(()),
            other => Err(format!("expected completion, got {other:?}").into()),
        }
    }
}