use std::collections::HashMap;

use crate::chunk::{Chunk, FunctionDef};
use crate::instruction::Instruction;
use crate::opcode::Opcode;
use crate::value::Value;

/// An unresolved jump target, patched to a relative offset once its address
/// is known (see [`ChunkBuilder::bind_label`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Label(u32);

/// Fluent assembler for [`Chunk`]s.
///
/// This exists because hand-computing relative jump offsets (design notes
/// §7 shows raw opcodes) is exactly the kind of bookkeeping that produces
/// off-by-one bytecode bugs that only show up as a wrong branch at runtime.
/// The builder defers that arithmetic: emit a `Jump`/`Branch` against a
/// [`Label`], bind the label once you know where it lands, and the builder
/// back-patches every use.
///
/// This is the *only* supported way to hand-author a `Chunk` in this crate;
/// a source-level compiler (design notes' long-term "Rust → bytecode" path)
/// would sit on top of this same API.
pub struct ChunkBuilder {
    name: String,
    constants: Vec<Value>,
    code: Vec<Instruction>,
    functions: Vec<FunctionDef>,
    next_label: u32,
    label_targets: HashMap<Label, u32>,
    /// (instruction index, label) pairs awaiting patch.
    pending_jumps: Vec<(usize, Label)>,
    fn_starts: HashMap<String, u32>,
}

