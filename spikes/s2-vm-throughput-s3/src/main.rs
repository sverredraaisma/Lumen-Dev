//! Spike S2: does the bytecode VM hit 60 fps for 300 pixels on real hardware?
//!
//! The assumption the whole architecture rests on. Effects are pure functions of
//! position and time, compiled to portable bytecode and *interpreted on the
//! device*; if a mid-range chip cannot run a real effect over a real strip
//! inside a frame, the design does not work and everything above it is built on
//! sand.
//!
//! Throwaway, per the plan. It exists to produce one number.
//!
//! # What it measures
//!
//! Five effects from the shipped corpus, spanning the cost range — 17 to 141
//! budget units per pixel — compiled by the real compiler and run by the real
//! interpreter. Per frame: the `frame` section once, then the `pixel` section
//! 300 times with per-pixel inputs that change, so nothing can be hoisted or
//! cached in a way a real render could not.
//!
//! # Why an ESP32-C3 is the honest test
//!
//! The plan names an S3 for this spike. A C3 is the harder target: one core at
//! 160 MHz against the S3's two at 240, and RISC-V without the S3's extra
//! addressing modes. A pass here is a conservative result rather than an
//! optimistic one, which is the direction a spike should err.
//!
//! # Reading the output
//!
//! `us/frame` is what a device would spend rendering one frame of 300 pixels.
//! The budget at 60 fps is 16 667 us, and a device has to do more in a frame
//! than render — receive, sync, drive the strip — so the interesting figure is
//! the fraction, not the pass.
//!
//! `budget` is the compiler's own figure for the effect, in units of 100 ns
//! each. Since the recalibration it is a prediction rather than a ranking:
//! `(budget + 20) / 10` should equal `us/pixel`, the 20 being the interpreter's
//! per-pixel call overhead. The one effect that carries a mask reads well under
//! its budget, because a budget is a worst case and a mask skips work.

#![no_std]
#![no_main]

mod opcost;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_println::println;

use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::{Machine, NoUniforms, PixelInputs};

/// LEDs in the strip this is sized against. 300 is a 5 m strip at 60/m, which
/// is the common case and what the plan names.
const PIXELS: usize = 300;

/// Frames averaged over, so one unlucky cache state does not decide the answer.
const FRAMES: u32 = 30;

/// The frame budget at 60 fps.
const BUDGET_US: u64 = 16_667;

/// Real bytecode, from the real compiler, for effects people would actually
/// run. Synthetic instruction mixes measure the interpreter; these measure the
/// system.
const PROGRAMS: &[(&str, u32, &[u8])] = &[
    ("07-alert", 136, include_bytes!("../programs/07-alert.lfxb")),
    ("01-breathe", 215, include_bytes!("../programs/01-breathe.lfxb")),
    (
        "05-beat-strobe",
        462,
        include_bytes!("../programs/05-beat-strobe.lfxb"),
    ),
    (
        "12-panel-plasma",
        388,
        include_bytes!("../programs/12-panel-plasma.lfxb"),
    ),
    ("03-drift", 562, include_bytes!("../programs/03-drift.lfxb")),
];

/// Inputs for one LED of a strip, as a real projection would produce them.
///
/// `u` runs 0..1 along the strip and the index counts up, so every pixel gets
/// different values and nothing the interpreter does can be reused between
/// them. Feeding the same inputs 300 times would measure a cache, not a render.
fn inputs_for(i: usize) -> PixelInputs {
    let u = Q16::from_ratio(i as i32, PIXELS as i32);
    PixelInputs {
        x: u,
        y: Q16::ZERO,
        z: Q16::ZERO,
        lx: u,
        ly: Q16::ZERO,
        lz: Q16::ZERO,
        index: Q16::from_int(i as i16),
        count: Q16::from_int(PIXELS as i16),
        u,
        uv_x: u,
        uv_y: Q16::HALF,
        // Something non-zero, since several effects read it and a zero history
        // can short-circuit arithmetic the real thing would perform.
        prev: [Q16::HALF, Q16::HALF, Q16::HALF],
    }
}

/// Microseconds to render `FRAMES` frames of `PIXELS` pixels, and the number of
/// pixels that actually produced a colour.
fn measure(program: &Program<'_>) -> (u64, u32) {
    let mut machine = Machine::new();
    let mut emitted = 0u32;

    let start = esp_hal::time::now();
    for frame in 0..FRAMES {
        // `t` advances like a real show clock so time-dependent effects do
        // different work each frame.
        let t = Q16::from_ratio(frame as i32, 60);
        if machine.run_frame_at(program, t, &mut NoUniforms).is_err() {
            return (0, 0);
        }
        for i in 0..PIXELS {
            match machine.run_pixel(program, &inputs_for(i), &mut NoUniforms) {
                Ok(lumen_vm::vm::PixelOutput::None) => {}
                Ok(_) => emitted += 1,
                Err(_) => return (0, 0),
            }
        }
    }
    let elapsed = (esp_hal::time::now() - start).to_micros();
    (elapsed, emitted)
}

#[esp_hal::main]
fn main() -> ! {
    // The clock the chip actually ships at. Measuring at a lower one would
    // flatter nothing and mislead everyone.
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let _p = esp_hal::init(config);
    let delay = Delay::new();

    loop {
        println!();
        println!("== spike S2: VM throughput, esp32s3 @ {} MHz", opcost::CLOCK_MHZ);
        println!("== {PIXELS} pixels, {FRAMES} frames averaged, budget {BUDGET_US} us/frame");
        println!(
            "{:<17} {:>6} {:>10} {:>9} {:>8} {:>7}",
            "effect", "budget", "us/frame", "us/pixel", "% of 60", "max fps"
        );

        for (name, units, bytes) in PROGRAMS {
            let Ok(program) = Program::parse(bytes) else {
                println!("{name:<17} FAILED TO PARSE");
                continue;
            };
            let (total_us, emitted) = measure(&program);
            if total_us == 0 {
                println!("{name:<17} FAULTED");
                continue;
            }
            let per_frame = total_us / FRAMES as u64;
            // Two decimal places without floating point: this crate forbids it
            // for the same reason the rest of the project does.
            let per_pixel_centi = (total_us * 100) / (FRAMES as u64 * PIXELS as u64);
            let percent = (per_frame * 100) / BUDGET_US;
            let max_fps = if per_frame == 0 {
                0
            } else {
                1_000_000 / per_frame
            };
            println!(
                "{name:<17} {units:>6} {per_frame:>10} {:>6}.{:02} {percent:>7}% {max_fps:>7}",
                per_pixel_centi / 100,
                per_pixel_centi % 100,
            );
            let _ = emitted;
        }

        opcost::run();

        println!("== done; repeating in 10 s");
        delay.delay_millis(10_000);
    }
}
