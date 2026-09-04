//! Rendering on the host, through the device's own render loop.
//!
//! Two modes, built on one fixture so they cannot disagree with each other.
//!
//! `--simulate` prints frames as a ramp. It exists because "the strip looks
//! wrong" has two possible causes and only one of them is the device: if the
//! ramp is right and the strip is not, the fault is below the renderer — the LED
//! driver, the wiring, or the output stage.
//!
//! `--verify` prints a frame's **fingerprint** at an exact show time, to compare
//! against the `== frame ... at show ...` line a device prints. Equal
//! fingerprints mean the host and the device produced identical linear frames
//! for the same moment. That is the property the whole architecture rests on —
//! it is why the VM is fixed point — and it had been asserted in tests and never
//! once checked against real silicon.
//!
//! The desktop daemon's preview is deliberately not reused for either: it
//! renders by its own route, so agreeing with it would prove only that two
//! implementations agree, which is the thing under suspicion.

use lumen_device::render::{Bound, Rgb, Shard};
use lumen_device::sources::{Source, SourceStack};
use lumen_device::zones::{Clause, DeviceLeds, Led, MapQuality, Membership, Projection, Zone};
use lumen_device::Renderer;
use lumen_proto::Uuid;
use lumen_vm::digest::Digest;
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::NoUniforms;

/// The device both modes render as.
///
/// Has to match the firmware's own fixture exactly — same LED count, same
/// synthetic coordinates, same zone projection — or a fingerprint comparison
/// measures the fixture rather than the renderer.
struct Fixture {
    dev: DeviceLeds,
    zone: Zone,
    membership: Membership,
    stack: SourceStack,
    source: Source,
}

fn fixture(leds: u16) -> Fixture {
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
    let membership = zone.resolve(&dev);
    let source = Source {
        id: Uuid([7; 16]),
        zone: zone.id,
        scene: Uuid([7; 16]),
        priority: 100,
        // A non-ambient source must carry an expiry - the stack refuses one
        // without, so a controller going away cannot leave a device stuck
        // showing something for ever.
        expires_at_us: Some(u64::MAX),
        fade_in_ms: 0,
        fade_out_ms: 0,
        pushed_at_us: 0,
        cost: 10,
    };
    let mut stack = SourceStack::new(100_000, 4);
    stack
        .push(0, source, &mut Vec::new())
        .expect("an ambient source");
    Fixture {
        dev,
        zone,
        membership,
        stack,
        source,
    }
}

/// Render forward from zero to `until_us`, calling `each` with every frame.
///
/// Forward rather than jumping to the moment, because `dt` and the per-pixel
/// history both depend on the frames before it. A device that has been running
/// for a minute is not in the state of one handed a timestamp, and comparing
/// against the second would prove nothing.
fn render_forward(
    bytecode: &[u8],
    leds: u16,
    fps: u32,
    until_us: u64,
    mut each: impl FnMut(u64, &[Rgb], u32),
) {
    let f = fixture(leds);
    let program = match Program::parse(bytecode) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("the bytecode does not parse: {e:?}");
            return;
        }
    };
    let bound = [Bound {
        source: f.source,
        program: &program,
        membership: &f.membership,
        projection: f.zone.projection,
    }];

    let mut renderer = Renderer::new();
    let mut out = vec![Rgb::BLACK; leds as usize];
    let step = (1_000_000 / fps as u64).max(1);
    let mut t_us = 0u64;
    loop {
        let report = renderer.render_shard(
            t_us,
            Q16::from_micros(t_us),
            &f.dev,
            &f.stack,
            &bound,
            &mut NoUniforms,
            &mut out,
            Shard::whole(leds),
        );
        each(t_us, &out, report.spent);
        if t_us >= until_us {
            return;
        }
        t_us = (t_us + step).min(until_us);
    }
}

/// A renderer parked on one effect, for a node that draws rather than sends.
pub struct Live {
    fixture: Fixture,
    bytecode: Vec<u8>,
    renderer: Renderer,
    out: Vec<Rgb>,
}