impl ChunkBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        ChunkBuilder {
            name: name.into(),
            constants: Vec::new(),
            code: Vec::new(),
            functions: Vec::new(),
            next_label: 0,
            label_targets: HashMap::new(),
            pending_jumps: Vec::new(),
            fn_starts: HashMap::new(),
        }
    }

    pub fn const_(&mut self, v: Value) -> u32 {
        // Constant deduplication keeps hot small-int/bool literals from
        // bloating the pool across a large generated function.
        if let Some(pos) = self.constants.iter().position(|c| c == &v) {
            return pos as u32;
        }
        self.constants.push(v);
        (self.constants.len() - 1) as u32
    }

    pub fn new_label(&mut self) -> Label {
        let l = Label(self.next_label);
        self.next_label += 1;
        l
    }

    /// Bind `label` to the *next* instruction that will be emitted.
    pub fn bind_label(&mut self, label: Label) {
        self.label_targets.insert(label, self.code.len() as u32);
    }

    fn emit(&mut self, instr: Instruction) -> usize {
        self.code.push(instr);
        self.code.len() - 1
    }

    pub fn emit_halt(&mut self) {
        self.emit(Instruction::nullary(Opcode::Halt));
    }

    pub fn emit_load_const(&mut self, dst: u8, konst: u32) {
        self.emit(Instruction::new(Opcode::LoadConst, dst, 0, 0, konst as i32));
    }

    pub fn emit_load_imm(&mut self, dst: u8, imm: i32) {
        self.emit(Instruction::a_imm(Opcode::LoadImm, dst, imm));
    }

    pub fn emit_move(&mut self, dst: u8, src: u8) {
        self.emit(Instruction::abc(Opcode::Move, dst, src, 0));
    }

    pub fn emit_binop(&mut self, op: Opcode, dst: u8, lhs: u8, rhs: u8) {
        debug_assert!(matches!(
            op,
            Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div | Opcode::Mod
                | Opcode::Eq | Opcode::Lt | Opcode::Le
        ));
        self.emit(Instruction::abc(op, dst, lhs, rhs));
    }

    pub fn emit_neg(&mut self, dst: u8, src: u8) {
        self.emit(Instruction::abc(Opcode::Neg, dst, src, 0));
    }

    pub fn emit_jump(&mut self, target: Label) {
        let idx = self.emit(Instruction::only_imm(Opcode::Jump, 0));
        self.pending_jumps.push((idx, target));
    }

    pub fn emit_branch(&mut self, cond: u8, target: Label) {
        let idx = self.emit(Instruction::a_imm(Opcode::Branch, cond, 0));
        self.pending_jumps.push((idx, target));
    }

    pub fn emit_spawn(&mut self, dst: u8, function: u32, argc: u8) {
        self.emit(Instruction::new(Opcode::Spawn, dst, argc, 0, function as i32));
    }

    pub fn emit_yield(&mut self) {
        self.emit(Instruction::nullary(Opcode::Yield));
    }

    pub fn emit_sleep(&mut self, millis_reg: u8) {
        self.emit(Instruction::abc(Opcode::Sleep, millis_reg, 0, 0));
    }

    pub fn emit_exit(&mut self, reg: u8) {
        self.emit(Instruction::abc(Opcode::Exit, reg, 0, 0));
    }

    pub fn emit_self_pid(&mut self, dst: u8) {
        self.emit(Instruction::abc(Opcode::SelfPid, dst, 0, 0));
    }

    pub fn emit_send(&mut self, target_pid_reg: u8, msg_reg: u8) {
        self.emit(Instruction::abc(Opcode::Send, target_pid_reg, msg_reg, 0));
    }

    pub fn emit_receive(&mut self, dst: u8) {
        self.emit(Instruction::abc(Opcode::Receive, dst, 0, 0));
    }

    pub fn emit_receive_timeout(&mut self, dst: u8, millis_reg: u8) {
        self.emit(Instruction::abc(Opcode::ReceiveTimeout, dst, millis_reg, 0));
    }

    pub fn emit_trap(&mut self, code: i32) {
        self.emit(Instruction::only_imm(Opcode::Trap, code));
    }

    pub fn emit_call(&mut self, dst: u8, function: u32, argc: u8) {
        self.emit(Instruction::new(Opcode::Call, dst, argc, 0, function as i32));
    }

    /// Emit a call through the runtime's native (FFI) function table
    /// (design notes §30-31). `native_index` is resolved by name against a
    /// `byteflow_vm::NativeTable` at the call site — this crate has no
    /// knowledge of what natives exist, on purpose (see
    /// [`crate::verify::verify`]'s note on why `CallNative` targets aren't
    /// range-checked statically).
    pub fn emit_call_native(&mut self, dst: u8, native_index: u32, argc: u8) {
        self.emit(Instruction::new(Opcode::CallNative, dst, argc, 0, native_index as i32));
    }

    pub fn emit_return(&mut self, reg: u8) {
        self.emit(Instruction::abc(Opcode::Return, reg, 0, 0));
    }

    /// Mark the start of a bytecode function at the current position and
    /// register it in the function table under `name`. Returns the function
    /// index, usable with [`ChunkBuilder::emit_call`]/[`ChunkBuilder::emit_spawn`]
    /// even before the function's body is emitted (functions may call
    /// themselves or each other, forward or backward).
    pub fn begin_function(&mut self, name: impl Into<String>, arity: u8, num_registers: u8) -> u32 {
        let name = name.into();
        let entry = self.code.len() as u32;
        let idx = self.functions.len() as u32;
        self.functions.push(FunctionDef {
            name: name.clone(),
            entry,
            arity,
            num_registers,
        });
        self.fn_starts.insert(name, idx);
        idx
    }

    pub fn function_index(&self, name: &str) -> Option<u32> {
        self.fn_starts.get(name).copied()
    }

    /// Patch the register-file size of an already-`begin_function`'d
    /// function. Exists for assemblers whose register count is only known
    /// *after* emitting the body — `begin_function` must still be called
    /// first so `entry` captures the current code cursor.
    pub fn set_num_registers(&mut self, function_index: u32, num_registers: u8) {
        if let Some(def) = self.functions.get_mut(function_index as usize) {
            def.num_registers = num_registers;
        }
    }

    /// Resolve every pending jump against its bound label and produce the
    /// final immutable [`Chunk`]. Panics (a build-time bug, not a runtime
    /// fault) if a label was referenced but never bound.
    pub fn finish(mut self) -> Chunk {
        for (idx, label) in self.pending_jumps.drain(..) {
            let target = *self
                .label_targets
                .get(&label)
                .unwrap_or_else(|| panic!("byteflow-bytecode: unbound label {label:?} in chunk '{}'", self.name));
            // Relative offset from the instruction *after* this jump.
            let offset = target as i64 - (idx as i64 + 1);
            self.code[idx].imm = offset as i32;
        }
        Chunk {
            name: self.name,
            constants: self.constants,
            code: self.code,
            functions: self.functions,
        }
    }
}