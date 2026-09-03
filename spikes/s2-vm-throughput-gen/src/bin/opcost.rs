//! Emit one tiny program per opcode, for measuring what each actually costs.
//!
//! Each program runs the same instruction `REPS` times in the pixel section. A
//! baseline of `REPS` NOPs measures the interpreter's dispatch loop, so
//! subtracting it leaves the cost of the operation itself.
//!
//! Operands are fixed registers rather than varied ones. A real effect has
//! dependencies between its instructions too, and spreading these across
//! registers would measure a pipeline this chip does not have.

use std::fs;
use std::path::PathBuf;

use lumen_vm::isa::{Instruction, OpCode};
use lumen_vm::program::builder::ProgramBuilder;
use lumen_vm::program::{Section, PALETTE_STOPS};
use lumen_vm::q16::Q16;

/// Enough repetitions that the per-frame overhead does not matter, few enough
/// that the program stays small.
const REPS: usize = 64;

/// Registers the measured instruction reads and writes.
const DST: u8 = 20;
const A: u8 = 16;
const B: u8 = 17;

fn program_for(op: OpCode) -> Vec<u8> {
    let mut b = ProgramBuilder::new();

    // Two operands with values that are safe for every op under test: no
    // division by zero, no log of zero, no huge exponent.
    let half = b.constant(Q16::HALF);
    let two = b.constant(Q16::from_int(2));
    b.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, A, half));
    b.push(Section::Pixel, Instruction::with_imm(OpCode::LoadK, B, two));

    // A palette and a channel, so the ops that need one have one.
    let stops: [(Q16, Q16, Q16); PALETTE_STOPS] =
        core::array::from_fn(|i| {
            let t = Q16::from_ratio(i as i32, (PALETTE_STOPS - 1) as i32);
            (t, Q16::HALF, Q16::ONE)
        });
    let palette = b.palette(&stops);
    let channel = b.channel(0);

    for _ in 0..REPS {
        let ins = match op {
            // Three consecutive registers for the run-shaped ops.
            OpCode::Noise3 | OpCode::Len3 | OpCode::Hsv2Rgb | OpCode::Rgb2Hsv => {
                Instruction::new(op, DST, A, 0)
            }
            OpCode::Palette => Instruction::new(op, DST, A, palette),
            OpCode::ChRead => Instruction::new(op, DST, channel, 0),
            OpCode::PrevRead => Instruction::new(op, DST, 0, 0),
            OpCode::LoadK => Instruction::with_imm(op, DST, half),
            _ => Instruction::new(op, DST, A, B),
        };
        b.push(Section::Pixel, ins);
    }
    b.push(Section::Pixel, Instruction::new(OpCode::EmitRgb, A, A, A));
    b.build()
}

fn main() {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: opcost <output directory>"),
    );
    fs::create_dir_all(&out).expect("create output directory");

    // Chosen to span what the cost table claims: arithmetic it calls cheap,
    // table-driven transcendentals it calls dear, and the colour and history
    // ops whose weights nobody has ever checked against a chip.
    let ops = [
        OpCode::Nop,
        OpCode::Mov,
        OpCode::Add,
        OpCode::Mul,
        OpCode::Div,
        OpCode::Madd,
        OpCode::Min,
        OpCode::Floor,
        OpCode::Fract,
        OpCode::SinTurns,
        OpCode::Sin,
        OpCode::Atan2,
        OpCode::Sqrt,
        OpCode::Pow,
        OpCode::Exp,
        OpCode::Log2,
        OpCode::Noise1,
        OpCode::Noise2,
        OpCode::Noise3,
        OpCode::Step,
        OpCode::SmoothStep,
        OpCode::Lerp,
        OpCode::Len2,
        OpCode::Len3,
        OpCode::Hsv2Rgb,
        OpCode::Rgb2Hsv,
        OpCode::Palette,
        OpCode::Temp2Rgb,
        OpCode::ChRead,
        OpCode::PrevRead,
        OpCode::LoadK,
        OpCode::Sub,
        OpCode::Neg,
        OpCode::Abs,
        OpCode::Max,
        OpCode::Clamp,
        OpCode::Lt,
        OpCode::Gt,
        OpCode::Eq,
        OpCode::Select,
        OpCode::Cos,
        OpCode::CosTurns,
        OpCode::Log,
        OpCode::Dot3,
    ];

    let mut manifest = String::new();
    for op in ops {
        let name = format!("{op:?}");
        let bytes = program_for(op);
        fs::write(out.join(format!("{name}.lvmb")), &bytes).expect("write");
        manifest.push_str(&format!("{name} {} {}\n", op.cost(), bytes.len()));
    }
    fs::write(out.join("manifest.txt"), &manifest).expect("write manifest");
    print!("{manifest}");
    eprintln!("{} programs, {REPS} repetitions each", ops.len());
}
