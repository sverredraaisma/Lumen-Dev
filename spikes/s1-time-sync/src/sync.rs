//! The sync exchange itself, and the roles either side of it.
//!
//! Deliberately the same shape as the wire format's `TICK` / `SYNC_REQ` /
//! `SYNC_RESP`, minus the parts a spike cannot exercise: no election, no epoch,
//! no wall clock. What is here is the four-timestamp exchange and the filter,
//! because those are what the ±500 µs claim rests on.
//!
//! Kept free of the network so it can be reasoned about on its own. Everything
//! below takes a timestamp and some bytes and returns bytes.

use esp_println::println;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};


use crate::stats::Samples;
use crate::{PORT, REQUEST_INTERVAL_MS};

/// Message type bytes, matching the wire format so the traces read the same.
const TICK: u8 = 0x10;
const SYNC_REQ: u8 = 0x11;
const SYNC_RESP: u8 = 0x12;

/// A `TICK` is one byte of type and nothing else here: the spike's follower only
/// needs the sender's address, which the datagram already carries.
const TICK_LEN: usize = 1;

/// `SYNC_REQ` is the type byte and `t1`.
const REQ_LEN: usize = 1 + 8;

/// `SYNC_RESP` is the type byte, the echoed `t1`, then `t2` and `t3`.
const RESP_LEN: usize = 1 + 8 * 3;

/// How often the master announces itself.
const TICK_INTERVAL_US: u64 = 1_000_000;

/// How many samples between reports. At 200 ms a report is every 20 seconds,
/// which is often enough to watch and rare enough not to drown the log.
const REPORT_EVERY: usize = 100;

pub enum State {
    /// Answers requests, and announces itself so a follower needs no address.
    Master {
        next_tick_us: u64,
        /// Requests answered. A master that ticks but never answers is a
        /// different fault from one that is not running, and on a consumer AP
        /// the difference is usually client isolation: broadcast is flooded
        /// while unicast between two stations is dropped.
        answered: u32,
    },
    /// Asks, filters, and accumulates.
    Follower {
        master: Option<IpEndpoint>,
        samples: Samples,
        /// The show clock's offset as last estimated, for reporting drift.
        first_offset: Option<(u64, i64)>,
        /// Requests sent and replies seen, so silence can be attributed.
        sent: u32,
        heard: u32,
    },
}

impl State {
    pub fn new(role: &str) -> Self {
        if role == "master" {
            State::Master {
                next_tick_us: 0,
                answered: 0,
            }
        } else {
            State::Follower {
                master: None,
                samples: Samples::new(),
                first_offset: None,
                sent: 0,
                heard: 0,
            }
        }
    }

    /// How long to wait before the next scheduled send.
    pub fn interval_us(&self) -> u64 {
        match self {
            State::Master { .. } => TICK_INTERVAL_US,
            State::Follower { .. } => REQUEST_INTERVAL_MS * 1000,
        }
    }

    /// The scheduled send: a `TICK` from a master, a `SYNC_REQ` from a follower
    /// that knows where to send one.
    pub fn on_tick(&mut self, now: u64) -> Option<(IpEndpoint, Datagram)> {
        match self {
            State::Master {
                next_tick_us,
                answered,
            } => {
                *next_tick_us = now + TICK_INTERVAL_US;
                println!("tick; answered {answered} requests so far");
                let mut out = Datagram::new();
                out.push(TICK);
                Some((broadcast(), out))
            }
            State::Follower {
                master, sent, heard, ..
            } => {
                let to = (*master)?;
                *sent += 1;
                if *sent % 25 == 0 {
                    println!("sent {sent} requests, heard {heard} replies");
                }
                let mut out = Datagram::new();
                out.push(SYNC_REQ);
                // `t1` is taken as late as possible, immediately before handing
                // the bytes to the stack. Every microsecond between here and the
                // wire is error that lands in the offset.
                out.push_u64(now_at_send());
                Some((to, out))
            }
        }
    }

