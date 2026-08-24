use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{Chunk, Instruction, Opcode, Value};

use super::fault::Fault;
use super::frame::Frame;
use super::native::NativeTable;
use super::result::VmResult;

/// Hard limit on call nesting. Frames are heap-allocated (see [`Frame`]), so
/// unbounded recursion would grow the process's memory instead of crashing
/// the worker thread's native stack — which is worse, not better, without a
/// limit. `4096` comfortably covers real recursive algorithms while keeping
/// a runaway `fn f() { f() }` a `Fault`, not an OOM.
pub const MAX_CALL_DEPTH: usize = 4096;

/// One virtual process's execution state: call stack + registers. Cheap
/// enough to construct that spawning a process is a handful of small heap
/// allocations, not a native thread/stack (contrast: a `std::thread` reserves
/// megabytes of stack whether it uses them or not).
///
/// `Vm` owns no scheduler, mailbox, or thread handle — see [`VmResult`] for
/// why that separation is the whole point.
pub struct Vm {
    chunk: Arc<Chunk>,
    natives: Arc<NativeTable>,
    frames: Vec<Frame>,
    /// Lifetime instruction counter, exposed for `ProcessMetrics` (design
    /// notes §26).
    instructions_executed: u64,
}

impl Vm {
    /// Construct a `Vm` ready to run `function` (an index into
    /// `chunk.functions`) with the given arguments loaded into `r0..argc`.
    /// `natives` is the FFI table `Opcode::CallNative` dispatches through —
    /// pass [`NativeTable::empty`] if the chunk never calls out to Rust.
    pub fn new(chunk: Arc<Chunk>, natives: Arc<NativeTable>, function: u32, args: &[Value]) -> Result<Self, Fault> {
        let def = chunk
            .function(function)
            .ok_or(Fault::BadFunction { index: function, table_size: chunk.functions.len() as u32 })?;
        let mut frame = Frame::new(function, def.num_registers, None);
        frame.pc = def.entry as usize;
        for (i, arg) in args.iter().enumerate().take(def.arity as usize) {
            frame.registers[i] = arg.clone();
        }
        Ok(Vm { chunk, natives, frames: vec![frame], instructions_executed: 0 })
    }

    pub fn instructions_executed(&self) -> u64 {
        self.instructions_executed
    }

    /// Index into `chunk.functions` for the active (top) call frame.
    /// Useful for diagnostics / supervisor logs when a process traps.
    pub fn current_function(&self) -> u32 {
        self.frames
            .last()
            .expect("Vm invariant: frames is never empty while running")
            .function
    }

    /// A cheap `Arc` clone of the chunk this VM is executing. Used by the
    /// scheduler to construct a child `Vm` for `Opcode::Spawn` without
    /// needing to know anything about `Chunk`'s internals — every process
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

    /// Deliver a value the scheduler produced on our behalf (the new Pid
    /// from a `Spawn`, or a dequeued mailbox message from a `Receive`) into
    /// the register the instruction that suspended us was targeting, ahead
    /// of the next [`Vm::run`] call. A no-op is never valid to skip: calling
    /// `run` without this after a `Spawn`/`Receive` result leaves the
    /// destination register holding its previous (stale) value.
    #[inline]
    pub fn resume_with(&mut self, dest_reg: u8, value: Value) -> Result<(), Fault> {
        self.set_reg(dest_reg, value)
    }

    #[inline]
    fn current(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("Vm invariant: frames is never empty while running")
    }

    #[inline]
    fn get_reg(&self, reg: u8) -> Result<Value, Fault> {
        let frame = self.frames.last().expect("non-empty");
        frame
            .registers
            .get(reg as usize)
            .cloned()
            .ok_or(Fault::RegisterOutOfRange { reg, frame_size: frame.registers.len() as u8 })
    }

    #[inline]
    fn set_reg(&mut self, reg: u8, value: Value) -> Result<(), Fault> {
        let frame = self.current();
        let len = frame.registers.len() as u8;
        match frame.registers.get_mut(reg as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(Fault::RegisterOutOfRange { reg, frame_size: len }),
        }
    }

