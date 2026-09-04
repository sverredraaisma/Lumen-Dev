//! Rendering on the host, through the device's own render loop.
//!
//! `--simulate` exists because "the strip looks wrong" has two possible causes
//! and only one of them is the device. This runs the **same**
//! `lumen_device::Renderer` the firmware runs, over the same LED count at the
//! same frame rate, and prints each frame as a ramp.
//!
//! So if the ramp is right and the strip is not, the fault is below the
//! renderer — the LED driver, the wiring, or the missing output stage. And if
//! the ramp is wrong too, no amount of staring at hardware will help.
//!
//! The desktop daemon already previews effects, and deliberately is not reused
//! here: it renders by its own route, so agreeing with it would prove only that
//! two implementations agree, which is the thing under suspicion.

use lumen_device::render::{Bound, Rgb, Shard};
use lumen_device::sources::{Source, SourceStack};
use lumen_device::zones::{Clause, DeviceLeds, Led, MapQuality, Projection, Zone};
use lumen_device::Renderer;
use lumen_proto::Uuid;
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::NoUniforms;

/// Render `frames` frames and print the last few.
pub fn simulate(bytecode: &[u8], leds: u16, fps: u32, frames: u32) {
    let device = Uuid([1; 16]);
    let dev = DeviceLeds {
        device,
        quality: MapQuality::Synthetic,
        leds: (0..leds)
            .map(|i| Led {
                index: i,
                world: [Q16::from_ratio(i as i32, leds as i32), Q16::ZERO, Q16::ZERO],
                local: [Q16::from_ratio(i as i32, leds as i32), Q16::ZERO, Q16::ZERO],
            })
            .collect(),
    };
    let zone = Zone {
        id: Uuid([50; 16]),
        include: vec![Clause::Device { device, leds: None }],
        exclude: vec![],
        projection: Projection::Strip,
    };
    let mem = zone.resolve(&dev);
    let src = Source {
        id: Uuid([7; 16]),
        zone: zone.id,
        scene: Uuid([7; 16]),
        priority: 100,
        // A non-ambient source must carry an expiry - the stack refuses one
        // without, so that a controller going away cannot leave a device stuck
        // showing something forever.
        expires_at_us: Some(u64::MAX),
        fade_in_ms: 0,
        fade_out_ms: 0,
        pushed_at_us: 0,
        cost: 10,
    };
    let mut stack = SourceStack::new(100_000, 4);
    stack.push(0, src, &mut Vec::new()).expect("an ambient source");

    let program = match Program::parse(bytecode) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("the bytecode does not parse: {e:?}");
            return;
        }
    };
    let bound = [Bound {
        source: src,
        program: &program,
        membership: &mem,
        projection: zone.projection,
    }];

    let mut renderer = Renderer::new();
    let mut out = vec![Rgb::BLACK; leds as usize];
    // A perceptual ramp, so a dim tail reads in a terminal the way it does to an
    // eye. The strip is sent linear values; this is for reading.
    let ramp: Vec<char> = " .:-=+*#%@".chars().collect();

    println!("simulating {leds} LEDs at {fps} fps, last frames:");
    for f in 0..frames {
        let us = (f as u64 * 1_000_000) / fps as u64;
        let t = Q16::from_micros(us);
        let report = renderer.render_shard(
            us,
            t,
            &dev,
            &stack,
            &bound,
            &mut NoUniforms,
            &mut out,
            Shard::whole(leds),
        );
        if f + 6 < frames {
            continue;
        }
        let line: String = out
            .iter()
            .map(|p| {
                let v = p.r.0.max(p.g.0).max(p.b.0).clamp(0, 65536) as f64 / 65536.0;
                // A square root stands in for a gamma curve. Close enough to
                // read by, and it needs no table.
                let perceptual = v.sqrt();
                let at = (perceptual * (ramp.len() - 1) as f64) as usize;
                ramp[at.min(ramp.len() - 1)]
            })
            .collect();
        println!("{f:>4} |{line}| {} units", report.spent);
    }
}
