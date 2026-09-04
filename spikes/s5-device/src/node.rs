//! The device, minus its I/O.
//!
//! Everything here is "what a Lumen node does with a datagram and with a
//! frame". It touches no socket and reads no clock — `main.rs` owns those and
//! passes results in, which is the same seam `lumen-device` draws and the reason
//! this file can be reasoned about without a radio.
//!
//! # What it implements
//!
//! A program arrives as `ProgBegin` / `ProgChunk` × n / `ProgEnd`, which is how
//! bytecode crosses a 1500-byte MTU. A `SrcPush` then puts it on the source
//! stack with a priority and an expiry. From there the real
//! `lumen_device::Renderer` produces the pixels, through the real VM, from the
//! real zone projection.
//!
//! Nothing here is a stand-in. That is the point of the spike: if a pixel comes
//! out wrong, the bug is in something that ships.

use alloc::vec;
use alloc::vec::Vec;

use lumen_device::channels::{Channel, ChannelUniforms, Channels};
use lumen_device::render::{Bound, Rgb, Shard};
use lumen_device::sources::{Source, SourceStack};
use lumen_device::zones::{Clause, DeviceLeds, Led, MapQuality, Membership, Projection, Zone};
use lumen_device::Renderer;
use lumen_proto::msg::Payload;
use lumen_proto::{Datagram, Uuid};
use lumen_vm::output::{Encoded, Output, PowerModel};
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;

/// Largest program this device will hold.
///
/// The corpus runs 1–3 KB. Four is room to spare without pretending a device
/// with 400 KB of RAM can hold something arbitrary.
pub const MAX_PROGRAM: usize = 4096;

/// What a received datagram did, for the log.
///
/// Returned rather than printed, because printing from the middle of a receive
/// loop is how a spike ends up measuring `println!`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handled {
    Ignored,
    NotForThisMesh,
    Undecodable,
    ProgramStarted { len: u32 },
    ProgramChunk { at: u32, len: usize },
    ProgramComplete {
        len: usize,
        budget: u32,
        channels: usize,
    },
    ProgramRejected,
    ChannelClaimed { id: u16 },
    ChannelSet { id: u16, value: i32 },
    ChannelUnknown { id: u16 },
    SourcePushed { priority: u8 },
    SourcePopped,
    SourceRejected,
    ClockSet { offset_us: i64 },
}

/// One Lumen node.
pub struct Node {
    mesh_prefix: [u8; 2],
    leds: DeviceLeds,
    zone: Zone,
    membership: Membership,
    stack: SourceStack,
    renderer: Renderer,
    /// Broadcast values an effect reads: a slider, a beat, a sensor.
    ///
    /// Declared from the program when one arrives, because the program's header
    /// is what says which channels it reads - a device does not decide, it is
    /// told, and that is what lets one effect be pointed at a different producer
    /// without recompiling.
    channels: Channels,

    /// The program being received, and the one being rendered. The same buffer:
    /// a device holds one program at a time here, and a half-received program
    /// replacing a running one is a visible failure the real firmware avoids
    /// with two slots. Called out rather than hidden — see RESULTS.
    program: Vec<u8>,
    program_len: usize,
    expected_len: usize,
    receiving: bool,

    /// Show time minus device time. A `Tick` sets it; until one arrives the
    /// device runs on its own clock, which is right for one device alone and
    /// wrong for two.
    clock_offset_us: i64,
    /// Whether a source is currently admitted, so `main` can say so once rather
    /// than every frame.
    pub rendering: bool,

    /// Linear light, before the output stage turns it into codes.
    frame: Vec<Rgb>,
    /// The output stage's dither state, one entry per channel, carried between
    /// frames. Without it the dark end of every fade lands in four visible
    /// steps and then stops early.
    residual: Vec<i32>,
    output: Output,
}

