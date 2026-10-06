//! Classic BPF, the little that a seccomp filter uses (`bpf(4)` on the BSDs
//! documents the machine; `Documentation/networking/filter.rst` in Linux).
//!
//! The machine has one 32-bit register, the accumulator. A filter loads
//! 32-bit words of `struct seccomp_data` into it, masks and compares them,
//! and jumps **forward only**, so every filter ends: no loops. A conditional
//! jump says how many instructions to skip if true and if false, in 8 bits
//! each (at most 255); an unconditional one (`ja`) has 32 bits. A filter ends
//! with `ret`, whose value is the verdict.
//!
//! [`Assembler`] writes programs with labels and resolves the offsets,
//! routing a conditional jump that's too far through a `ja`.

use super::SeccompError;

/// One instruction: the kernel's `struct sock_filter`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction {
    /// What it does (`BPF_LD | BPF_W | BPF_ABS`, …).
    pub code: u16,
    /// Instructions to skip if a conditional jump is true.
    pub jt: u8,
    /// Instructions to skip if it's false.
    pub jf: u8,
    /// The operand.
    pub k: u32,
}

/// `BPF_LD | BPF_W | BPF_ABS`: load the 32-bit word at offset `k` of the
/// input.
pub(super) const LOAD: u16 = 0x20;
/// `BPF_ALU | BPF_AND | BPF_K`: accumulator &= `k`.
pub(super) const AND: u16 = 0x54;
/// `BPF_JMP | BPF_JA`: skip `k` instructions.
pub(super) const GOTO: u16 = 0x05;
/// `BPF_JMP | BPF_JEQ | BPF_K`: accumulator == `k`?
pub(super) const JUMP_EQ: u16 = 0x15;
/// `BPF_JMP | BPF_JGT | BPF_K`: accumulator > `k`?
pub(super) const JUMP_GT: u16 = 0x25;
/// `BPF_JMP | BPF_JGE | BPF_K`: accumulator >= `k`?
pub(super) const JUMP_GE: u16 = 0x35;
/// `BPF_RET | BPF_K`: the verdict is `k`.
pub(super) const RETURN: u16 = 0x06;

/// The most instructions the kernel takes in one filter (`BPF_MAXINSNS`).
pub(super) const MAX_INSTRUCTIONS: usize = 4096;

/// A conditional jump's test, against a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Test {
    Equal,
    Greater,
    GreaterOrEqual,
}

/// A place in the program, bound with [`Assembler::bind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Label(usize);

#[derive(Debug)]
enum Op {
    Load(u32),
    And(u32),
    Jump {
        test: Test,
        k: u32,
        then: Label,
        otherwise: Label,
    },
    Goto(Label),
    Return(u32),
    Bind(Label),
}

/// Writes a program with labels; [`Assembler::assemble`] resolves them.
#[derive(Debug, Default)]
pub(super) struct Assembler {
    ops: Vec<Op>,
    labels: usize,
}

impl Assembler {
    /// A new label, to bind later (only forward jumps exist).
    pub(super) fn label(&mut self) -> Label {
        self.labels += 1;
        Label(self.labels - 1)
    }

    /// Puts `label` here: jumps to it go to the next instruction.
    pub(super) fn bind(&mut self, label: Label) {
        self.ops.push(Op::Bind(label));
    }

    /// Loads the 32-bit word at `offset` of `seccomp_data`.
    pub(super) fn load(&mut self, offset: u32) {
        self.ops.push(Op::Load(offset));
    }

    /// accumulator &= `mask`.
    pub(super) fn and(&mut self, mask: u32) {
        self.ops.push(Op::And(mask));
    }

    /// Goes to `then` if the accumulator passes `test` against `k`, to
    /// `otherwise` if not.
    pub(super) fn jump(&mut self, test: Test, k: u32, then: Label, otherwise: Label) {
        self.ops.push(Op::Jump {
            test,
            k,
            then,
            otherwise,
        });
    }

