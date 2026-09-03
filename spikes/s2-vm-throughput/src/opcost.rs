//! Per-opcode cost, measured rather than assumed.
//!
//! The frame measurement showed the budget model mis-ranking real effects by
//! nearly four times: an effect the compiler prices at 57 units per pixel runs
//! slower than one it prices at 141. The budget is what a device uses to promise
//! a frame rate before it runs anything, so weights that disagree with the chip
//! by that much make the promise decorative.
//!
//! Each program here runs one instruction 64 times. A baseline of 64 `NOP`s
//! measures the dispatch loop, so subtracting it leaves the operation. What
//! comes out is a nanosecond figure per opcode, which is what `OpCode::cost()`
//! should be proportional to and currently is not.

use esp_println::println;
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::{Machine, NoUniforms, PixelInputs};

/// Repetitions inside each program. Must match the generator.
const REPS: u64 = 64;

/// Iterations per measurement. The whole loop is small, so it needs to run a
/// lot to be worth more than the timer's resolution.
const ITERS: u64 = 2_000;

/// The fitted dispatch cost, so a per-opcode figure can be reported as the whole
/// instruction rather than as the part above dispatch. Measured by
/// `dispatch_cost` below, which prints it again each run: if the two ever
/// diverge, this constant is stale.
const DISPATCH_NS: u64 = 837;

/// `(name, the cost the compiler currently assigns, bytecode)`.
pub const PROGRAMS: &[(&str, u32, &[u8])] = &[
    ("Nop", 8, include_bytes!("../opcost/Nop.lvmb")),
    ("Mov", 10, include_bytes!("../opcost/Mov.lvmb")),
    ("Add", 11, include_bytes!("../opcost/Add.lvmb")),
    ("Mul", 12, include_bytes!("../opcost/Mul.lvmb")),
    ("Div", 22, include_bytes!("../opcost/Div.lvmb")),
    ("Madd", 13, include_bytes!("../opcost/Madd.lvmb")),
    ("Min", 11, include_bytes!("../opcost/Min.lvmb")),
    ("Floor", 10, include_bytes!("../opcost/Floor.lvmb")),
    ("Fract", 10, include_bytes!("../opcost/Fract.lvmb")),
    ("SinTurns", 15, include_bytes!("../opcost/SinTurns.lvmb")),
    ("Sin", 24, include_bytes!("../opcost/Sin.lvmb")),
    ("Atan2", 27, include_bytes!("../opcost/Atan2.lvmb")),
    ("Sqrt", 57, include_bytes!("../opcost/Sqrt.lvmb")),
    ("Pow", 25, include_bytes!("../opcost/Pow.lvmb")),
    ("Exp", 17, include_bytes!("../opcost/Exp.lvmb")),
    ("Log2", 17, include_bytes!("../opcost/Log2.lvmb")),
    ("Noise1", 13, include_bytes!("../opcost/Noise1.lvmb")),
    ("Noise2", 19, include_bytes!("../opcost/Noise2.lvmb")),
    ("Noise3", 29, include_bytes!("../opcost/Noise3.lvmb")),
    ("Step", 10, include_bytes!("../opcost/Step.lvmb")),
    ("SmoothStep", 25, include_bytes!("../opcost/SmoothStep.lvmb")),
    ("Lerp", 13, include_bytes!("../opcost/Lerp.lvmb")),
    ("Len2", 60, include_bytes!("../opcost/Len2.lvmb")),
    ("Len3", 60, include_bytes!("../opcost/Len3.lvmb")),
    ("Hsv2Rgb", 15, include_bytes!("../opcost/Hsv2Rgb.lvmb")),
    ("Rgb2Hsv", 38, include_bytes!("../opcost/Rgb2Hsv.lvmb")),
    ("Palette", 33, include_bytes!("../opcost/Palette.lvmb")),
    ("Temp2Rgb", 23, include_bytes!("../opcost/Temp2Rgb.lvmb")),
    ("ChRead", 9, include_bytes!("../opcost/ChRead.lvmb")),
    ("PrevRead", 10, include_bytes!("../opcost/PrevRead.lvmb")),
    ("LoadK", 12, include_bytes!("../opcost/LoadK.lvmb")),
    ("Sub", 11, include_bytes!("../opcost/Sub.lvmb")),
    ("Neg", 11, include_bytes!("../opcost/Neg.lvmb")),
    ("Abs", 11, include_bytes!("../opcost/Abs.lvmb")),
    ("Max", 11, include_bytes!("../opcost/Max.lvmb")),
    ("Clamp", 11, include_bytes!("../opcost/Clamp.lvmb")),
    ("Lt", 11, include_bytes!("../opcost/Lt.lvmb")),
    ("Gt", 11, include_bytes!("../opcost/Gt.lvmb")),
    ("Eq", 11, include_bytes!("../opcost/Eq.lvmb")),
    ("Select", 11, include_bytes!("../opcost/Select.lvmb")),
    ("Cos", 24, include_bytes!("../opcost/Cos.lvmb")),
    ("CosTurns", 15, include_bytes!("../opcost/CosTurns.lvmb")),
    ("Log", 17, include_bytes!("../opcost/Log.lvmb")),
    ("Dot3", 17, include_bytes!("../opcost/Dot3.lvmb")),
];

