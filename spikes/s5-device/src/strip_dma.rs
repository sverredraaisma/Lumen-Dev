//! Driving the strip from SPI with DMA, so the CPU is not in the loop.
//!
//! The RMT driver in `strip.rs` works and does not scale. RMT holds 48 words of
//! channel memory and the CPU refills half of it every 24 words — about forty
//! times for a 30-LED RGBW frame, each with a 30 µs deadline. A WiFi interrupt
//! that overruns one leaves the line idle mid-frame, the strip reads the gap as
//! a latch, and everything after it lands in the wrong LED. Holding interrupts
//! off for the frame fixes it and costs 1.2 ms at 30 LEDs — tolerable — but
//! 12 ms at 300, which is most of a frame.
//!
//! DMA removes the deadline instead of racing it. The whole frame is written
//! once into a buffer the DMA engine owns, and the peripheral clocks it out with
//! no further help; an interrupt during transmission is simply irrelevant.
//!
//! # Why SPI rather than RMT
//!
//! esp-hal 0.23's RMT driver has no DMA support. SPI does, and a shift register
//! clocking fixed-width bits is a perfectly good pulse generator if the bit
//! pattern is chosen to match.
//!
//! # The one ratio that works
//!
//! Each LED bit becomes four SPI bits at 3.2 MHz, so one SPI bit is 312.5 ns and
//! an LED bit is 1.25 µs — which is the period the part wants.
//!
//! | | pattern | high time | the part allows |
//! |---|---|---|---|
//! | zero | `1000` | 312 ns | 150–450 ns |
//! | one | `1100` | 625 ns | 450–750 ns |
//!
//! Three bits at 2.4 MHz is the more commonly seen choice and does not work
//! here: it puts a one's high time at 833 ns, outside what an SK6812 accepts.
//! The arithmetic is worth doing rather than copying, because a strip fed
//! out-of-spec pulses does not fail — it works on the bench and misbehaves on
//! the twentieth LED of a long run.
//!
//! # What it costs
//!
//! Sixteen bytes of DMA buffer per RGBW LED, against zero for the RMT path: 480
//! bytes for thirty LEDs, 4.8 KB for three hundred. That is the trade — memory
//! for not having a deadline.

use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
use esp_hal::gpio::interconnect::PeripheralOutput;
use esp_hal::peripheral::Peripheral;
use esp_hal::spi::master::{Config, Spi, SpiDmaBus};
use esp_hal::spi::Mode;
use esp_hal::time::RateExtU32;
use esp_hal::Blocking;

use crate::strip::Format;

/// SPI bits per LED bit. See the table above.
const EXPANSION: usize = 4;

/// The clock that makes [`EXPANSION`] land inside the part's timing windows.
pub const CLOCK_HZ: u32 = 3_200_000;

/// Two LED bits per byte, so one LED byte becomes four.
const BYTES_PER_LED_BYTE: usize = 8 * EXPANSION / 8;

/// Bytes of DMA buffer for `leds` LEDs in a format taking `per_led` bytes.
pub const fn buffer_bytes(leds: usize, per_led: usize) -> usize {
    leds * per_led * BYTES_PER_LED_BYTE
}

/// The four-bit patterns, packed two LED bits to a byte.
///
/// A lookup rather than shifts in the inner loop: this runs over every bit of
/// every LED of every frame, and the table is sixteen bytes.
const PAIRS: [u8; 4] = [
    0b1000_1000, // 0, 0
    0b1000_1100, // 0, 1
    0b1100_1000, // 1, 0
    0b1100_1100, // 1, 1
];

/// An addressable strip on one pin, clocked by DMA.
pub struct DmaStrip<'d> {
    bus: SpiDmaBus<'d, Blocking>,
    format: Format,
}

impl<'d> DmaStrip<'d> {
    /// Take SPI, a DMA channel and a pin.
    ///
    /// Only MOSI is connected: there is nothing to read back from a strip, and
    /// leaving the clock unrouted keeps it off a pin somebody may be using.
    pub fn new<MOSI: PeripheralOutput>(
        spi: impl Peripheral<P = esp_hal::peripherals::SPI2> + 'd,
        channel: impl Peripheral<P = esp_hal::dma::DmaChannel0> + 'd,
        mosi: impl Peripheral<P = MOSI> + 'd,
        format: Format,
        rx: DmaRxBuf,
        tx: DmaTxBuf,
    ) -> Result<Self, esp_hal::spi::master::ConfigError> {
        let config = Config::default()
            .with_frequency(CLOCK_HZ.Hz())
            // Mode 0: idle low, sampled on the rising edge. Idle low matters -
            // the line resting high between frames would look to the strip like
            // a very long pulse rather than the latch it needs.
            .with_mode(Mode::_0);
        let spi = Spi::new(spi, config)?.with_mosi(mosi).with_dma(channel);
        Ok(DmaStrip {
            bus: spi.with_buffers(rx, tx),
            format,
        })
    }

    /// Write `pixels` — three bytes per LED in R, G, B — to the strip.
    ///
    /// `scratch` must hold [`buffer_bytes`] for this many LEDs and is the buffer
    /// the DMA engine reads, so it must outlive the transfer. The call blocks
    /// until the frame has gone out; what it does *not* do is need the CPU
    /// during it, which is the whole point.
    pub fn write(&mut self, pixels: &[u8], scratch: &mut [u8]) -> Result<(), ()> {
        let per_led = self.format.bytes_per_led();
        let leds = pixels.len() / 3;
        let needed = buffer_bytes(leds, per_led);
        if scratch.len() < needed {
            return Err(());
        }

        let mut at = 0;
        for led in 0..leds {
            let (r, g, b) = (pixels[led * 3], pixels[led * 3 + 1], pixels[led * 3 + 2]);
            // GRB, which is what the part expects and not what anyone assumes.
            // The white byte stays zero: white is mixed from the colour dies so
            // that this strip agrees with its neighbours about what a colour is.
            let bytes = [g, r, b, 0u8];
            for byte in &bytes[..per_led] {
                for pair in (0..4).rev() {
                    let two = (byte >> (pair * 2)) & 0b11;
                    scratch[at] = PAIRS[two as usize];
                    at += 1;
                }
            }
        }

        // A frame that fails to go out leaves the strip showing the last one,
        // which is the right failure: "a device is never dark because of
        // software" covers the driver too.
        self.bus.write(&scratch[..at]).map_err(|_| ())
    }
}

/// DMA buffers sized for a strip.
///
/// The receive side is a single byte because SPI is bidirectional and the driver
/// wants both, while a strip has nothing to say back. Zero is not allowed.
#[macro_export]
macro_rules! strip_dma_buffers {
    ($bytes:expr) => {{
        let (rx, rx_d, tx, tx_d) = esp_hal::dma_buffers!(1, $bytes);
        (
            esp_hal::dma::DmaRxBuf::new(rx_d, rx).expect("rx descriptors"),
            esp_hal::dma::DmaTxBuf::new(tx_d, tx).expect("tx descriptors"),
        )
    }};
}

#[allow(unused_imports)]
use dma_buffers as _;