    /// Goes to `label`.
    pub(super) fn goto(&mut self, label: Label) {
        self.ops.push(Op::Goto(label));
    }

    /// Ends with the verdict `value`.
    pub(super) fn ret(&mut self, value: u32) {
        self.ops.push(Op::Return(value));
    }

    /// The program, with every jump's offset worked out.
    ///
    /// Jumps only go forward, so it's built from the end: when a jump is
    /// reached, everything it can jump to is already placed, and its
    /// distance is known. A conditional jump further than 8 bits reaches
    /// goes to a `ja` placed right after it instead.
    ///
    /// # Errors
    ///
    /// The program is longer than the kernel takes.
    pub(super) fn assemble(self) -> Result<Vec<Instruction>, SeccompError> {
        // Distances are counted from the end: `bound[label]` is how many
        // instructions follow the label, and `placed` how many follow the
        // instruction being placed now. A jump from there to the label
        // skips `placed - bound[label]` instructions.
        let mut bound: Vec<Option<usize>> = vec![None; self.labels];
        let mut reversed: Vec<Instruction> = Vec::with_capacity(self.ops.len());
        let at = |bound: &[Option<usize>], label: Label| -> usize {
            bound[label.0].unwrap_or_else(|| unreachable!("label {label:?} not bound after a jump"))
        };
        for op in self.ops.into_iter().rev() {
            match op {
                Op::Bind(label) => bound[label.0] = Some(reversed.len()),
                Op::Load(offset) => reversed.push(instruction(LOAD, offset)),
                Op::And(mask) => reversed.push(instruction(AND, mask)),
                Op::Return(value) => reversed.push(instruction(RETURN, value)),
                Op::Goto(label) => {
                    let skip = reversed.len() - at(&bound, label);
                    reversed.push(instruction(GOTO, skip_u32(skip)?));
                }
                Op::Jump {
                    test,
                    k,
                    then,
                    otherwise,
                } => {
                    // Where the jump lands for `label`, counted like `bound`:
                    // the label itself if it's near enough (with room for
                    // the two `ja`s this jump may need), or a `ja` to it.
                    let target = |label: Label, reversed: &mut Vec<Instruction>| {
                        let distance = reversed.len() + 2 - at(&bound, label);
                        if u8::try_from(distance).is_ok() {
                            return Ok(at(&bound, label));
                        }
                        let skip = reversed.len() - at(&bound, label);
                        reversed.push(instruction(GOTO, skip_u32(skip)?));
                        Ok::<_, SeccompError>(reversed.len())
                    };
                    let then = target(then, &mut reversed)?;
                    let otherwise = target(otherwise, &mut reversed)?;
                    let skip = |to: usize| {
                        u8::try_from(reversed.len() - to)
                            .unwrap_or_else(|_| unreachable!("a jump past 255 without a `ja`"))
                    };
                    let code = match test {
                        Test::Equal => JUMP_EQ,
                        Test::Greater => JUMP_GT,
                        Test::GreaterOrEqual => JUMP_GE,
                    };
                    reversed.push(Instruction {
                        code,
                        jt: skip(then),
                        jf: skip(otherwise),
                        k,
                    });
                }
            }
        }
        if reversed.len() > MAX_INSTRUCTIONS {
            return Err(SeccompError::TooLong(reversed.len()));
        }
        reversed.reverse();
        Ok(reversed)
    }
}

