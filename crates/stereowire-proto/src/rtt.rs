//! Round-trip time and jitter tracking.
//!
//! Both figures matter for different reasons: round-trip time sets how far
//! behind the far end you are, while jitter decides how much audio has to be
//! buffered to play without gaps. A steady 90 ms link is far easier to work
//! against than one wandering between 50 and 130.

/// Weight of each new sample in the smoothed estimates, as a divisor.
/// 16 is the value RFC 3550 uses for jitter, and is slow enough that a single
/// delayed packet does not swing the display.
const SMOOTHING: f64 = 16.0;

#[derive(Debug, Default, Clone, Copy)]
pub struct Rtt {
    last_micros: u64,
    smoothed_micros: f64,
    jitter_micros: f64,
    samples: u64,
}

impl Rtt {
    /// Folds in one measurement.
    pub fn record(&mut self, rtt_micros: u64) {
        let sample = rtt_micros as f64;
        if self.samples == 0 {
            self.smoothed_micros = sample;
            self.jitter_micros = 0.0;
        } else {
            // Jitter is tracked against the smoothed value rather than the
            // previous sample, so a single outlier does not count twice.
            let deviation = (sample - self.smoothed_micros).abs();
            self.jitter_micros += (deviation - self.jitter_micros) / SMOOTHING;
            self.smoothed_micros += (sample - self.smoothed_micros) / SMOOTHING;
        }
        self.last_micros = rtt_micros;
        self.samples += 1;
    }

    pub fn has_measurement(&self) -> bool {
        self.samples > 0
    }

    pub fn last_ms(&self) -> f64 {
        self.last_micros as f64 / 1000.0
    }

    /// The smoothed round trip, which is what to quote as "the latency".
    pub fn smoothed_ms(&self) -> f64 {
        self.smoothed_micros / 1000.0
    }

    pub fn jitter_ms(&self) -> f64 {
        self.jitter_micros / 1000.0
    }

    /// Rough one-way delay. Real routes can be asymmetric, so treat this as
    /// approximate.
    pub fn one_way_ms(&self) -> f64 {
        self.smoothed_ms() / 2.0
    }
}

/// Formats a millisecond figure for display.
///
/// A fast link would otherwise read as a flat "0 ms", which looks like a
/// failed measurement rather than a good one.
pub fn format_ms(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_is_taken_verbatim() {
        let mut rtt = Rtt::default();
        assert!(!rtt.has_measurement());
        rtt.record(80_000);
        assert!(rtt.has_measurement());
        assert_eq!(rtt.smoothed_ms(), 80.0);
        assert_eq!(rtt.last_ms(), 80.0);
        assert_eq!(rtt.jitter_ms(), 0.0, "one sample cannot show jitter");
        assert_eq!(rtt.one_way_ms(), 40.0);
    }

    #[test]
    fn steady_link_converges_with_no_jitter() {
        let mut rtt = Rtt::default();
        for _ in 0..200 {
            rtt.record(75_000);
        }
        assert!((rtt.smoothed_ms() - 75.0).abs() < 0.01);
        assert!(
            rtt.jitter_ms() < 0.01,
            "a perfectly steady link has no jitter"
        );
    }

    #[test]
    fn jitter_rises_on_an_unsteady_link() {
        let mut steady = Rtt::default();
        let mut noisy = Rtt::default();
        for i in 0..200 {
            steady.record(75_000);
            // Swing between 50 and 100 ms.
            noisy.record(if i % 2 == 0 { 50_000 } else { 100_000 });
        }
        assert!(
            noisy.jitter_ms() > steady.jitter_ms() + 5.0,
            "a swinging link must report clearly more jitter, got {} vs {}",
            noisy.jitter_ms(),
            steady.jitter_ms()
        );
        // The smoothed value should still sit between the extremes.
        assert!((50.0..=100.0).contains(&noisy.smoothed_ms()));
    }

    #[test]
    fn small_values_keep_a_decimal_so_they_do_not_read_as_zero() {
        assert_eq!(format_ms(0.4), "0.4");
        assert_eq!(format_ms(9.94), "9.9");
        assert_eq!(format_ms(12.4), "12");
        assert_eq!(format_ms(78.6), "79");
    }

    #[test]
    fn a_single_outlier_does_not_dominate() {
        let mut rtt = Rtt::default();
        for _ in 0..100 {
            rtt.record(70_000);
        }
        rtt.record(500_000);
        assert!(
            rtt.smoothed_ms() < 100.0,
            "one spike should only nudge the estimate: {}",
            rtt.smoothed_ms()
        );
        assert_eq!(rtt.last_ms(), 500.0, "the raw last value is still reported");
    }
}
