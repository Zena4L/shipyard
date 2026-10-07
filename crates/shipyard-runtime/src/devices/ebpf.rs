//! eBPF, the little that a device program uses (the kernel's
//! `Documentation/bpf/standardization/instruction-set.rst`).
//!
//! The machine has eleven 64-bit registers. `r1` holds the address of the
//! program's input (its context) when it starts; `r0` holds the verdict
//! when it ends (`exit`). An instruction is 8 bytes: an opcode, the
//! destination and source registers (4 bits each), a signed 16-bit offset,
//! and a signed 32-bit immediate. A jump's offset counts instructions from
//! the next one.
//!
//! The kernel's verifier walks every path before loading a program: each
//! register must be written before it's read, every memory access must be
//! one the program type allows, and every path must end. A device program
//! never loops, so this assembler only jumps forward.
//!
//! [`Assembler`] writes programs with labels and resolves the offsets.

/// One instruction: the kernel's `struct bpf_insn`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Instruction {
    /// What it does.
    pub(crate) code: u8,
    /// The destination register (low 4 bits) and the source (high 4).
    pub(crate) registers: u8,
    /// A jump's distance, or a memory access's offset.
    pub(crate) offset: i16,
    /// The constant operand.
    pub(crate) immediate: i32,
}

/// A register: `r0` to `r10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reg(pub(crate) u8);

/// `r0`: the verdict.
pub(crate) const R0: Reg = Reg(0);
/// `r1`: the context's address at the start.
pub(crate) const R1: Reg = Reg(1);

// Opcodes: the class (low 3 bits), the operation, and whether the operand
// is the immediate (`K`) or a register (`X`). The 32-bit ALU class zeroes a
// result's upper half, so every value here stays a plain `u32`.

/// `BPF_LDX | BPF_MEM | BPF_W`: dst = the 32-bit word at src + offset.
pub(crate) const LOAD_WORD: u8 = 0x61;
/// `BPF_ALU | BPF_MOV | BPF_K`: dst = immediate (32-bit).
pub(crate) const MOV: u8 = 0xb4;
/// `BPF_ALU | BPF_MOV | BPF_X`: dst = src (32-bit).
pub(crate) const MOV_REG: u8 = 0xbc;
/// `BPF_ALU | BPF_AND | BPF_K`: dst &= immediate.
pub(crate) const AND: u8 = 0x54;
/// `BPF_ALU | BPF_RSH | BPF_K`: dst >>= immediate.
pub(crate) const RSH: u8 = 0x74;
/// `BPF_JMP | BPF_JEQ | BPF_K`: jump if dst == immediate.
pub(crate) const JUMP_EQ: u8 = 0x15;
/// `BPF_JMP | BPF_JNE | BPF_K`: jump if dst != immediate.
pub(crate) const JUMP_NE: u8 = 0x55;
/// `BPF_JMP | BPF_JA`: jump.
pub(crate) const GOTO: u8 = 0x05;
/// `BPF_JMP | BPF_EXIT`: end, with `r0`.
pub(crate) const EXIT: u8 = 0x95;

/// The most instructions a program has here: far more than the most rules
/// make (a few each), and far less than the kernel takes.
pub(crate) const MAX_INSTRUCTIONS: usize = 1 << 14;

/// A place in the program, bound with [`Assembler::bind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Label(usize);

/// A conditional jump's test, against a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Test {
    /// dst == k.
    Equal,
    /// dst != k.
    NotEqual,
}

/// Writes a program with labels; [`Assembler::assemble`] resolves them.
#[derive(Debug, Default)]
pub(crate) struct Assembler {
    program: Vec<Instruction>,
    /// Each label's place, once bound.
    bound: Vec<Option<usize>>,
    /// Jumps to patch: the instruction's index, and where it goes.
    jumps: Vec<(usize, Label)>,
}

impl Assembler {
    /// A new label, to bind later (only forward jumps exist).
    pub(crate) fn label(&mut self) -> Label {
        self.bound.push(None);
        Label(self.bound.len() - 1)
    }

    /// Puts `label` here: jumps to it go to the next instruction.
    pub(crate) fn bind(&mut self, label: Label) {
        self.bound[label.0] = Some(self.program.len());
    }

    /// dst = the 32-bit word at `offset` past the address in `src`.
    pub(crate) fn load_word(&mut self, dst: Reg, src: Reg, offset: i16) {
        self.push(LOAD_WORD, dst, src, offset, 0);
    }