impl Live {
    pub fn new(bytecode: Vec<u8>, leds: u16) -> Option<Live> {
        Program::parse(&bytecode).ok()?;
        Some(Live {
            fixture: fixture(leds),
            bytecode,
            renderer: Renderer::new(),
            out: vec![Rgb::BLACK; leds as usize],
        })
    }

    /// Render the frame at `show_us` and return its fingerprint.
    ///
    /// `show_us` must already be on the frame grid: two synchronised nodes never
    /// render on the same microsecond, and comparing frames drawn at whatever
    /// moment each happened to wake would show a difference that is not there.
    pub fn frame(&mut self, show_us: u64) -> u64 {
        let f = &self.fixture;
        let program = Program::parse(&self.bytecode).expect("checked at construction");
        let bound = [Bound {
            source: f.source,
            program: &program,
            membership: &f.membership,
            projection: f.zone.projection,
        }];
        let leds = self.out.len() as u16;
        self.renderer.render_shard(
            show_us,
            Q16::from_micros(show_us),
            &f.dev,
            &f.stack,
            &bound,
            &mut NoUniforms,
            &mut self.out,
            Shard::whole(leds),
        );
        digest_of(&self.out)
    }
}

/// One frame's fingerprint, hashed the way a device hashes it.
fn digest_of(frame: &[Rgb]) -> u64 {
    let mut d = Digest::new();
    for px in frame {
        d.push(px.r);
        d.push(px.g);
        d.push(px.b);
    }
    d.value()
}

/// Print the last few frames as a ramp.
pub fn simulate(bytecode: &[u8], leds: u16, fps: u32, frames: u32) {
    // A perceptual ramp, so a dim tail reads in a terminal the way it does to an
    // eye. The strip is sent linear values; this is for reading.
    let ramp: Vec<char> = " .:-=+*#%@".chars().collect();
    let step = (1_000_000 / fps as u64).max(1);
    let until = step * frames.saturating_sub(1) as u64;

    println!("simulating {leds} LEDs at {fps} fps, last frames:");
    render_forward(bytecode, leds, fps, until, |t_us, frame, spent| {
        if t_us + step * 6 < until {
            return;
        }
        let line: String = frame
            .iter()
            .map(|p| {
                let v = p.r.0.max(p.g.0).max(p.b.0).clamp(0, 65536) as f64 / 65536.0;
                // A square root stands in for a gamma curve. Close enough to
                // read by, and it needs no table.
                let at = (v.sqrt() * (ramp.len() - 1) as f64) as usize;
                ramp[at.min(ramp.len() - 1)]
            })
            .collect();
        println!("{t_us:>9} us |{line}| {spent} units");
    });
}

/// Print the fingerprint of the frame at exactly `show_us`.
///
/// Rendered from a fresh renderer at that one moment, which is **exact only for
/// an effect that is a pure function of position and time**. A feedback effect
/// depends on every frame before it — `prev` is this pixel's own history and
/// `dt` is the gap between frames — so comparing one against a device that has
/// been running for half a minute would be comparing two different states and
/// calling the difference a bug.
///
/// So this refuses a program that reads `dt`, which the header declares. It
/// cannot see `prev` the same way, which is a real limitation and is why the
/// effect to verify with is one from the corpus known to be stateless.
pub fn verify(bytecode: &[u8], leds: u16, show_us: u64) {
    let f = fixture(leds);
    let program = match Program::parse(bytecode) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("the bytecode does not parse: {e:?}");
            return;
        }
    };
    if program.reads_dt {
        eprintln!(
            "this effect reads `dt`, so its frame depends on the ones before it.
             A single-frame fingerprint would compare two different histories.
             Verify with an effect that is a pure function of position and time."
        );
        return;
    }

    let bound = [Bound {
        source: f.source,
        program: &program,
        membership: &f.membership,
        projection: f.zone.projection,
    }];
    let mut renderer = Renderer::new();
    let mut out = vec![Rgb::BLACK; leds as usize];
    renderer.render_shard(
        show_us,
        Q16::from_micros(show_us),
        &f.dev,
        &f.stack,
        &bound,
        &mut NoUniforms,
        &mut out,
        Shard::whole(leds),
    );
    println!("host  frame {:016x} at show {show_us} us", digest_of(&out));
}
