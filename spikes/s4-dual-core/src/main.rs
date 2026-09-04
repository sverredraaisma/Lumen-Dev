//! Spike S4: is the second core worth having, and does it render the same show?
//!
//! S2 measured the interpreter and found it dispatch-bound: 583 ns per
//! instruction on an S3, about 140 cycles, roughly 80% of it dispatch. A faster
//! ESP32 buys its clock and nothing else. The one thing left that could buy
//! real headroom is the second core, because the pixels of a frame are
//! independent — each is a pure function of its own position and the values the
//! `frame` section hoisted.
//!
//! # Two questions, and the second matters more
//!
//! **How much faster?** Two cores rendering halves of a strip should approach
//! 2×, less whatever the split costs: each shard runs the `frame` section for
//! itself, and the cores have to meet at the end of every frame.
//!
//! **Is it the same frame?** This is the one that decides whether the feature
//! ships. Every device in a mesh computes the same show from the same clock —
//! that is what a gradient spanning six strips rests on, and why the VM is
//! fixed point rather than float. A two-core device that rendered even one
//! pixel differently from a one-core device would break that agreement, and it
//! would not be visible until two kinds of device were in one room.
//!
//! So this compares bytes, not just microseconds. `lumen-device` has a host
//! test making the same claim; this is the claim on real silicon, on two real
//! cores, with two real caches.
//!
//! # What runs
//!
//! The actual `Renderer::render_shard` from `lumen-device`, not a copy of the
//! loop. Five effects from the shipped corpus spanning 136 to 562 budget units,
//! 300 LEDs, the same as S2 so the numbers can be put side by side.
//!
//! `07-alert` is in the list for a reason beyond its cost: it carries a mask, so
//! it is the effect where a contiguous split is least fair. Whatever imbalance
//! costs, it shows up there.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::cpu_control::{CpuControl, Stack};
use esp_hal::delay::Delay;
use esp_println::println;

use lumen_device::render::{Bound, Rgb, Shard};
use lumen_device::sources::{Source, SourceStack};
use lumen_device::zones::{Clause, DeviceLeds, Led, MapQuality, Membership, Projection, Zone};
use lumen_device::Renderer;
use lumen_proto::Uuid;
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::NoUniforms;

/// LEDs in the strip. 300 is a 5 m strip at 60/m — the common case, and what S2
/// used, so the two spikes can be read against each other.
const PIXELS: u16 = 300;

/// Frames averaged over, so one unlucky cache state does not decide the answer.
const FRAMES: u32 = 30;

/// The frame budget at 60 fps.
const BUDGET_US: u64 = 16_667;

const PROGRAMS: &[(&str, u32, &[u8])] = &[
    ("07-alert", 136, include_bytes!("../programs/07-alert.lfxb")),
    ("01-breathe", 215, include_bytes!("../programs/01-breathe.lfxb")),
    (
        "12-panel-plasma",
        388,
        include_bytes!("../programs/12-panel-plasma.lfxb"),
    ),
    (
        "05-beat-strobe",
        462,
        include_bytes!("../programs/05-beat-strobe.lfxb"),
    ),
    ("03-drift", 562, include_bytes!("../programs/03-drift.lfxb")),
];

/// Where the app core's stack lives. 32 KiB: the render loop allocates from the
/// heap rather than the stack, so this only has to hold the call chain.
static mut APP_STACK: Stack<32768> = Stack::new();

/// The handshake. The main core publishes a frame number in `WORK`; the app
/// core renders its shard and publishes the same number in `DONE`.
///
/// Two atomics and no lock, because there is nothing to protect: the shards own
/// disjoint LEDs, so they write disjoint memory. That is not a happy accident,
/// it is what `Shard` is for — `split_at_mut` hands each core its own run of the
/// output buffer and the type system carries the guarantee from there.
static WORK: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);

/// Which program the app core should be rendering, as an index into `PROGRAMS`.
static PROGRAM: AtomicU32 = AtomicU32::new(0);

/// Show time for the frame being rendered, in Q16 seconds. Written before
/// `WORK`, read after — the release/acquire pair on `WORK` is what makes that
/// ordering real rather than hopeful.
static T_Q16: AtomicU32 = AtomicU32::new(0);

/// Where the app core leaves its half of the strip.
///
/// Its own buffer rather than a slice of the main core's, because the two cores
/// are separate stacks with no lifetime relationship a borrow could express.
/// The main core copies it in after the join; at 150 pixels that is a memcpy
/// against a render, and it is measured inside the timing rather than outside.
static mut APP_OUT: [Rgb; PIXELS as usize] = [Rgb::BLACK; PIXELS as usize];