    /// A datagram arrived. Returns a reply to send, if this message wants one.
    pub fn on_datagram(
        &mut self,
        t: u64,
        bytes: &[u8],
        from: IpEndpoint,
    ) -> Option<(IpEndpoint, Datagram)> {
        let kind = *bytes.first()?;
        match (&mut *self, kind) {
            // A master answers. `t2` is arrival and `t3` is departure, and the
            // gap between them is the master's own processing, which the
            // requester subtracts rather than mistaking for network time.
            (State::Master { answered, .. }, SYNC_REQ) if bytes.len() >= REQ_LEN => {
                *answered += 1;
                let t1 = u64_at(bytes, 1)?;
                let mut out = Datagram::new();
                out.push(SYNC_RESP);
                out.push_u64(t1);
                out.push_u64(t);
                out.push_u64(now_at_send());
                Some((
                    IpEndpoint {
                        addr: from.addr,
                        port: PORT,
                    },
                    out,
                ))
            }

            // A follower learns where the master is from the announcement, and
            // needs nothing else configured.
            (State::Follower { master, .. }, TICK) if bytes.len() >= TICK_LEN => {
                if master.is_none() {
                    println!("== master found at {}", from.addr);
                    *master = Some(IpEndpoint {
                        addr: from.addr,
                        port: PORT,
                    });
                }
                None
            }

            (
                State::Follower {
                    samples,
                    first_offset,
                    heard,
                    ..
                },
                SYNC_RESP,
            ) if bytes.len() >= RESP_LEN => {
                *heard += 1;
                let t1 = u64_at(bytes, 1)? as i64;
                let t2 = u64_at(bytes, 9)? as i64;
                let t3 = u64_at(bytes, 17)? as i64;
                let t4 = t as i64;

                // The wire format's arithmetic, unchanged.
                let offset = ((t2 - t1) + (t3 - t4)) / 2;
                let rtt = (t4 - t1) - (t3 - t2);

                // A negative round trip means the two clocks moved between the
                // timestamps in a way the arithmetic cannot express — a counter
                // wrap, or a reply matched to the wrong request. Never a sample.
                if rtt < 0 {
                    return None;
                }

                if first_offset.is_none() {
                    *first_offset = Some((t, offset));
                }
                samples.add(t, rtt, offset);

                // Triggered on samples taken, not samples kept. The filter can
                // reject most of them, and a report that waits for a hundred
                // survivors would go quiet exactly when the network is worth
                // reporting on.
                if samples.total() % REPORT_EVERY == 0 && samples.total() > 0 {
                    samples.report(t);
                }
                None
            }

            _ => None,
        }
    }
}

/// The timestamp to put in a message, taken as late as possible.
///
/// A separate function purely so the call site reads as a deliberate act rather
/// than an incidental one: moving this earlier in a function would quietly add
/// its own execution time to every measurement taken with it.
fn now_at_send() -> u64 {
    esp_hal::time::now().duration_since_epoch().to_micros()
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at + 8)?;
    let mut v = [0u8; 8];
    v.copy_from_slice(slice);
    Some(u64::from_le_bytes(v))
}

/// A small fixed buffer, since nothing here is longer than a `SYNC_RESP`.
pub struct Datagram {
    bytes: [u8; RESP_LEN],
    len: usize,
}

impl Datagram {
    fn new() -> Self {
        Datagram {
            bytes: [0; RESP_LEN],
            len: 0,
        }
    }

    fn push(&mut self, b: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = b;
            self.len += 1;
        }
    }

    fn push_u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.push(b);
        }
    }
}

impl core::ops::Deref for Datagram {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The broadcast address a master announces on.
pub const fn broadcast() -> IpEndpoint {
    IpEndpoint {
        addr: IpAddress::Ipv4(Ipv4Address::new(255, 255, 255, 255)),
        port: PORT,
    }
}
