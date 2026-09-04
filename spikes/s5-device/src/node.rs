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
use lumen_vm::digest::Digest;
use lumen_vm::output::{Encoded, Output, PowerModel};
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;

/// Zones this device resolves: the whole strip, and each half.
pub const ZONES: usize = 3;

/// Zone ids, as the byte every one of a zone UUID's sixteen bytes is set to.
pub const ZONE_ALL: u8 = 50;
pub const ZONE_FIRST: u8 = 51;
pub const ZONE_SECOND: u8 = 52;

/// Programs a device holds at once.
///
/// Two: a show and an alert over it. That is the smallest number that makes the
/// source stack mean anything, and more than a spike needs to prove the point.
pub const SLOTS: usize = 2;

/// Sources this device will render at once.
pub const MAX_SOURCES: usize = 4;

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
}

/// One Lumen node.
pub struct Node {
    mesh_prefix: [u8; 2],
    leds: DeviceLeds,
    /// Zones this device belongs to, each already resolved against its LEDs.
    ///
    /// Three, and the two halves are the point: `u` runs 0..1 across *the zone
    /// a source targets*, not across the strip, so the same effect pushed at
    /// each half draws twice rather than stretching once. That is what makes an
    /// effect independent of the fixture it lands on, and it had never run
    /// outside the simulator.
    ///
    /// Defined locally here. In the real system a zone is a record that arrives
    /// over the wire and is resolved on a mapping change - never per frame.
    zones: [(Zone, Membership); ZONES],
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
    /// One program per slot, so an alert can sit over a show.
    ///
    /// A device holding one program can only ever show one thing, which makes
    /// the source stack - priority, expiry, a higher source winning a pixel -
    /// untestable on hardware. Two is the smallest number that makes it real.
    slots: [Vec<u8>; SLOTS],
    slot_len: [usize; SLOTS],
    /// Which slot is being filled, and how much is expected.
    filling: Option<usize>,
    expected_len: usize,
    /// Which slot each admitted source renders from.
    ///
    /// The real binding is a scene record naming a program, and this spike has
    /// no records - so a source takes the slot of the program that most recently
    /// finished arriving. That is the controller's contract here: send a
    /// program, then push the source that uses it. Stated plainly because it is
    /// the one place this device is not the real thing.
    source_slot: [(Uuid, usize); MAX_SOURCES],
    source_slots_used: usize,
    last_filled: usize,

    /// Whether a source is currently admitted, so `main` can say so once rather
    /// than every frame.
    pub rendering: bool,