impl Node {
    /// A node driving `count` LEDs in a line.
    ///
    /// The strip's geometry is declared rather than measured, so the quality is
    /// `Synthetic`: laid along an arbitrary axis from an arbitrary origin.
    /// `u` still runs 0..1 along it and every 1D effect works, which is the
    /// point - **mapping is a pure upgrade**, and a device that has never been
    /// mapped lights correctly rather than waiting to be told where it is.
    pub fn new(mesh_prefix: [u8; 2], device: Uuid, count: u16) -> Node {
        let leds = DeviceLeds {
            device,
            quality: MapQuality::Synthetic,
            leds: (0..count)
                .map(|i| Led {
                    index: i,
                    world: [Q16::from_ratio(i as i32, count as i32), Q16::ZERO, Q16::ZERO],
                    local: [Q16::from_ratio(i as i32, count as i32), Q16::ZERO, Q16::ZERO],
                })
                .collect(),
        };
        let zone = Zone {
            id: Uuid([50; 16]),
            include: vec![Clause::Device { device, leds: None }],
            exclude: vec![],
            projection: Projection::Strip,
        };
        let membership = zone.resolve(&leds);
        Node {
            mesh_prefix,
            leds,
            zone,
            membership,
            // Per-pixel budget this device will spend across all sources, and
            // how many it will admit at once. Generous: a C3 rendering 30 LEDs
            // has far more headroom per pixel than one rendering 300, and the
            // budget is per pixel.
            stack: SourceStack::new(100_000, 4),
            renderer: Renderer::new(),
            channels: Channels::new(),
            program: vec![0; MAX_PROGRAM],
            program_len: 0,
            expected_len: 0,
            receiving: false,
            clock_offset_us: 0,
            rendering: false,
            frame: vec![Rgb::BLACK; count as usize],
            residual: vec![0; count as usize * 3],
            // A 500 mA budget, which is what a USB port promises without
            // negotiating for more. Thirty of these at full white want about
            // 1.2 A, so this strip *will* derate - which is the point: a board
            // that browns out mid-frame looks exactly like a driver that cannot
            // hold one.
            output: Output::new().with_power(PowerModel::ws2812(500)),
        }
    }

    pub fn led_count(&self) -> usize {
        self.leds.leds.len()
    }

    /// Bytes of program held, so a silent device can say whether it ever got
    /// one.
    pub fn program_bytes(&self) -> usize {
        self.program_len
    }

    /// Sources admitted, for the same reason.
    pub fn source_count(&self) -> usize {
        self.stack.active().len()
    }

    /// Device time to show time.
    pub fn show_time(&self, now_us: u64) -> u64 {
        now_us.saturating_add_signed(self.clock_offset_us)
    }

    /// Take one datagram off the wire.
    pub fn receive(&mut self, bytes: &[u8], now_us: u64) -> Handled {
        let Ok(dg) = Datagram::decode(bytes) else {
            return Handled::Undecodable;
        };
        // Answerable from the header alone, which is exactly why the header is
        // not encrypted: a device on a shared network drops somebody else's
        // mesh without decrypting anything.
        if dg.header.mesh_prefix != self.mesh_prefix {
            return Handled::NotForThisMesh;
        }
        let Ok(Some(payload)) = dg.parse_payload() else {
            return Handled::Undecodable;
        };

        match payload {
            Payload::Tick(tick) => {
                let offset = dg.header.show_time_us as i64 - now_us as i64;
                self.clock_offset_us = offset;
                let _ = tick;
                Handled::ClockSet { offset_us: offset }
            }

            Payload::ProgBegin(begin) => {
                if begin.total_len as usize > MAX_PROGRAM {
                    return Handled::ProgramRejected;
                }
                self.expected_len = begin.total_len as usize;
                self.program_len = 0;
                self.receiving = true;
                Handled::ProgramStarted {
                    len: begin.total_len,
                }
            }

            Payload::ProgChunk(chunk) => {
                if !self.receiving {
                    return Handled::Ignored;
                }
                let at = chunk.offset as usize;
                let end = at + chunk.data.len();
                if end > self.expected_len || end > MAX_PROGRAM {
                    // A chunk outside the length the sender declared is either a
                    // corrupt transfer or a different program's chunk arriving
                    // late. Either way, writing it would scribble on the buffer.
                    self.receiving = false;
                    return Handled::ProgramRejected;
                }
                self.program[at..end].copy_from_slice(chunk.data);
                self.program_len = self.program_len.max(end);
                Handled::ProgramChunk {
                    at: chunk.offset,
                    len: chunk.data.len(),
                }
            }

            Payload::ProgEnd(_) => {
                if !self.receiving || self.program_len != self.expected_len {
                    self.receiving = false;
                    return Handled::ProgramRejected;
                }
                self.receiving = false;
                // Parsed once, here, rather than every frame. The answer cannot
                // change, and re-parsing at 30 Hz is a measurable slice of a
                // frame spent proving something already known.
                match Program::parse(&self.program[..self.program_len]) {
                    Ok(p) => {
                        // Declare what this program reads. A channel nobody has
                        // published to yet reads its default, so an effect is
                        // never waiting on a producer before it will render.
                        //
                        // The hold is generous here: a slider on a phone sends
                        // when a finger moves, and a finger that stops moving is
                        // not a producer that has died.
                        for slot in 0..p.channel_count() {
                            if let Some(id) = p.channel_id(slot as u8) {
                                self.channels.declare(Channel::new(id, 2_000, Q16::ZERO));
                            }
                        }
                        Handled::ProgramComplete {
                            len: self.program_len,
                            budget: p.budget,
                            channels: p.channel_count(),
                        }
                    }
                    Err(_) => {
                        self.program_len = 0;
                        Handled::ProgramRejected
                    }
                }
            }

            Payload::ChanClaim(claim) => {
                // Read the clock before taking the borrow: `show_time` needs
                // `&self` and `get_mut` holds `&mut self`.
                let now = self.show_time(now_us);
                let Some(channel) = self.channels.get_mut(claim.channel_id) else {
                    return Handled::ChannelUnknown {
                        id: claim.channel_id,
                    };
                };
                channel.claim(
                    now,
                    dg.header.sender_prefix,
                    claim.priority,
                    claim.lease_ms,
                );
                Handled::ChannelClaimed {
                    id: claim.channel_id,
                }
            }

            Payload::Chan(chan) => {
                let now = self.show_time(now_us);
                let Some(channel) = self.channels.get_mut(chan.channel_id) else {
                    return Handled::ChannelUnknown {
                        id: chan.channel_id,
                    };
                };
                // One Q16 per channel. A multi-value producer sends one
                // datagram per band rather than packing them, so a receiver
                // that only reads band 3 is not decoding sixteen.
                if chan.payload.len() < 4 {
                    return Handled::Undecodable;
                }
                let raw = i32::from_le_bytes([
                    chan.payload[0],
                    chan.payload[1],
                    chan.payload[2],
                    chan.payload[3],
                ]);
                channel.publish(
                    now,
                    dg.header.sender_prefix,
                    chan.producer_seq,
                    Q16(raw),
                );
                Handled::ChannelSet {
                    id: chan.channel_id,
                    value: raw,
                }
            }

            Payload::SrcPush(push) => {
                let source = Source {
                    id: push.source_id,
                    zone: self.zone.id,
                    scene: push.scene_id,
                    priority: push.priority,
                    expires_at_us: push.expires_at,
                    fade_in_ms: push.fade_in_ms,
                    fade_out_ms: push.fade_out_ms,
                    pushed_at_us: self.show_time(now_us),
                    cost: 10,
                };
                match self
                    .stack
                    .push(self.show_time(now_us), source, &mut Vec::new())
                {
                    Ok(()) => {
                        self.rendering = true;
                        Handled::SourcePushed {
                            priority: push.priority,
                        }
                    }
                    Err(_) => Handled::SourceRejected,
                }
            }

            Payload::SrcPop(pop) => {
                self.stack
                    .pop(self.show_time(now_us), pop.source_id, &mut Vec::new());
                Handled::SourcePopped
            }

            _ => Handled::Ignored,
        }
    }

