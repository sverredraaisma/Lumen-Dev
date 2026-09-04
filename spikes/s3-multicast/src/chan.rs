//! The `CHAN` flood, and what a receiver makes of it.
//!
//! One device sends a `CHAN`-shaped datagram to a multicast group sixty times a
//! second. Every other device joins the group and counts what arrives. That is
//! the whole spike: the architecture broadcasts *values* rather than pixels, and
//! it only works if a value sent once reaches every device that needs it.
//!
//! # What is being measured
//!
//! **Loss**, from gaps in `producer_seq` — the field the wire format already
//! carries for latest-wins ordering, which doubles as a sequence number for
//! exactly this. And **jitter**, as the spread of arrival intervals around the
//! 16 667 µs the sender aims for.
//!
//! Jitter matters more than average latency and is the thing a mean would hide.
//! A channel that is reliably 40 ms late is a channel a device can compensate
//! for; one that is 2 ms late and occasionally 60 ms late cannot be compensated
//! for at all, because a receiver never knows which kind of packet it is holding.

use esp_println::println;

use crate::stats::Histogram;

/// Message type, matching the wire format so a capture reads the same.
pub const CHAN: u8 = 0x21;

/// The datagram: type, channel id, producer sequence, and a send timestamp.
///
/// The timestamp is not in the real `CHAN` payload; it is here so a receiver can
/// see one-way delay as well as arrival spacing. Without it, a burst that
/// arrives late but evenly looks identical to one that arrives on time.
pub const LEN: usize = 1 + 2 + 4 + 8;

/// How often the sender sends. Sixty a second is what a show runs at, and the
/// whole question is whether the network carries it.
pub const INTERVAL_US: u64 = 16_667;

pub fn encode(channel: u16, seq: u32, now_us: u64) -> [u8; LEN] {
    let mut out = [0u8; LEN];
    out[0] = CHAN;
    out[1..3].copy_from_slice(&channel.to_le_bytes());
    out[3..7].copy_from_slice(&seq.to_le_bytes());
    out[7..15].copy_from_slice(&now_us.to_le_bytes());
    out
}

/// `(channel, seq, sent_us)`, or `None` if this is not one of ours.
pub fn decode(bytes: &[u8]) -> Option<(u16, u32, u64)> {
    if bytes.len() < LEN || bytes[0] != CHAN {
        return None;
    }
    let channel = u16::from_le_bytes([bytes[1], bytes[2]]);
    let seq = u32::from_le_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
    let mut t = [0u8; 8];
    t.copy_from_slice(&bytes[7..15]);
    Some((channel, seq, u64::from_le_bytes(t)))
}

/// What one receiver has seen.
pub struct Sink {
    /// Highest sequence seen, so gaps can be counted without holding a window.
    highest: Option<u32>,
    received: u32,
    /// Sequences that never arrived, counted from the gaps.
    missing: u32,
    /// Arrivals that came after a later one. Separate from loss: a packet that
    /// arrives out of order was not lost, and counting it as loss would blame
    /// the network for something the receiver can simply sort.
    reordered: u32,
    last_arrival_us: Option<u64>,
    /// Spacing between arrivals, which is what tears a show when it varies.
    gaps: Histogram,
    /// The longest silence, because one 200 ms hole is a visible freeze however
    /// good the average is.
    worst_gap_us: u64,
}

impl Default for Sink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink {
    pub const fn new() -> Sink {
        Sink {
            highest: None,
            received: 0,
            missing: 0,
            reordered: 0,
            last_arrival_us: None,
            gaps: Histogram::new(),
            worst_gap_us: 0,
        }
    }

    pub fn received(&self) -> u32 {
        self.received
    }

    pub fn on_datagram(&mut self, now_us: u64, bytes: &[u8]) {
        let Some((_, seq, _sent)) = decode(bytes) else {
            return;
        };
        self.received += 1;

        match self.highest {
            None => self.highest = Some(seq),
            Some(highest) if seq > highest => {
                // Everything between the last one and this one never arrived.
                self.missing += seq - highest - 1;
                self.highest = Some(seq);
            }
            Some(_) => {
                // Older than something already seen. It was late, not lost, and
                // one of the two it was counted against comes back.
                self.reordered += 1;
                self.missing = self.missing.saturating_sub(1);
            }
        }

        if let Some(last) = self.last_arrival_us {
            let gap = now_us.saturating_sub(last);
            self.gaps.add(gap as i64);
            self.worst_gap_us = self.worst_gap_us.max(gap);
        }
        self.last_arrival_us = Some(now_us);
    }

    /// Everything the sender sent that this receiver should have seen.
    fn expected(&self) -> u32 {
        match self.highest {
            // Sequences start at zero, so the highest seen is one less than the
            // count sent - measured from the first arrival rather than from the
            // sender's start, since a receiver that joined late did not lose
            // what it was never sent.
            Some(_) => self.received + self.missing,
            None => 0,
        }
    }

    /// Start a fresh window, keeping the sequence position.
    ///
    /// Reported per window rather than since boot, because a cumulative figure
    /// never recovers from a bad start: the source's transmit queue backs up
    /// while sinks are still associating, every one of those is a sequence
    /// number nobody receives, and an average over the whole run reports that
    /// startup for ever. Steady state is the thing being measured.
    pub fn begin_window(&mut self) {
        self.received = 0;
        self.missing = 0;
        self.reordered = 0;
        self.gaps = Histogram::new();
        self.worst_gap_us = 0;
    }

    pub fn report(&self) {
        let expected = self.expected();
        if expected == 0 {
            println!("nothing received yet");
            return;
        }
        // Per ten thousand, so a tenth of a percent is visible without floats.
        let loss = self.missing as u64 * 10_000 / expected as u64;
        println!(
            "n={} lost={} ({}.{:02}%) reordered={}",
            self.received,
            self.missing,
            loss / 100,
            loss % 100,
            self.reordered
        );
        println!(
            "   gap p50={}us p95={}us p99={}us worst={}us",
            self.gaps.percentile(50),
            self.gaps.percentile(95),
            self.gaps.percentile(99),
            self.worst_gap_us
        );
        // A frame is what the criterion is stated in: a gap longer than one is
        // a frame this device rendered with stale data.
        let over_frame = self.gaps.count_above(INTERVAL_US as i64 * 2);
        println!(
            "   => loss {}.{:02}% ({}), {} gaps over two frames",
            loss / 100,
            loss % 100,
            if loss < 100 { "within 1%" } else { "OVER 1%" },
            over_frame
        );
    }
}