    /// Linear light, before the output stage turns it into codes.
    frame: Vec<Rgb>,
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
        // The whole strip, then each half. A zone naming a range of LEDs is
        // the simplest thing that is not the whole device, and it is enough to
        // show `u` being relative to the zone rather than to the strip.
        let ranges = [
            (ZONE_ALL, None),
            (ZONE_FIRST, Some((0, count / 2))),
            (ZONE_SECOND, Some((count / 2, count))),
        ];
        let zones = ranges.map(|(id, leds_range)| {
            let zone = Zone {
                id: Uuid([id; 16]),
                include: vec![Clause::Device {
                    device,
                    leds: leds_range,
                }],
                exclude: vec![],
                projection: Projection::Strip,
            };
            let membership = zone.resolve(&leds);
            (zone, membership)
        });
        Node {
            mesh_prefix,
            leds,
            zones,
            // Per-pixel budget this device will spend across all sources, and
            // how many it will admit at once. Generous: a C3 rendering 30 LEDs
            // has far more headroom per pixel than one rendering 300, and the
            // budget is per pixel.
            stack: SourceStack::new(100_000, 4),
            renderer: Renderer::new(),
            channels: Channels::new(),
            slots: [const { Vec::new() }; SLOTS],
            slot_len: [0; SLOTS],
            filling: None,
            expected_len: 0,
            source_slot: [(Uuid([0; 16]), 0); MAX_SOURCES],
            source_slots_used: 0,
            last_filled: 0,
            rendering: false,
            frame: vec![Rgb::BLACK; count as usize],
            // A 500 mA budget, which is what a USB port promises without
            // negotiating for more. Thirty of these at full white want about
            // 1.2 A, so this strip *will* derate - which is the point: a board
            // that browns out mid-frame looks exactly like a driver that cannot
            // hold one.
            //
            // No dithering. It was tried and it looks like a fault: at 30 fps a
            // pixel sitting near half a code toggles at about 15 Hz, which is
            // close to the worst frequency there is for human vision. A fade
            // that ends slightly early is the better trade on a bare strip.
            output: Output::new().with_power(PowerModel::ws2812(500)),
        }
    }

    pub fn led_count(&self) -> usize {
        self.leds.leds.len()
    }

    /// Bytes of program held across every slot, so a silent device can say
    /// whether it ever got one.
    pub fn program_bytes(&self) -> usize {
        self.slot_len.iter().sum()
    }

    /// Sources admitted, for the same reason.
    pub fn source_count(&self) -> usize {
        self.stack.active().len()
    }

    /// The zone a source named, falling back to the whole device.
    ///
    /// Falling back rather than refusing: a source naming a zone this device
    /// has never heard of should light something, and the whole strip is the
    /// least surprising something. A device is never dark because of software.
    fn zone_for(&self, id: Uuid) -> (&Zone, &Membership) {
        let found = self
            .zones
            .iter()
            .find(|(zone, _)| zone.id == id)
            .unwrap_or(&self.zones[0]);
        (&found.0, &found.1)
    }

    /// Remember which slot a source renders from.
    ///
    /// Overwrites an existing entry for the same source, so re-pushing a source
    /// after loading a new program moves it rather than leaving both.
    fn bind(&mut self, source: Uuid, slot: usize) {
        for entry in self.source_slot[..self.source_slots_used].iter_mut() {
            if entry.0 == source {
                entry.1 = slot;
                return;
            }
        }
        if self.source_slots_used < MAX_SOURCES {
            self.source_slot[self.source_slots_used] = (source, slot);
            self.source_slots_used += 1;
        }
    }

    /// Which slot a source renders from; slot 0 if it was never bound.
    ///
    /// Falling back rather than refusing: a source whose binding was lost should
    /// show *something*, and slot 0 is the show. A device is never dark because
    /// of software.
    fn slot_of(&self, source: Uuid) -> usize {
        self.source_slot[..self.source_slots_used]
            .iter()
            .find(|(id, _)| *id == source)
            .map_or(0, |(_, slot)| *slot)
    }

    /// Show time, which the caller already holds.
    ///
    /// This used to add an offset learned from `Tick`. It no longer does: the
    /// mesh owns the clock now, disciplined by election and sync in
    /// `lumen_device::node::Node`, and a second authority applying its own
    /// correction on top would be two clocks fighting over one show.
    pub fn show_time(&self, now_us: u64) -> u64 {
        now_us
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
            // `Tick` is not handled here. It is the mesh's, and a device that
            // also took a clock from it would have two.

            Payload::ProgBegin(begin) => {
                if begin.total_len as usize > MAX_PROGRAM {
                    return Handled::ProgramRejected;
                }
                // `slot` is the controller's choice, clamped rather than
                // refused: a device with two slots asked for a third should
                // overwrite something rather than ignore the program and leave
                // the sender believing it landed.
                let slot = (begin.slot as usize).min(SLOTS - 1);
                self.expected_len = begin.total_len as usize;
                self.slots[slot].clear();
                self.slots[slot].resize(MAX_PROGRAM, 0);
                self.slot_len[slot] = 0;
                self.filling = Some(slot);
                Handled::ProgramStarted {
                    len: begin.total_len,
                }
            }

            Payload::ProgChunk(chunk) => {
                let Some(slot) = self.filling else {
                    return Handled::Ignored;
                };
                let at = chunk.offset as usize;
                let end = at + chunk.data.len();
                if end > self.expected_len || end > MAX_PROGRAM {
                    // A chunk outside the length the sender declared is either a
                    // corrupt transfer or a different program's chunk arriving
                    // late. Either way, writing it would scribble on the buffer.
                    self.filling = None;
                    return Handled::ProgramRejected;
                }
                self.slots[slot][at..end].copy_from_slice(chunk.data);
                self.slot_len[slot] = self.slot_len[slot].max(end);
                Handled::ProgramChunk {
                    at: chunk.offset,
                    len: chunk.data.len(),
                }
            }

            Payload::ProgEnd(_) => {
                let Some(slot) = self.filling.take() else {
                    return Handled::ProgramRejected;
                };
                if self.slot_len[slot] != self.expected_len {
                    self.slot_len[slot] = 0;
                    return Handled::ProgramRejected;
                }
                // Parsed once, here, rather than every frame. The answer cannot
                // change, and re-parsing at 30 Hz is a measurable slice of a
                // frame spent proving something already known.
                match Program::parse(&self.slots[slot][..self.slot_len[slot]]) {
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
                        self.last_filled = slot;
                        Handled::ProgramComplete {
                            len: self.slot_len[slot],
                            budget: p.budget,
                            channels: p.channel_count(),
                        }
                    }
                    Err(_) => {
                        self.slot_len[slot] = 0;
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
                    zone: push.zone_id,
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
                        self.bind(push.source_id, self.last_filled);
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
    pub fn render(&mut self, now_us: u64, out: &mut [u8]) -> Option<(u32, Encoded, Rendered)> {
        if self.program_bytes() == 0 || self.stack.active().is_empty() {
            return None;
        }
        // Quantised to the frame grid. Two synchronised nodes never render on
        // the same microsecond, and rendering at whatever moment each happened
        // to wake would make identical clocks produce different frames - which
        // is exactly the disagreement everything else is arranged to prevent.
        let show_us = (self.show_time(now_us) / FRAME_US) * FRAME_US;
        let t = Q16::from_micros(show_us);

        // Fields taken apart so the borrow checker can see they are disjoint:
        // `bound` holds references into the zones and the program slots while
        // the renderer and the frame buffer are borrowed mutably, and through
        // `self` those are one borrow.
        let Node {
            leds,
            zones,
            stack,
            renderer,
            channels,
            slots,
            slot_len,
            source_slot,
            source_slots_used,
            frame,
            output,
            ..
        } = self;

        // Every admitted source, each with the program it was pushed against.
        //
        // One source was enough to light a strip and could never show what the
        // stack is *for*: an alert over a show, resolved per pixel, with the
        // higher priority winning and the lower one still there underneath when
        // it expires. The render loop has always done this; a device holding one
        // program simply never asked it to.
        let mut programs: [Option<Program<'_>>; MAX_SOURCES] = [const { None }; MAX_SOURCES];
        let mut bound: Vec<Bound<'_>> = Vec::new();
        let active: Vec<Source> = stack.active().to_vec();
        let slot_of = |id: Uuid| -> usize {
            source_slot[..*source_slots_used]
                .iter()
                .find(|(s, _)| *s == id)
                .map_or(0, |(_, slot)| *slot)
        };
        for (i, source) in active.iter().enumerate().take(MAX_SOURCES) {
            let slot = slot_of(source.id);
            if slot_len[slot] == 0 {
                continue;
            }
            programs[i] = Program::parse(&slots[slot][..slot_len[slot]]).ok();
        }
        for (i, source) in active.iter().enumerate().take(MAX_SOURCES) {
            if let Some(program) = &programs[i] {
                // The zone the source named, so `u` is relative to that zone's
                // LEDs. A source targeting a zone this device is not in
                // contributes nothing rather than being stretched across
                // everything - which is how one push reaches a room without
                // every device having to be told about it separately.
                let found = zones
                    .iter()
                    .find(|(zone, _)| zone.id == source.zone)
                    .unwrap_or(&zones[0]);
                let (zone, membership) = (&found.0, &found.1);
                if membership.is_empty() {
                    continue;
                }
                bound.push(Bound {
                    source: *source,
                    program,
                    membership,
                    projection: zone.projection,
                });
            }
        }
        if bound.is_empty() {
            return None;
        }

        // Channels are read through the program that declares them. With several
        // sources the first one's table is used, which is right while every
        // effect on this device reads the same channels and is a known
        // simplification the moment two of them do not.
        let first = bound[0].program;
        let mut uniforms = ChannelUniforms::new(channels, first, show_us);
        let report = renderer.render_shard(
            show_us,
            t,
            leds,
            stack,
            &bound,
            &mut uniforms,
            frame,
            Shard::whole(leds.leds.len() as u16),
        );

        // Linear light through the output stage, which is where brightness, the
        // power budget and the dithering live. Writing `(v * 255) >> 16` here
        // instead - which is what this did - throws away everything below one
        // code and lets the strip ask the supply for more than it has.
        let mut linear = [Q16::ZERO; MAX_CHANNELS];
        let n = (frame.len() * 3).min(MAX_CHANNELS);
        for (i, px) in frame.iter().enumerate() {
            if i * 3 + 2 >= n {
                break;
            }
            linear[i * 3] = px.r;
            linear[i * 3 + 1] = px.g;
            linear[i * 3 + 2] = px.b;
        }
        // The dither's phase comes from show time, so every device in the mesh
        // dithers this frame the same way. A local frame counter would work on
        // one device and make two of them disagree at the dark end.
        let phase = (show_us / 33_333) as u32;
        let encoded = output.encode(&linear[..n], phase, out);

        // A fingerprint of what the VM produced, hashed before the output stage
        // so it is the *render* being compared rather than this device's supply
        // and brightness. Two devices with different power budgets must still
        // agree here; that is the claim the whole architecture rests on.
        Some((
            report.spent,
            encoded,
            Rendered {
                digest: Digest::of_frame(&linear[..n]),
                show_us,
            },
        ))
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

/// What one frame came out as, for comparing against another renderer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rendered {
    /// Fingerprint of the linear frame.
    pub digest: u64,
    /// The show time it was rendered for. Meaningless without this: two
    /// devices agreeing on a hash for different moments have proved nothing.
    pub show_us: u64,
}

/// The frame grid every device in the mesh renders on.
///
/// 30 fps, a whole number of microseconds so the grid does not drift, and a
/// divisor of the mesh's 120 Hz timing grid so mixed-rate devices stay in phase
/// rather than beating against each other.
pub const FRAME_US: u64 = 33_333;

/// Channels the linear staging buffer holds. 300 LEDs, the size the project
/// sizes against, so the same firmware drives a longer strip unchanged.
const MAX_CHANNELS: usize = 900;
