//! Programs of N NOPs, to separate per-call overhead from per-instruction cost.
//!
//! One length cannot tell them apart: a single measurement of "64 NOPs took X"
//! is consistent with a huge fixed cost and a free interpreter, or the reverse.
//! Two lengths give the slope, which is what the dispatch loop actually costs.

use std::fs;
use std::path::PathBuf;

use lumen_vm::isa::{Instruction, OpCode};
use lumen_vm::program::builder::ProgramBuilder;
use lumen_vm::program::Section;

fn nops(n: usize) -> Vec<u8> {
    let mut b = ProgramBuilder::new();
    for _ in 0..n {
        b.push(Section::Pixel, Instruction::new(OpCode::Nop, 0, 0, 0));
    }
    b.push(Section::Pixel, Instruction::new(OpCode::EmitRgb, 0, 0, 0));
    b.build()
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).expect("usage: dispatch <dir>"));
    fs::create_dir_all(&out).expect("create");
    for n in [16usize, 64, 256] {
        fs::write(out.join(format!("nop{n}.lvmb")), nops(n)).expect("write");
        println!("nop{n}");
    }
}
