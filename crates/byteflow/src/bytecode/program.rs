//! Primary API for assembling [`Chunk`] programs in host Rust.
//!
//! Use [`Program`] to define a module and [`Fn`] to emit each function with
//! named registers instead of manual `r0` / `emit_*` bookkeeping.
//!
//! ```rust
//! use byteflow::Program;
//!
//! let mut program = Program::new("count");
//! program.function("main", 0, |f| {
//!     let limit = f.load_int(100);
//!     let counter = f.load_i32(0);
//!     f.while_lt(counter, limit, |f| f.add_imm(counter, 1));
//!     f.return_(counter);
//! });
//! let chunk = program.build();
//! ```

use super::builder::ChunkBuilder;

pub use super::builder::Label;
use super::chunk::Chunk;
use super::opcode::Opcode;
use super::value::Value;

/// Function index returned by [`Program::function`].
pub type FuncId = u32;

/// A virtual register in the current function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Reg(u8);

impl Reg {
    /// Underlying register index in the bytecode function frame.
    pub const fn index(self) -> u8 {
        self.0
    }
}

impl From<Reg> for u8 {
    fn from(r: Reg) -> u8 {
        r.0
    }
}

/// Contiguous register window (e.g. four slots for `make_msg` natives).
#[derive(Clone, Copy, Debug)]
pub struct RegWindow {
    base: Reg,
    len: u8,
}

impl RegWindow {
    /// First register in the window (native result lands here for `make_msg`).
    pub fn base(self) -> Reg {
        self.base
    }

    /// Register at offset `i` within the window (`0 <= i < len`).
    pub fn at(self, i: u8) -> Reg {
        debug_assert!(i < self.len, "RegWindow index out of bounds");
        Reg(self.base.0.saturating_add(i))
    }
}

/// Assemble one bytecode module.
pub struct Program {
    inner: ChunkBuilder,
}

impl Program {
    /// Start a new module named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            inner: ChunkBuilder::new(name),
        }
    }

    /// Define a function and run `body` against a fresh [`Fn`] context.
    ///
    /// Returns the function index for [`Fn::spawn`] / [`Fn::call`].
    pub fn function(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        body: impl FnOnce(&mut Fn<'_>),
    ) -> FuncId {
        let mut f = Fn::open(&mut self.inner, name, arity);
        let id = f.function;
        body(&mut f);
        id
    }

    /// Define a function with an explicit register-file size.
    ///
    /// Useful for fixed register layouts (`reg(255)`) or verification demos
    /// where `num_registers < arity` must fail static checks.
    pub fn function_raw(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        num_registers: u8,
        body: impl FnOnce(&mut Fn<'_>),
    ) -> FuncId {
        let mut f = Fn::open_with(&mut self.inner, name, arity, num_registers);
        let id = f.function;
        body(&mut f);
        id
    }

    /// Look up a function index by name (available before [`Self::build`]).
    pub fn function_index(&self, name: &str) -> Option<FuncId> {
        self.inner.function_index(name)
    }

    /// Finish assembly and produce an immutable [`Chunk`].
    pub fn build(self) -> Chunk {
        self.inner.finish()
    }
}

/// Emit instructions for one bytecode function.
pub struct Fn<'a> {
    b: &'a mut ChunkBuilder,
    function: FuncId,
    next_reg: u8,
    scratch: Option<Reg>,
}

impl<'a> Fn<'a> {
    fn open(b: &'a mut ChunkBuilder, name: impl Into<String>, arity: u8) -> Self {
        Self::open_with(b, name, arity, arity.max(4))
    }

    fn open_with(
        b: &'a mut ChunkBuilder,
        name: impl Into<String>,
        arity: u8,
        num_registers: u8,
    ) -> Self {
        let function = b.begin_function(name, arity, num_registers);
        Self {
            b,
            function,
            next_reg: num_registers,
            scratch: None,
        }
    }

    /// Allocate a fresh local register.
    pub fn local(&mut self) -> Reg {
        let reg = Reg(self.next_reg);
        self.next_reg = self.next_reg.saturating_add(1);
        self.b.set_num_registers(self.function, self.next_reg);
        reg
    }

    /// Bind to an explicit register index without growing the declared frame.
    pub fn reg(&mut self, index: u8) -> Reg {
        Reg(index)
    }

    /// Ensure the register file is at least `count` slots wide.
    pub fn reserve(&mut self, count: u8) {
        if count > self.next_reg {
            self.next_reg = count;
            self.b.set_num_registers(self.function, self.next_reg);
        }
    }

    /// Allocate `count` contiguous locals; useful before [`Self::native_n`].
    pub fn window(&mut self, count: u8) -> RegWindow {
        let base_idx = self.next_reg;
        for _ in 0..count {
            let _ = self.local();
        }
        RegWindow {
            base: Reg(base_idx),
            len: count,
        }
    }