    /// dst = `value`.
    pub(crate) fn mov(&mut self, dst: Reg, value: u32) {
        self.push(MOV, dst, Reg(0), 0, value.cast_signed());
    }

    /// dst = src.
    pub(crate) fn mov_reg(&mut self, dst: Reg, src: Reg) {
        self.push(MOV_REG, dst, src, 0, 0);
    }

    /// dst &= `mask`.
    pub(crate) fn and(&mut self, dst: Reg, mask: u32) {
        self.push(AND, dst, Reg(0), 0, mask.cast_signed());
    }

    /// dst >>= `bits`.
    pub(crate) fn rsh(&mut self, dst: Reg, bits: u32) {
        self.push(RSH, dst, Reg(0), 0, bits.cast_signed());
    }

    /// Goes to `to` if `dst` passes `test` against `k`; on otherwise.
    pub(crate) fn jump_if(&mut self, test: Test, dst: Reg, k: u32, to: Label) {
        let code = match test {
            Test::Equal => JUMP_EQ,
            Test::NotEqual => JUMP_NE,
        };
        self.jumps.push((self.program.len(), to));
        self.push(code, dst, Reg(0), 0, k.cast_signed());
    }

    /// Goes to `to`.
    pub(crate) fn goto(&mut self, to: Label) {
        self.jumps.push((self.program.len(), to));
        self.push(GOTO, Reg(0), Reg(0), 0, 0);
    }

    /// Ends, with the verdict `value`.
    pub(crate) fn ret(&mut self, value: u32) {
        self.mov(R0, value);
        self.push(EXIT, Reg(0), Reg(0), 0, 0);
    }

    fn push(&mut self, code: u8, dst: Reg, src: Reg, offset: i16, immediate: i32) {
        self.program.push(Instruction {
            code,
            registers: (src.0 << 4) | (dst.0 & 0x0f),
            offset,
            immediate,
        });
    }

    /// The program, with every jump's offset worked out.
    ///
    /// # Errors
    ///
    /// The program is longer than [`MAX_INSTRUCTIONS`] (then too, a jump
    /// might not fit its 16 bits).
    ///
    /// # Panics
    ///
    /// A label jumped to was never bound, or bound before the jump: a bug
    /// in the generator, never the input's doing.
    pub(crate) fn assemble(mut self) -> Result<Vec<Instruction>, usize> {
        if self.program.len() > MAX_INSTRUCTIONS {
            return Err(self.program.len());
        }
        for (at, label) in self.jumps {
            let target =
                self.bound[label.0].unwrap_or_else(|| unreachable!("label {label:?} never bound"));
            assert!(target > at, "a jump that isn't forward");
            // Within MAX_INSTRUCTIONS, the distance fits 16 bits.
            self.program[at].offset = i16::try_from(target - at - 1)
                .unwrap_or_else(|_| unreachable!("a jump past 16 bits"));
        }
        Ok(self.program)
    }
}

/// Runs `program` as the kernel would, on a context of 32-bit words
/// (`context[i]` at offset `4 * i`), for tests: the verdict. Panics on what
/// the verifier would refuse: an instruction this module doesn't make, a
/// register read before it's written, a jump backwards or past the end, a
/// load outside the context.
#[cfg(test)]
pub(crate) fn run(program: &[Instruction], context: &[u32]) -> u64 {
    // The context's "address" in r1: loads through r1 read `context`.
    const CONTEXT: u64 = 0xc0_0000;
    let mut registers: [Option<u64>; 11] = [None; 11];
    registers[1] = Some(CONTEXT);
    let mut pc = 0usize;
    loop {
        let instruction = program[pc];
        let dst = usize::from(instruction.registers & 0x0f);
        let src = usize::from(instruction.registers >> 4);
        let read = |registers: &[Option<u64>; 11], at: usize| {
            registers[at].unwrap_or_else(|| panic!("r{at} read before it's written, at {pc}"))
        };
        let k = u64::from(instruction.immediate.cast_unsigned());
        let mut next = pc + 1;
        let jump = |taken: bool| {
            let offset = usize::try_from(instruction.offset).expect("a jump backwards");
            if taken { pc + 1 + offset } else { pc + 1 }
        };
        match instruction.code {
            LOAD_WORD => {
                assert_eq!(
                    read(&registers, src),
                    CONTEXT,
                    "a load not from the context"
                );
                let at = usize::try_from(instruction.offset).unwrap();
                assert!(at % 4 == 0 && at / 4 < context.len(), "a load at {at}");
                registers[dst] = Some(u64::from(context[at / 4]));
            }
            MOV => registers[dst] = Some(k),
            MOV_REG => registers[dst] = Some(read(&registers, src) & 0xffff_ffff),
            AND => registers[dst] = Some(read(&registers, dst) & k),
            RSH => registers[dst] = Some((read(&registers, dst) & 0xffff_ffff) >> k),
            JUMP_EQ => next = jump(read(&registers, dst) == k),
            JUMP_NE => next = jump(read(&registers, dst) != k),
            GOTO => next = jump(true),
            EXIT => return read(&registers, 0),
            code => panic!("an instruction this module doesn't make: {code:#x}"),
        }
        assert!(next < program.len(), "ran off the end");
        pc = next;
    }
}

