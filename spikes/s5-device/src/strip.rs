//! Driving an addressable strip from the C3's RMT peripheral.
//!
//! WS2812, SK6812 and friends all speak the same one-wire protocol: a bit is a
//! pulse whose *high* time says whether it is a one or a zero, and a long low
//! period latches the frame. RMT exists to emit exactly that — a list of
//! (level, duration) pairs, clocked in hardware — so the CPU builds a buffer
//! once per frame and the peripheral does the timing. Bit-banging would work
//! and would then break the moment WiFi took an interrupt mid-frame.
//!
//! # Colour order and the white channel
//!
//! These parts take **GRB**, not RGB. Getting it wrong does not fail, it swaps
//! red and green, which looks like a bad effect rather than a bad driver.
//!
//! `SK6812` is two different parts sold under one number. The plain one is
//! 24-bit GRB and is WS2812-compatible; the RGBW one is 32-bit GRBW and takes a
//! fourth byte per LED. Feed 24-bit data to an RGBW strip and it lights three
//! quarters of the strip in the wrong colours, because every LED consumes 32
//! bits regardless and simply takes them from wherever the stream has got to.
//! [`Format`] chooses, and the self-test is built to make the difference obvious
//! rather than subtle.

use esp_hal::gpio::interconnect::PeripheralOutput;
use esp_hal::peripheral::Peripheral;
use esp_hal::rmt::{Channel, Error, PulseCode, TxChannel, TxChannelConfig, TxChannelCreator};
use esp_hal::Blocking;

/// What one LED consumes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// 24-bit GRB. WS2812, WS2812B, plain SK6812.
    Grb,
    /// 32-bit GRBW. SK6812 RGBW, where the fourth byte is a dedicated white.
    Grbw,
}

impl Format {
    pub fn bytes_per_led(self) -> usize {
        match self {
            Format::Grb => 3,
            Format::Grbw => 4,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Grb => "GRB (24-bit)",
            Format::Grbw => "GRBW (32-bit)",
        }
    }
}

/// The RMT clock this driver configures, in MHz. One tick is 12.5 ns.
pub const CLOCK_MHZ: u32 = 80;

/// Pulse widths in RMT ticks at 80 MHz.
///
/// The datasheets quote 0.3/0.9 µs for a zero and 0.6/0.6 for a one, each
/// ±150 ns. These sit in the middle of those windows rather than at an edge,
/// because the tolerance is what absorbs a strip's own variation and there is
/// nothing to gain from spending it here.
const T0H: u16 = 26; // 325 ns
const T0L: u16 = 70; // 875 ns
const T1H: u16 = 52; // 650 ns
const T1L: u16 = 44; // 550 ns

/// Longest strip this driver will drive in one transaction.
///
/// 300 is the strip the whole project sizes against. The buffer is one `u32`
/// per bit plus a terminator, so at GRBW that is 300 × 32 + 1 words — 38 KB,
/// which is why it is a caller-provided buffer rather than something this
/// module allocates.
pub const MAX_LEDS: usize = 300;

/// Words a buffer needs for `leds` LEDs in `format`.
pub const fn buffer_words(leds: usize, format_bytes: usize) -> usize {
    leds * format_bytes * 8 + 1
}

/// An addressable strip on one pin.
pub struct Strip<C: TxChannel> {
    channel: Option<C>,
    format: Format,
}

impl<const N: u8> Strip<Channel<Blocking, N>>
where
    Channel<Blocking, N>: TxChannel,
{
    /// Take an RMT channel and a pin.
    ///
    /// The channel is configured for a strip and nothing else: no carrier, idle
    /// low, and no clock division — the divider would cost resolution the pulse
    /// widths above are already spending carefully.
    pub fn new<'d, P: PeripheralOutput>(
        creator: impl TxChannelCreator<'d, Channel<Blocking, N>, P>,
        pin: impl Peripheral<P = P> + 'd,
        format: Format,
    ) -> Result<Self, Error> {
        let config = TxChannelConfig {
            clk_divider: 1,
            idle_output_level: false,
            idle_output: true,
            carrier_modulation: false,
            carrier_high: 0,
            carrier_low: 0,
            carrier_level: false,
        };
        Ok(Strip {
            channel: Some(creator.configure(pin, config)?),
            format,
        })
    }
}

impl<C: TxChannel> Strip<C> {
    pub fn format(&self) -> Format {
        self.format
    }

    /// Write `pixels` — three bytes per LED, in R, G, B order — to the strip.
    ///
    /// `scratch` must hold [`buffer_words`] for this many LEDs. Passed in rather
    /// than owned because at 300 LEDs it is 38 KB, and a device this size should
    /// be able to see where that went.
    ///
    /// The white byte on an RGBW strip is left at zero: white is mixed from the
    /// three colour channels, exactly as every other device in the mesh mixes
    /// it. Driving the dedicated white LED instead would make this strip a
    /// different colour from its neighbours for the same program, which is the
    /// one thing a mesh cannot have. A future `Projection`-style decision could
    /// route it, but it is a colour decision and does not belong in a driver.
    pub fn write(&mut self, pixels: &[u8], scratch: &mut [u32]) -> Result<(), Error> {
        let per_led = self.format.bytes_per_led();
        let leds = pixels.len() / 3;
        let words = buffer_words(leds, per_led);
        if scratch.len() < words {
            return Err(Error::Overflow);
        }

        let mut at = 0;
        for led in 0..leds {
            let (r, g, b) = (pixels[led * 3], pixels[led * 3 + 1], pixels[led * 3 + 2]);
            // GRB, which is what the part expects and not what anyone assumes.
            let bytes = [g, r, b, 0u8];
            for byte in &bytes[..per_led] {
                for bit in (0..8).rev() {
                    scratch[at] = if byte & (1 << bit) != 0 {
                        PulseCode::new(true, T1H, false, T1L)
                    } else {
                        PulseCode::new(true, T0H, false, T0L)
                    };
                    at += 1;
                }
            }
        }
        // An empty code ends the transmission. Without it the peripheral runs
        // on into whatever the buffer held last frame.
        scratch[at] = PulseCode::empty();
        at += 1;

        let channel = self.channel.take().expect("a channel to transmit on");
        // Interrupts held off for the whole frame.
        //
        // RMT holds 48 words of channel RAM and the CPU refills half of it every
        // 24 words - about forty times for a 30-LED RGBW frame, each with a
        // 30 µs deadline. A WiFi interrupt that overruns one of those deadlines
        // leaves the line idle mid-frame, the strip takes the gap for a latch,
        // and everything after it lands in the wrong LED. That is exactly what
        // it looks like: a stable pattern with random pixels flashing through
        // it, which appeared the moment the radio was switched on and was
        // nowhere to be seen in the strip-only self-test.
        //
        // The cost is real: a 30-LED frame is 1.2 ms, so at 30 fps this holds
        // interrupts off about 3.6% of the time, and it scales with the strip -
        // 300 LEDs would be 12 ms, which is most of a frame and not acceptable.
        // The proper fix is DMA, and it is the first thing in S5's follow-ups.
        match critical_section::with(|_| {
            channel.transmit(&scratch[..at]).and_then(|t| {
            t.wait().map_err(|(e, c)| {
                // Keep the channel even when the transmission failed: dropping
                // it here would make one bad frame the end of all output, and
                // "a device is never dark because of software" covers the
                // driver too.
                self.channel = Some(c);
                e
            })
            })
        }) {
            Ok(c) => {
                self.channel = Some(c);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