    /// Load a small immediate into a new local.
    pub fn load_i32(&mut self, n: i32) -> Reg {
        let reg = self.local();
        self.b.emit_load_imm(reg.0, n);
        reg
    }

    /// Load a constant-pool integer into a new local.
    pub fn load_int(&mut self, n: i64) -> Reg {
        let konst = self.b.const_(Value::Int(n));
        let reg = self.local();
        self.b.emit_load_const(reg.0, konst);
        reg
    }

    /// Store an immediate into an existing register.
    pub fn set(&mut self, reg: Reg, imm: i32) {
        self.b.emit_load_imm(reg.0, imm);
    }

    /// `dst = src`
    pub fn mov(&mut self, dst: Reg, src: Reg) {
        self.b.emit_move(dst.0, src.0);
    }

    fn binop(&mut self, op: Opcode, lhs: Reg, rhs: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_binop(op, dst.0, lhs.0, rhs.0);
        dst
    }

    fn binop_imm(&mut self, op: Opcode, lhs: Reg, imm: i32) -> Reg {
        let rhs = self.temp();
        self.b.emit_load_imm(rhs.0, imm);
        self.binop(op, lhs, rhs)
    }

    /// `dst = lhs + rhs` into a new local.
    pub fn add(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Add, lhs, rhs)
    }

    /// `dst += imm` in place.
    pub fn add_imm(&mut self, dst: Reg, imm: i32) {
        let tmp = self.temp();
        self.b.emit_load_imm(tmp.0, imm);
        self.b.emit_binop(Opcode::Add, dst.0, dst.0, tmp.0);
    }

    pub fn sub(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Sub, lhs, rhs)
    }

    pub fn mul(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Mul, lhs, rhs)
    }

    pub fn div(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Div, lhs, rhs)
    }

    pub fn modulo(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Mod, lhs, rhs)
    }

    pub fn neg(&mut self, src: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_neg(dst.0, src.0);
        dst
    }

    pub fn eq(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Eq, lhs, rhs)
    }

    pub fn eq_imm(&mut self, lhs: Reg, imm: i32) -> Reg {
        self.binop_imm(Opcode::Eq, lhs, imm)
    }

    pub fn lt(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Lt, lhs, rhs)
    }

    pub fn le(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Le, lhs, rhs)
    }

    /// While `counter < limit` (interpreter `Branch` skips on falsy).
    pub fn while_lt<F>(&mut self, counter: Reg, limit: Reg, body: F)
    where
        F: FnOnce(&mut Fn<'_>),
    {
        let head = self.b.new_label();
        let done = self.b.new_label();
        let cond = self.local();
        self.b.bind_label(head);
        self.b.emit_binop(Opcode::Lt, cond.0, counter.0, limit.0);
        self.b.emit_branch(cond.0, done);
        body(self);
        self.b.emit_jump(head);
        self.b.bind_label(done);
    }

    pub fn label(&mut self) -> Label {
        self.b.new_label()
    }

    pub fn bind(&mut self, label: Label) {
        self.b.bind_label(label);
    }

    pub fn jump(&mut self, label: Label) {
        self.b.emit_jump(label);
    }

    /// Branch when `cond` is falsy (`0`).
    pub fn branch_if_falsy(&mut self, cond: Reg, target: Label) {
        self.b.emit_branch(cond.0, target);
    }

    pub fn return_(&mut self, value: Reg) {
        self.b.emit_return(value.0);
    }

    pub fn call(&mut self, function: FuncId, argc: u8) -> Reg {
        let dst = self.local();
        self.b.emit_call(dst.0, function, argc);
        dst
    }

    pub fn halt(&mut self) {
        self.b.emit_halt();
    }

    pub fn yield_(&mut self) {
        self.b.emit_yield();
    }

    pub fn sleep(&mut self, millis: Reg) {
        self.b.emit_sleep(millis.0);
    }

    pub fn exit(&mut self, reg: Reg) {
        self.b.emit_exit(reg.0);
    }

    /// Self Cap (`SEND` / `ASK` target for this flow).
    pub fn self_cap(&mut self) -> Reg {
        let cap = self.local();
        self.b.emit_self_pid(cap.0);
        cap
    }

    pub fn spawn(&mut self, function: FuncId, argc: u8) -> Reg {
        let cap = self.local();
        self.spawn_at(cap, function, argc);
        cap
    }

    /// Spawn into an explicit destination register (e.g. `reg(255)`).
    pub fn spawn_at(&mut self, dst: Reg, function: FuncId, argc: u8) {
        self.b.emit_spawn(dst.0, function, argc);
    }

    pub fn send(&mut self, target_cap: Reg, msg: Reg) {
        self.b.emit_send(target_cap.0, msg.0);
    }

    pub fn receive(&mut self) -> Reg {
        let msg = self.local();
        self.b.emit_receive(msg.0);
        msg
    }

    pub fn receive_timeout(&mut self, millis: Reg) -> Reg {
        let msg = self.local();
        self.b.emit_receive_timeout(msg.0, millis.0);
        msg
    }

    pub fn receive_match(&mut self, tag: Reg) -> Reg {
        let msg = self.local();
        self.b.emit_receive_match(msg.0, tag.0);
        msg
    }

    pub fn receive_match_imm(&mut self, tag: u16) -> Reg {
        let msg = self.local();
        self.b.emit_receive_match_imm(msg.0, tag);
        msg
    }

    /// RPC hop: deliver `msg` to `target_cap`, wait for correlated reply.
    pub fn ask(&mut self, target_cap: Reg, msg: Reg) -> Reg {
        let reply = self.local();
        self.b.emit_ask(reply.0, target_cap.0, msg.0);
        reply
    }

    pub fn trap(&mut self, code: i32) {
        self.b.emit_trap(code);
    }

    /// Copy `src`, call a one-arg native, return the result in a new local.
    ///
    /// The source register is preserved (`CallNative` clobbers its argument slot).
    pub fn native1_from(&mut self, src: Reg, native: u32) -> Reg {
        let dst = self.local();
        self.b.emit_native1_from(dst.0, src.0, native);
        dst
    }

    /// Side-effect native on a copy of `src` (e.g. `print`).
    pub fn native1_on(&mut self, src: Reg, native: u32) {
        let tmp = self.local();
        self.b.emit_native1_from(tmp.0, src.0, native);
    }

    /// `CallNative` with `argc` args already at `base..base+argc`.
    pub fn native_n(&mut self, base: Reg, native: u32, argc: u8) {
        self.b.emit_native_n(base.0, native, argc);
    }

    /// Call a native with `argc` args already in `base..base+argc`.
    pub fn call_native(&mut self, base: Reg, native: u32, argc: u8) {
        self.native_n(base, native, argc);
    }

    /// Call a zero-arg native; returns the result register.
    pub fn call_native0(&mut self, native: u32) -> Reg {
        let dst = self.local();
        self.b.emit_call_native(dst.0, native, 0);
        dst
    }

    /// Build a [`Value::Message`] via `make_msg` at `native_index`.
    ///
    /// Index `2` in [`crate::std_native_map`]. Args are packed into a
    /// contiguous four-register window; returns the message register.
    pub fn make_msg(
        &mut self,
        native_index: u32,
        sender: Reg,
        request_id: Reg,
        tag: i32,
        payload: Reg,
    ) -> Reg {
        let w = self.window(4);
        self.mov(w.at(0), sender);
        self.mov(w.at(1), request_id);
        self.set(w.at(2), tag);
        self.mov(w.at(3), payload);
        self.native_n(w.base(), native_index, 4);
        w.base()
    }

    fn temp(&mut self) -> Reg {
        if let Some(scratch) = self.scratch {
            return scratch;
        }
        let scratch = self.local();
        self.scratch = Some(scratch);
        scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NativeTable, Value, Vm, VmResult};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn count_to(n: i64) -> Chunk {
        let mut program = Program::new("count");
        program.function("main", 0, |f| {
            let limit = f.load_int(n);
            let counter = f.load_i32(0);
            f.while_lt(counter, limit, |f| f.add_imm(counter, 1));
            f.return_(counter);
        });
        program.build()
    }

    #[test]
    fn function_raw_keeps_declared_register_file_for_verify() -> TestResult {
        let mut program = Program::new("malformed");
        program.function_raw("main", 3, 1, |f| {
            let r0 = f.reg(0);
            f.return_(r0);
        });
        let chunk = program.build();
        assert_eq!(chunk.functions[0].arity, 3);
        assert_eq!(chunk.functions[0].num_registers, 1);
        assert!(matches!(
            crate::verify(&chunk),
            Err(crate::VerifyError::ArityExceedsRegisters {
                function: 0,
                arity: 3,
                num_registers: 1,
            })
        ));
        Ok(())
    }

    #[test]
    fn count_loop_returns_n() -> TestResult {
        let chunk = count_to(100);
        let mut vm = Vm::new(std::sync::Arc::new(chunk), NativeTable::empty(), 0, &[])?;
        assert!(matches!(
            vm.run(10_000),
            VmResult::Complete(Value::Int(100))
        ));
        Ok(())
    }

    #[test]
    fn ping_pong_shape() -> TestResult {
        let chunk = crate::samples::ping_pong();
        crate::verify(&chunk)?;
        assert!(chunk.functions.iter().any(|f| f.name == "main"));
        assert!(chunk.functions.iter().any(|f| f.name == "pong"));
        Ok(())
    }
}