/// One device's LEDs, laid out along a line as a strip is.
fn device() -> DeviceLeds {
    DeviceLeds {
        device: Uuid([1; 16]),
        quality: MapQuality::Mapped,
        leds: (0..PIXELS)
            .map(|i| Led {
                index: i,
                world: [Q16::from_ratio(i as i32, PIXELS as i32), Q16::ZERO, Q16::ZERO],
                local: [Q16::from_ratio(i as i32, PIXELS as i32), Q16::ZERO, Q16::ZERO],
            })
            .collect(),
    }
}

fn zone_over(dev: &DeviceLeds) -> (Zone, Membership) {
    let z = Zone {
        id: Uuid([50; 16]),
        include: vec![Clause::Device {
            device: dev.device,
            leds: None,
        }],
        exclude: vec![],
        projection: Projection::Strip,
    };
    let m = z.resolve(dev);
    (z, m)
}

fn source() -> Source {
    Source {
        id: Uuid([1; 16]),
        zone: Uuid([50; 16]),
        scene: Uuid([1; 16]),
        priority: 10,
        expires_at_us: None,
        fade_in_ms: 0,
        fade_out_ms: 0,
        pushed_at_us: 0,
        cost: 10,
    }
}

/// What the app core does forever: wait for a frame, render the back half of
/// the strip, say it is done.
fn app_core() {
    let dev = device();
    let (zone, mem) = zone_over(&dev);
    let src = source();
    let mut stack = SourceStack::new(100_000, 4);
    stack.push(0, src, &mut Vec::new()).expect("ambient source");

    // Its own renderer. The VM's register file survives from `frame` into every
    // pixel of that frame, so two cores sharing one machine would be two cores
    // writing one register file.
    let mut renderer = Renderer::new();
    let shard = Shard::new(1, 2, PIXELS).expect("the back half");

    let mut last = 0u32;
    loop {
        // Acquire, paired with the main core's release on `WORK`: everything it
        // wrote before publishing is visible here afterwards.
        let token = WORK.load(Ordering::Acquire);
        if token == last {
            continue;
        }
        last = token;

        let which = PROGRAM.load(Ordering::Relaxed);
        let (_, _, bytes) = PROGRAMS[which as usize];
        let program = Program::parse(bytes).expect("a program the main core parsed");
        let t = Q16(T_Q16.load(Ordering::Relaxed) as i32);
        let bound = [Bound {
            source: src,
            program: &program,
            membership: &mem,
            projection: zone.projection,
        }];

        // Safety: this core owns `APP_OUT[..shard.len()]` for the whole of a
        // frame. The main core reads it only after seeing `DONE`, which is
        // released below.
        let out = unsafe { &mut *core::ptr::addr_of_mut!(APP_OUT) };
        // Start from black each frame, exactly as the single-core comparison
        // does. Without this a program that renders nothing would be compared
        // against whatever the previous program left here, and the difference
        // would be reported as a split that renders a different frame - which
        // is a lie in the direction that matters most.
        out.fill(Rgb::BLACK);
        renderer.render_shard(
            0,
            t,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out[..shard.len() as usize],
            shard,
        );

        DONE.store(token, Ordering::Release);
    }
}


/// Microseconds to render `FRAMES` frames of the whole strip on one core, and
/// the frame it ended on.
fn measure_single(program: &Program<'_>) -> (u64, Vec<Rgb>, usize) {
    let dev = device();
    let (zone, mem) = zone_over(&dev);
    let src = source();
    let mut stack = SourceStack::new(100_000, 4);
    stack.push(0, src, &mut Vec::new()).expect("ambient source");

    let mut renderer = Renderer::new();
    let mut out = vec![Rgb::BLACK; PIXELS as usize];
    let bound = [Bound {
        source: src,
        program,
        membership: &mem,
        projection: zone.projection,
    }];

    let mut faults = 0usize;
    let start = esp_hal::time::now();
    for f in 0..FRAMES {
        let t = Q16::from_ratio(f as i32, 60);
        let report = renderer.render_shard(
            0,
            t,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
            Shard::whole(PIXELS),
        );
        faults += report.faults.len();
        if f == 0 {
            if let Some(first) = report.faults.first() {
                println!("   ^ first fault: {first:?}; program budget {}", program.budget);
            }
        }
    }
    ((esp_hal::time::now() - start).to_micros(), out, faults)
}