/// An instruction that doesn't jump conditionally.
const fn instruction(code: u16, k: u32) -> Instruction {
    Instruction {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn skip_u32(skip: usize) -> Result<u32, SeccompError> {
    u32::try_from(skip).map_err(|_| SeccompError::TooLong(skip))
}

/// Runs `program` on `data` (a `struct seccomp_data`) as the kernel would,
/// for tests: the verdict. Panics on anything the kernel would refuse to
/// load (a jump past the end, a load outside `data`, no `ret` at the end).
#[cfg(test)]
pub(super) fn run(program: &[Instruction], data: &[u8; 64]) -> u32 {
    let mut accumulator = 0u32;
    let mut pc = 0;
    loop {
        let instruction = program[pc];
        pc += 1;
        let k = instruction.k;
        let mut skip = |taken: bool| {
            pc += usize::from(if taken {
                instruction.jt
            } else {
                instruction.jf
            });
        };
        match instruction.code {
            LOAD => {
                let at = usize::try_from(k).unwrap();
                assert!(at % 4 == 0 && at + 4 <= data.len(), "load at {at}");
                accumulator = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
            }
            AND => accumulator &= k,
            GOTO => pc += usize::try_from(k).unwrap(),
            JUMP_EQ => skip(accumulator == k),
            JUMP_GT => skip(accumulator > k),
            JUMP_GE => skip(accumulator >= k),
            RETURN => return k,
            code => panic!("unknown instruction {code:#x}"),
        }
        assert!(pc < program.len(), "ran off the end");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(word0: u32) -> [u8; 64] {
        let mut data = [0; 64];
        data[..4].copy_from_slice(&word0.to_le_bytes());
        data
    }

    #[test]
    fn labels_resolve_to_offsets() {
        let mut asm = Assembler::default();
        let (yes, no) = (asm.label(), asm.label());
        asm.load(0);
        asm.jump(Test::Equal, 7, yes, no);
        asm.bind(no);
        asm.ret(1);
        asm.bind(yes);
        asm.ret(2);
        let program = asm.assemble().unwrap();
        assert_eq!(
            program[1],
            Instruction {
                code: JUMP_EQ,
                jt: 1,
                jf: 0,
                k: 7
            }
        );
        assert_eq!(run(&program, &data(7)), 2);
        assert_eq!(run(&program, &data(8)), 1);
    }

    /// A target more than 255 instructions away is reached through a `ja`,
    /// either way the test goes.
    #[test]
    fn far_targets_go_through_a_goto() {
        for far_when_true in [true, false] {
            let mut asm = Assembler::default();
            let (near, far, filler) = (asm.label(), asm.label(), asm.label());
            asm.load(0);
            if far_when_true {
                asm.jump(Test::GreaterOrEqual, 10, far, near);
            } else {
                asm.jump(Test::GreaterOrEqual, 10, near, far);
            }
            asm.bind(near);
            asm.goto(filler);
            asm.bind(filler);
            for value in 0..300 {
                asm.ret(1000 + value);
            }
            asm.bind(far);
            asm.ret(42);
            let program = asm.assemble().unwrap();
            assert!(program.iter().any(|i| i.code == GOTO && i.k > 255));
            let (big, small) = (run(&program, &data(10)), run(&program, &data(9)));
            if far_when_true {
                assert_eq!((big, small), (42, 1000));
            } else {
                assert_eq!((big, small), (1000, 42));
            }
        }
    }

    /// Right at the edge: 255 instructions away is still a direct jump
    /// (or a `ja`, never a wrong offset), 256 isn't.
    #[test]
    fn jumps_at_the_edge_land_where_they_should() {
        for gap in 250..262 {
            let mut asm = Assembler::default();
            let (target, next) = (asm.label(), asm.label());
            asm.load(0);
            asm.jump(Test::Equal, 1, target, next);
            asm.bind(next);
            for _ in 0..gap {
                asm.ret(0);
            }
            asm.bind(target);
            asm.ret(7);
            let program = asm.assemble().unwrap();
            assert_eq!(run(&program, &data(1)), 7, "gap {gap}");
            assert_eq!(run(&program, &data(2)), 0, "gap {gap}");
        }
    }

    #[test]
    fn too_long_a_program_is_refused() {
        let mut asm = Assembler::default();
        for _ in 0..=MAX_INSTRUCTIONS {
            asm.ret(0);
        }
        assert_eq!(
            asm.assemble(),
            Err(SeccompError::TooLong(MAX_INSTRUCTIONS + 1))
        );
    }
}