/// Whether every instruction of `program` can be reached from the first,
/// as the verifier requires ("unreachable insn"), for tests.
#[cfg(test)]
pub(crate) fn all_reachable(program: &[Instruction]) -> bool {
    let mut reached = vec![false; program.len()];
    let mut next = vec![0usize];
    while let Some(at) = next.pop() {
        if at >= program.len() || reached[at] {
            continue;
        }
        reached[at] = true;
        let instruction = program[at];
        let target = at + 1 + usize::try_from(instruction.offset).unwrap_or(0);
        match instruction.code {
            EXIT => {}
            GOTO => next.push(target),
            JUMP_EQ | JUMP_NE => next.extend([at + 1, target]),
            _ => next.push(at + 1),
        }
    }
    reached.into_iter().all(|reached| reached)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_are_the_kernels_eight_bytes() {
        assert_eq!(std::mem::size_of::<Instruction>(), 8);
        let mut asm = Assembler::default();
        asm.load_word(Reg(2), R1, 4);
        let program = asm.assemble().unwrap();
        // `ldxw r2, [r1+4]`: the destination in the low nibble.
        assert_eq!(
            program[0],
            Instruction {
                code: 0x61,
                registers: 0x12,
                offset: 4,
                immediate: 0
            }
        );
    }

    #[test]
    fn labels_resolve_to_offsets() {
        let mut asm = Assembler::default();
        let yes = asm.label();
        asm.load_word(Reg(2), R1, 0);
        asm.jump_if(Test::Equal, Reg(2), 7, yes);
        asm.ret(1);
        asm.bind(yes);
        asm.ret(2);
        let program = asm.assemble().unwrap();
        assert_eq!(program[1].offset, 2, "over the `mov` and `exit`");
        assert_eq!(run(&program, &[7]), 2);
        assert_eq!(run(&program, &[8]), 1);
    }

    #[test]
    fn each_test_and_operation() {
        let mut asm = Assembler::default();
        let (set, out) = (asm.label(), asm.label());
        asm.load_word(Reg(2), R1, 0);
        asm.mov_reg(Reg(3), Reg(2));
        asm.and(Reg(3), 0xffff);
        asm.rsh(Reg(2), 16);
        asm.and(Reg(2), 4);
        asm.jump_if(Test::NotEqual, Reg(2), 0, set);
        asm.jump_if(Test::NotEqual, Reg(3), 9, out);
        asm.ret(9);
        asm.bind(set);
        asm.ret(4);
        asm.bind(out);
        let end = asm.label();
        asm.goto(end);
        asm.ret(99);
        asm.bind(end);
        asm.ret(8);
        let program = asm.assemble().unwrap();
        assert_eq!(run(&program, &[(4 << 16) | 9]), 4, "bit 4 above the 16");
        assert_eq!(run(&program, &[(2 << 16) | 9]), 9);
        assert_eq!(run(&program, &[(2 << 16) | 8]), 8, "over the goto");
    }

    #[test]
    #[should_panic(expected = "read before it's written")]
    fn the_interpreter_is_as_strict_as_the_verifier() {
        let mut asm = Assembler::default();
        asm.mov_reg(Reg(3), Reg(4));
        asm.ret(0);
        run(&asm.assemble().unwrap(), &[0]);
    }

    #[test]
    fn too_long_a_program_is_refused() {
        let mut asm = Assembler::default();
        for _ in 0..=MAX_INSTRUCTIONS / 2 {
            asm.ret(0);
        }
        assert!(asm.assemble().is_err());
    }
}
