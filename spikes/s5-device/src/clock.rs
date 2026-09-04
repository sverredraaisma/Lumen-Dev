//! The show clock, and the one rule about it: it never steps.
//!
//! Time sync produces a correction, and the obvious thing to do with a
//! correction is add it. That is wrong here, and visibly so: every effect is a
//! function of this clock, so a step means a frame is rendered twice or skipped
//! — a stutter on every device that just resynchronised, which is all of them,
//! once every thirty seconds.
//!
//! So a correction is **slewed**: the clock runs slightly fast or slightly slow
//! until it has absorbed the offset, and no frame is ever anywhere but where the
//! frame before it implied. `lumen-device` says this in
//! `Action::DisciplineClock`'s own documentation; this is a device honouring it.
//!
//! # Why the rate is what it is
//!
//! 200 parts per million: 200 µs of correction per second of wall time. Fast
//! enough to absorb the offsets S1 measured — p50 225-350 µs, p95 675-1500 —
//! inside a few seconds, and slow enough that the resulting rate error is far
//! below anything an eye resolves. A crystal's own drift is around 33 ppm, so
//! this is roughly six times the error the hardware already has and nobody
//! notices that either.

/// Parts per million the clock may run fast or slow while correcting.
const SLEW_PPM: i64 = 200;

/// A monotonic microsecond clock that can be disciplined without jumping.
pub struct ShowClock {
    /// Hardware microseconds at the last advance.
    last_raw_us: u64,
    /// What this clock reads now.
    now_us: u64,
    /// Correction still to be worked off, signed.
    pending_us: i64,
}

impl ShowClock {
    /// Start at `raw_us`, reading zero.
    ///
    /// Show time counts from boot rather than from an epoch: the VM reads it as
    /// Q16.16 seconds, which holds about nine hours, and a wall-clock timestamp
    /// saturates that immediately. What a mesh agrees on is an *elapsed* count.
    pub const fn new(raw_us: u64) -> ShowClock {
        ShowClock {
            last_raw_us: raw_us,
            now_us: 0,
            pending_us: 0,
        }
    }

    /// Advance to hardware time `raw_us` and return the show time.
    ///
    /// Monotonic even if the hardware clock goes backwards, which it should not
    /// and which a saturating subtraction makes harmless rather than
    /// catastrophic.
    pub fn advance_to(&mut self, raw_us: u64) -> u64 {
        let elapsed = raw_us.saturating_sub(self.last_raw_us);
        self.last_raw_us = raw_us;

        // The correction this interval may absorb, at the slew rate. Integrated
        // over the interval rather than recomputed from the total, because drift
        // is a rate error: a clock corrected by "half of what is left" every
        // time never actually arrives.
        let budget = (elapsed as i64).saturating_mul(SLEW_PPM) / 1_000_000;
        let applied = self.pending_us.clamp(-budget, budget);
        self.pending_us -= applied;

        self.now_us = self
            .now_us
            .saturating_add_signed(elapsed as i64 + applied)
            .max(self.now_us);
        self.now_us
    }

    pub const fn now_us(&self) -> u64 {
        self.now_us
    }

    /// Take on a correction, to be worked off gradually.
    ///
    /// Accumulates rather than replaces: a second correction arriving before the
    /// first has been absorbed means both were measured against a clock that was
    /// wrong, and dropping either would leave the error in.
    pub fn discipline(&mut self, offset_us: i64) {
        self.pending_us = self.pending_us.saturating_add(offset_us);
    }

    /// Correction still outstanding. Diagnostic: a device that never converges
    /// shows a `pending` that does not shrink.
    pub const fn pending_us(&self) -> i64 {
        self.pending_us
    }

    /// Jump straight to a show time, for the one case that is not a correction.
    ///
    /// Joining a mesh whose show has been running for an hour is not a drift to
    /// be slewed out - at 200 ppm it would take five days - it is a device that
    /// does not know what time it is yet. It steps once, before it renders
    /// anything, and slews from then on.
    pub fn set(&mut self, show_us: u64) {
        self.now_us = show_us;
        self.pending_us = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_undisciplined_clock_tracks_the_hardware() {
        let mut c = ShowClock::new(1_000);
        assert_eq!(c.advance_to(1_000), 0);
        assert_eq!(c.advance_to(2_000), 1_000);
        assert_eq!(c.advance_to(1_002_000), 1_001_000);
    }

    #[test]
    fn a_correction_is_absorbed_gradually_and_completely() {
        // The whole point: it arrives, and it never arrives all at once.
        let mut c = ShowClock::new(0);
        c.advance_to(1_000_000);
        c.discipline(1_000);

        // One second may absorb 200 us at 200 ppm, so this takes five.
        let mut raw = 1_000_000;
        for _ in 0..4 {
            raw += 1_000_000;
            c.advance_to(raw);
            assert!(c.pending_us() > 0, "absorbed too fast");
        }
        raw += 1_000_000;
        c.advance_to(raw);
        assert_eq!(c.pending_us(), 0, "did not converge");
    }

    #[test]
    fn the_clock_never_goes_backwards_even_correcting_backwards() {
        // A frame rendered twice is a stutter; a clock that went backwards would
        // be a show playing in reverse for a moment. Neither is acceptable, and
        // the second is what a naive negative correction does.
        let mut c = ShowClock::new(0);
        c.advance_to(10_000_000);
        c.discipline(-5_000);

        // 5 ms of correction at 200 ppm needs 25 s of wall time to absorb, so
        // this runs for thirty. Slewing is deliberately slow; a test that
        // expected it to finish quickly would be testing for the bug.
        let mut last = c.now_us();
        let mut raw = 10_000_000;
        for _ in 0..300 {
            raw += 100_000;
            let now = c.advance_to(raw);
            assert!(now >= last, "went backwards: {last} then {now}");
            last = now;
        }
        assert_eq!(c.pending_us(), 0, "did not converge in 30 s");
    }

    #[test]
    fn corrections_accumulate_rather_than_replace() {
        // Two corrections before either is absorbed means both were measured
        // against a clock that was wrong. Dropping one leaves the error in.
        let mut c = ShowClock::new(0);
        c.advance_to(1_000);
        c.discipline(300);
        c.discipline(400);
        assert_eq!(c.pending_us(), 700);
    }

    #[test]
    fn joining_a_running_show_steps_once() {
        // Slewing an hour of difference at 200 ppm would take five days. A
        // device that does not know the time yet is not a device that has
        // drifted.
        let mut c = ShowClock::new(0);
        c.advance_to(1_000_000);
        c.set(3_600_000_000);
        assert_eq!(c.now_us(), 3_600_000_000);
        assert_eq!(c.pending_us(), 0);
        assert_eq!(c.advance_to(2_000_000), 3_601_000_000);
    }

    #[test]
    fn hardware_going_backwards_does_not_move_the_show_clock_backwards() {
        let mut c = ShowClock::new(5_000_000);
        c.advance_to(6_000_000);
        let before = c.now_us();
        assert_eq!(c.advance_to(1_000_000), before);
    }
}