    /// Render one frame into `out`, three bytes per LED.
    ///
    /// Returns the budget units spent, or `None` if there was nothing to render.
    /// `out` is left alone in that case rather than blacked: a device with no
    /// source keeps showing what it was showing, which is what makes an ambient
    /// floor a floor rather than a special case.
    pub fn render(&mut self, now_us: u64, out: &mut [u8]) -> Option<(u32, Encoded)> {
        if self.program_len == 0 {
            return None;
        }
        let program = Program::parse(&self.program[..self.program_len]).ok()?;
        let show_us = self.show_time(now_us);
        let t = Q16::from_micros(show_us);

        // The device's channels, seen as this program's uniforms. Without this
        // every channel read returns zero - which is exactly what a channel with
        // no producer correctly returns, so the mistake is invisible.
        let mut uniforms = ChannelUniforms::new(&self.channels, &program, show_us);
        let bound = [Bound {
            source: *self.stack.active().first()?,
            program: &program,
            membership: &self.membership,
            projection: self.zone.projection,
        }];
        let report = self.renderer.render_shard(
            show_us,
            t,
            &self.leds,
            &self.stack,
            &bound,
            &mut uniforms,
            &mut self.frame,
            Shard::whole(self.leds.leds.len() as u16),
        );

        // Linear light through the output stage, which is where brightness, the
        // power budget and the dithering live. Writing `(v * 255) >> 16` here
        // instead - which is what this did - throws away everything below one
        // code and lets the strip ask the supply for more than it has.
        let mut linear = [Q16::ZERO; MAX_CHANNELS];
        let n = (self.frame.len() * 3).min(MAX_CHANNELS);
        for (i, px) in self.frame.iter().enumerate() {
            if i * 3 + 2 >= n {
                break;
            }
            linear[i * 3] = px.r;
            linear[i * 3 + 1] = px.g;
            linear[i * 3 + 2] = px.b;
        }
        let encoded = self.output.encode(&linear[..n], Some(&mut self.residual), out);
        Some((report.spent, encoded))
    }

    /// Drop sources whose expiry has passed.
    ///
    /// Called every frame. Expiry is absolute show time, so it happens at the
    /// same instant on every device rather than whenever each one got the push.
    pub fn advance(&mut self, now_us: u64) {
        self.stack
            .advance(self.show_time(now_us), &mut Vec::new());
        self.rendering = !self.stack.active().is_empty();
    }
}

/// Channels the linear staging buffer holds. 300 LEDs, the size the project
/// sizes against, so the same firmware drives a longer strip unchanged.
const MAX_CHANNELS: usize = 900;