    /// Fetch the next instruction and advance `pc`. Returns `None` if
    /// control fell off the end of the function body without an explicit
    /// `Return`/`Halt` — treated as an implicit `Return Unit`, matching the
    /// "functions falling off the end return unit" convention rather than
    /// faulting, which is friendlier to hand-written/generated bytecode
    /// that omits a trailing return.
    #[inline]
    fn fetch(&mut self) -> Option<Instruction> {
        let pc = self.current().pc;
        let instr = self.chunk.code.get(pc).copied();
        if instr.is_some() {
            self.current().pc = pc + 1;
        }
        instr
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
            _ => unreachable!("numeric_binop called with non-arithmetic opcode {op:?}"),
        };
        self.set_reg(dst, result)
    }

    /// Run at most `budget` instructions (cooperative-preemption quantum,
    /// design notes §10-11), or until the process completes / needs an
    /// effect the scheduler must perform / faults.
    ///
    /// Every exit path is captured by [`VmResult`] — this function itself
    /// never panics on malformed *verified* bytecode; faults are returned,
    /// not thrown, so a buggy process can't take a worker thread down.
    pub fn run(&mut self, budget: u32) -> VmResult {
        for _ in 0..budget {
            self.instructions_executed += 1;
            let instr = match self.fetch() {
                Some(i) => i,
                None => {
                    // Fell off the end of a function: implicit `return Unit`.
                    match self.pop_frame(Value::Unit) {
                        Some(result) => return result,
                        None => continue,
                    }
                }
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
                    let target = self.current().pc as i64 + instr.imm as i64;
                    self.current().pc = target as usize;
                }
                Opcode::Branch => {
                    let cond = trap!(self.get_reg(instr.a));
                    if !cond.is_truthy() {
                        let target = self.current().pc as i64 + instr.imm as i64;
                        self.current().pc = target as usize;
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
                        args.push(trap!(self.get_reg(dst + i)));
                    }
                    let mut new_frame = Frame::new(function, def.num_registers, Some(dst));
                    new_frame.pc = def.entry as usize;
                    for (i, a) in args.into_iter().enumerate().take(def.arity as usize) {
                        new_frame.registers[i] = a;
                    }
                    self.frames.push(new_frame);
                }
                Opcode::CallNative => {
                    let native_index = instr.imm as u32;
                    let argc = instr.b;
                    let dst = instr.a;
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
                        args.push(trap!(self.get_reg(dst + i)));
                    }
                    // Runs inline on this worker thread — see
                    // `NativeFn`'s doc comment on why natives must not
                    // block. This is the actual FFI boundary (design
                    // notes §30-31): plain Rust on one side, bytecode
                    // registers on the other, with `Fault::NativeError`
                    // as the only channel for a native-side failure to
                    // become a process fault instead of a host panic.
                    match native_fn(&args) {
                        Ok(value) => trap!(self.set_reg(dst, value)),
                        Err(fault) => return VmResult::Trap(fault),
                    }
                }
                Opcode::Return => {
                    let v = trap!(self.get_reg(instr.a));
                    if let Some(result) = self.pop_frame(v) {
                        return result;
                    }
                }
                Opcode::Spawn => {
                    let argc = instr.b;
                    let mut args = Vec::with_capacity(argc as usize);
                    for i in 0..argc {
                        // Args live at r[a+1 .. a+1+argc] — deliberately
                        // offset from `a` itself, which the scheduler will
                        // overwrite with the new process's Pid once it
                        // exists (see Opcode::Spawn's doc comment).
                        args.push(trap!(self.get_reg(instr.a + 1 + i)));
                    }
                    return VmResult::Spawn { function: instr.imm as u32, args, dest_reg: instr.a };
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
                    let pid = match target.as_pid() {
                        Some(p) => p,
                        None => {
                            return VmResult::Trap(Fault::TypeMismatch { expected: "pid", got: target.type_name() })
                        }
                    };
                    return VmResult::Send { target: pid, message };
                }
                Opcode::Receive => {
                    return VmResult::Receive { dest_reg: instr.a, timeout: None };
                }
                Opcode::ReceiveTimeout => {
                    let millis = trap!(self.get_reg(instr.b));
                    let ms = millis.as_int().unwrap_or(0).max(0) as u64;
                    return VmResult::Receive { dest_reg: instr.a, timeout: Some(Duration::from_millis(ms)) };
                }
                Opcode::Trap => return VmResult::Trap(Fault::Explicit(instr.imm)),
            }
        }
        VmResult::Yield
    }

    /// Pop the current frame, delivering `value` to the caller (or
    /// finishing the process if this was the outermost frame). Returns
    /// `Some(VmResult::Complete(_))` only in the latter case.
    fn pop_frame(&mut self, value: Value) -> Option<VmResult> {
        let finished = self.frames.pop().expect("non-empty");
        match finished.dest_reg {
            Some(dest) => {
                // Caller frame still on the stack; ignore an out-of-range
                // dest (shouldn't happen for verified bytecode emitted by
                // this crate's own builder, but we don't want to panic on
                // foreign bytecode) by trapping instead.
                if self.set_reg(dest, value).is_err() {
                    return Some(VmResult::Trap(Fault::RegisterOutOfRange {
                        reg: dest,
                        frame_size: self.frames.last().map(|f| f.registers.len() as u8).unwrap_or(0),
                    }));
                }
                None
            }
            None => Some(VmResult::Complete(value)),
        }
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