fn inputs() -> PixelInputs {
    PixelInputs {
        x: Q16::HALF,
        y: Q16::HALF,
        z: Q16::HALF,
        lx: Q16::HALF,
        ly: Q16::HALF,
        lz: Q16::HALF,
        index: Q16::from_int(7),
        count: Q16::from_int(300),
        u: Q16::HALF,
        uv_x: Q16::HALF,
        uv_y: Q16::HALF,
        prev: [Q16::HALF, Q16::HALF, Q16::HALF],
    }
}

/// Nanoseconds per execution of one program's pixel section.
fn time_ns(bytes: &[u8]) -> Option<u64> {
    let program = Program::parse(bytes).ok()?;
    let mut machine = Machine::new();
    let inputs = inputs();

    let start = esp_hal::time::now();
    for _ in 0..ITERS {
        machine.run_pixel(&program, &inputs, &mut NoUniforms).ok()?;
    }
    let us = (esp_hal::time::now() - start).to_micros();
    Some(us * 1000 / ITERS)
}

/// Separate the cost of calling `run_pixel` from the cost of dispatching one
/// instruction inside it.
///
/// A single length cannot tell them apart: "64 NOPs took 59 us" is equally
/// consistent with a large fixed cost and a free interpreter, or the reverse,
/// and the two lead to opposite conclusions about what to optimise. Two lengths
/// give the slope.
fn dispatch_cost() {
    const LENGTHS: [(u64, &[u8]); 3] = [
        (16, include_bytes!("../opcost/nop16.lvmb")),
        (64, include_bytes!("../opcost/nop64.lvmb")),
        (256, include_bytes!("../opcost/nop256.lvmb")),
    ];
    let mut points = [(0u64, 0u64); 3];
    for (i, (n, bytes)) in LENGTHS.iter().enumerate() {
        let ns = time_ns(bytes).unwrap_or(0);
        points[i] = (*n, ns);
        println!("   {n:>4} nops: {ns:>8} ns");
    }
    let (n_lo, t_lo) = points[0];
    let (n_hi, t_hi) = points[2];
    if n_hi > n_lo && t_hi > t_lo {
        let per_instruction_centi = ((t_hi - t_lo) * 100) / (n_hi - n_lo);
        // Extrapolate back to zero instructions for the per-call cost.
        let slope_ns = (t_hi - t_lo) / (n_hi - n_lo);
        let overhead = t_lo.saturating_sub(slope_ns * n_lo);
        println!(
            "   dispatch: {}.{:02} ns/instruction, {overhead} ns per run_pixel call",
            per_instruction_centi / 100,
            per_instruction_centi % 100
        );
        // 160 MHz, so one cycle is 6.25 ns.
        println!("   that is {} cycles per instruction at 160 MHz", per_instruction_centi / 625);
    }
}

pub fn run() {
    println!();
    println!("== per-opcode cost, {REPS} per program, {ITERS} runs each");
    println!("== dispatch, fitted over three program lengths");
    dispatch_cost();

    let Some(baseline) = time_ns(PROGRAMS[0].2) else {
        println!("baseline failed");
        return;
    };
    println!("== dispatch baseline: {} ns for {REPS} NOPs", baseline);
    println!(
        "{:<11} {:>6} {:>10} {:>11} {:>10}",
        "opcode", "says", "ns above", "ns total", "vs table"
    );

    for (name, claimed, bytes) in PROGRAMS {
        let Some(total) = time_ns(bytes) else {
            println!("{name:<11} FAULTED");
            continue;
        };
        // Per operation, with the dispatch loop taken out. `Nop` measures the
        // loop itself, so its own figure lands near zero by construction.
        let net = total.saturating_sub(baseline);
        let ns_hundredths = (net * 100) / REPS;

        // What the cost table would have to say for this opcode, if `Add` were
        // still worth 1 unit. That is the comparison that matters: the budget
        // is a set of ratios, not absolute times.
        // The table says `claimed` units and a unit is 100 ns, so a weight
        // that matches the chip predicts `claimed * 100` ns for the whole
        // instruction, dispatch included.
        let predicted_ns = *claimed as u64 * 100;
        let actual_ns = DISPATCH_NS + ns_hundredths / 100;
        let error_pct = if predicted_ns > 0 {
            (actual_ns * 100) / predicted_ns
        } else {
            0
        };
        println!(
            "{name:<11} {claimed:>6} {:>7}.{:02} {actual_ns:>11} {error_pct:>9}%",
            ns_hundredths / 100,
            ns_hundredths % 100,
        );
    }
    println!("== done");
}