/// The same, with the back half of every frame rendered on the app core.
fn measure_split(program: &Program<'_>, program_index: u32) -> (u64, Vec<Rgb>, usize) {
    let dev = device();
    let (zone, mem) = zone_over(&dev);
    let src = source();
    let mut stack = SourceStack::new(100_000, 4);
    stack.push(0, src, &mut Vec::new()).expect("ambient source");

    let mut renderer = Renderer::new();
    let mut out = vec![Rgb::BLACK; PIXELS as usize];
    let bound = [Bound {
        source: src,
        program,
        membership: &mem,
        projection: zone.projection,
    }];
    let mine = Shard::new(0, 2, PIXELS).expect("the front half");
    let theirs = Shard::new(1, 2, PIXELS).expect("the back half");

    PROGRAM.store(program_index, Ordering::Relaxed);

    let mut faults = 0usize;
    let start = esp_hal::time::now();
    for f in 0..FRAMES {
        let t = Q16::from_ratio(f as i32, 60);
        T_Q16.store(t.0 as u32, Ordering::Relaxed);

        // Release: the app core's acquire on `WORK` makes `T_Q16` and `PROGRAM`
        // visible to it.
        let token = f + 1;
        WORK.store(token, Ordering::Release);

        let report = renderer.render_shard(
            0,
            t,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out[..mine.len() as usize],
            mine,
        );
        faults += report.faults.len();

        // Join. Spinning rather than sleeping: the wait is tens of
        // microseconds and this is measuring the split, not the scheduler.
        while DONE.load(Ordering::Acquire) != token {
            core::hint::spin_loop();
        }

        // The copy is inside the timing on purpose. A real firmware would hand
        // the DMA two descriptors and skip it, but this spike should not
        // flatter the split by leaving out work it caused.
        let app = unsafe { &*core::ptr::addr_of!(APP_OUT) };
        out[mine.len() as usize..].copy_from_slice(&app[..theirs.len() as usize]);
    }
    ((esp_hal::time::now() - start).to_micros(), out, faults)
}

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let p = esp_hal::init(config);
    // A membership is a Vec and the machines live in a BTreeMap, so a device
    // that renders needs a heap. 64 KiB is far more than 300 LEDs need; sizing
    // it tightly is a firmware question, not a spike one.
    esp_alloc::heap_allocator!(64 * 1024);
    let delay = Delay::new();

    let mut cpu = CpuControl::new(p.CPU_CTRL);
    let stack = unsafe { &mut *core::ptr::addr_of_mut!(APP_STACK) };
    let _guard = match cpu.start_app_core(stack, app_core) {
        Ok(g) => g,
        Err(e) => {
            println!("could not start the app core: {e:?}");
            loop {}
        }
    };

    loop {
        println!();
        println!("== spike S4: splitting the pixel loop, esp32s3 @ 240 MHz");
        println!("== {PIXELS} pixels, {FRAMES} frames averaged, budget {BUDGET_US} us/frame");
        println!(
            "{:<17} {:>6} {:>9} {:>9} {:>8} {:>8} {:>7}",
            "effect", "budget", "1 core", "2 cores", "speedup", "% of 60", "same?"
        );

        let mut all_identical = true;
        for (index, (name, units, bytes)) in PROGRAMS.iter().enumerate() {
            let Ok(program) = Program::parse(bytes) else {
                println!("{name:<17} FAILED TO PARSE");
                continue;
            };

            let (single_us, single_out, single_faults) = measure_single(&program);
            let (split_us, split_out, split_faults) = measure_split(&program, index as u32);

            let one = single_us / FRAMES as u64;
            let two = split_us / FRAMES as u64;
            // Two decimal places without floating point, which this project
            // forbids in shipping code and there is no reason to allow here.
            let speedup_centi = if two == 0 { 0 } else { (one * 100) / two };
            let percent = (two * 100) / BUDGET_US;

            // The question that decides whether this ships.
            let identical = single_out == split_out;
            all_identical &= identical;
            let differing = single_out
                .iter()
                .zip(&split_out)
                .filter(|(a, b)| a != b)
                .count();

            println!(
                "{name:<17} {units:>6} {one:>8}u {two:>8}u {:>5}.{:02}x {percent:>7}% {:>7}",
                speedup_centi / 100,
                speedup_centi % 100,
                if identical { "yes" } else { "NO" },
            );
            if !identical {
                println!("   ^ {differing} of {PIXELS} pixels differ between one core and two");
            }
            // A render that costs nothing rendered nothing. Saying so is the
            // difference between a fast effect and a broken one, and the two
            // look identical in a column of microseconds.
            if single_faults > 0 || split_faults > 0 {
                println!(
                    "   ^ faulted: {single_faults} on one core, {split_faults} on the front half of two"
                );
            }
        }

        println!();
        if all_identical {
            println!("== two cores render exactly what one core renders, on all {} effects", PROGRAMS.len());
        } else {
            println!("== SPLIT RENDERS A DIFFERENT FRAME. Do not ship this.");
        }
        println!("== done; repeating in 10 s");
        delay.delay_millis(10_000);
    }
}
