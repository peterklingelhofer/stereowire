//! Video bitrate control.
//!
//! The receiver has every signal worth acting on (loss, round trip, jitter), so
//! it decides the rate and asks the sender for it. The sender clamps that
//! request to what it was configured to allow.
//!
//! Two signals drive it. Loss is the obvious one but arrives late: by the time
//! packets drop, a queue somewhere is already full. Rising round-trip time
//! relative to the quietest measurement seen is the earlier warning, because a
//! filling buffer shows up as delay before it shows up as loss.

/// Loss above this is unambiguous congestion; back off hard.
const LOSS_HIGH: f32 = 0.05;
/// Below this the link is considered clean. Real links lose the occasional
/// packet without being congested, and reacting to that would cost quality for
/// nothing.
const LOSS_LOW: f32 = 0.01;

/// Queueing delay above this means a buffer is filling; back off.
const QUEUE_HIGH_MS: f64 = 60.0;
/// Above this, hold steady rather than probing for more.
const QUEUE_LOW_MS: f64 = 25.0;

/// Multiplicative decrease. Sharp enough to clear a queue quickly.
const DECREASE: f64 = 0.85;
/// Additive-ish increase. Deliberately slower than the decrease.
const INCREASE: f64 = 1.08;

/// Consecutive clean reports required before probing upward, so a single quiet
/// moment during congestion does not restart the climb.
const CLEAN_BEFORE_INCREASE: u32 = 2;

#[derive(Debug, Clone, Copy)]
pub struct Feedback {
    /// Fraction of video frames lost over the last window, 0.0 to 1.0.
    pub loss: f32,
    pub rtt_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Decreased,
    Held,
    Increased,
}

pub struct Controller {
    target_bps: u32,
    min_bps: u32,
    max_bps: u32,
    /// The quietest round trip seen, taken as the uncongested baseline.
    baseline_rtt_ms: f64,
    clean_reports: u32,
    /// Smoothed loss, so parity does not flap on a single unlucky report.
    recent_loss: f32,
}

impl Controller {
    /// Starts at the maximum: an idle link should never be probed up to its
    /// configured quality, it should simply run there.
    pub fn new(max_bps: u32, min_bps: u32) -> Self {
        Controller {
            target_bps: max_bps,
            min_bps: min_bps.min(max_bps),
            max_bps,
            baseline_rtt_ms: f64::INFINITY,
            clean_reports: 0,
            recent_loss: 0.0,
        }
    }

    pub fn target_bps(&self) -> u32 {
        self.target_bps
    }

    /// Parity blocks each fragment group should carry.
    ///
    /// A clean link pays 10% for single-loss repair. Once packets are actually
    /// going missing the second block is worth its 10%, because it makes any
    /// pair of losses in a group recoverable instead of costing a whole frame.
    pub fn parity_blocks(&self) -> u32 {
        if self.recent_loss > LOSS_LOW {
            2
        } else {
            1
        }
    }

    pub fn update(&mut self, feedback: Feedback) -> Action {
        // Decay toward the newest reading, so a burst raises parity quickly but
        // one clean window does not drop it again.
        self.recent_loss = self.recent_loss.max(feedback.loss) * 0.75 + feedback.loss * 0.25;
        if feedback.rtt_ms > 0.0 {
            self.baseline_rtt_ms = self.baseline_rtt_ms.min(feedback.rtt_ms);
        }
        let queueing = if self.baseline_rtt_ms.is_finite() {
            (feedback.rtt_ms - self.baseline_rtt_ms).max(0.0)
        } else {
            0.0
        };

        if feedback.loss > LOSS_HIGH || queueing > QUEUE_HIGH_MS {
            self.clean_reports = 0;
            self.scale(DECREASE);
            Action::Decreased
        } else if feedback.loss > LOSS_LOW || queueing > QUEUE_LOW_MS {
            // Something is off but not clearly congestion. Stop probing rather
            // than give up quality.
            self.clean_reports = 0;
            Action::Held
        } else {
            self.clean_reports += 1;
            if self.clean_reports >= CLEAN_BEFORE_INCREASE && self.target_bps < self.max_bps {
                self.scale(INCREASE);
                Action::Increased
            } else {
                Action::Held
            }
        }
    }

    fn scale(&mut self, factor: f64) {
        let scaled = (self.target_bps as f64 * factor) as u32;
        self.target_bps = scaled.clamp(self.min_bps, self.max_bps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u32 = 40_000_000;
    const MIN: u32 = 5_000_000;

    fn clean() -> Feedback {
        Feedback {
            loss: 0.0,
            rtt_ms: 80.0,
        }
    }

    #[test]
    fn parity_rises_with_loss_and_settles_back_when_it_clears() {
        let mut c = Controller::new(MAX, MIN);
        assert_eq!(
            c.parity_blocks(),
            1,
            "a fresh link pays only for single-loss repair"
        );

        for _ in 0..5 {
            c.update(Feedback {
                loss: 0.03,
                rtt_ms: 80.0,
            });
        }
        assert_eq!(
            c.parity_blocks(),
            2,
            "real loss is worth the second parity block"
        );

        for _ in 0..50 {
            c.update(clean());
        }
        assert_eq!(
            c.parity_blocks(),
            1,
            "a link that clears should stop paying for it"
        );
    }

    #[test]
    fn a_single_unlucky_window_does_not_flap_the_parity() {
        let mut c = Controller::new(MAX, MIN);
        for _ in 0..20 {
            c.update(clean());
        }
        c.update(Feedback {
            loss: 0.03,
            rtt_ms: 80.0,
        });
        assert_eq!(c.parity_blocks(), 2);
        // One clean report straight after must not immediately undo it.
        c.update(clean());
        assert_eq!(
            c.parity_blocks(),
            2,
            "parity should not chatter report to report"
        );
    }

    #[test]
    fn a_clean_link_stays_at_full_quality() {
        let mut c = Controller::new(MAX, MIN);
        for _ in 0..100 {
            c.update(clean());
        }
        assert_eq!(c.target_bps(), MAX, "an idle link must never cost quality");
    }

    #[test]
    fn occasional_loss_does_not_cost_quality() {
        let mut c = Controller::new(MAX, MIN);
        for _ in 0..50 {
            // Half a percent is normal for the internet.
            c.update(Feedback {
                loss: 0.005,
                rtt_ms: 80.0,
            });
        }
        assert_eq!(
            c.target_bps(),
            MAX,
            "sporadic loss must not trigger backoff"
        );
    }

    #[test]
    fn sustained_loss_backs_off_but_never_below_the_floor() {
        let mut c = Controller::new(MAX, MIN);
        for _ in 0..200 {
            c.update(Feedback {
                loss: 0.20,
                rtt_ms: 80.0,
            });
        }
        assert_eq!(c.target_bps(), MIN, "must stop at the floor");
    }

    #[test]
    fn rising_delay_backs_off_before_any_loss_appears() {
        let mut c = Controller::new(MAX, MIN);
        c.update(clean());
        let before = c.target_bps();
        // A queue is filling: the round trip climbs while loss is still zero.
        let action = c.update(Feedback {
            loss: 0.0,
            rtt_ms: 80.0 + QUEUE_HIGH_MS + 10.0,
        });
        assert_eq!(action, Action::Decreased, "delay is the early warning");
        assert!(c.target_bps() < before);
    }

    #[test]
    fn recovers_to_full_quality_once_the_link_clears() {
        let mut c = Controller::new(MAX, MIN);
        for _ in 0..20 {
            c.update(Feedback {
                loss: 0.20,
                rtt_ms: 80.0,
            });
        }
        assert!(c.target_bps() < MAX);
        for _ in 0..200 {
            c.update(clean());
        }
        assert_eq!(c.target_bps(), MAX, "quality must come back");
    }

    #[test]
    fn one_clean_report_during_congestion_does_not_restart_the_climb() {
        let mut c = Controller::new(MAX, MIN);
        for _ in 0..5 {
            c.update(Feedback {
                loss: 0.20,
                rtt_ms: 80.0,
            });
        }
        let dipped = c.target_bps();
        // A single quiet window between bursts must not be read as recovery.
        assert_eq!(c.update(clean()), Action::Held);
        assert_eq!(c.target_bps(), dipped);
    }

    #[test]
    fn the_baseline_tracks_the_quietest_round_trip() {
        let mut c = Controller::new(MAX, MIN);
        // A link that starts congested should not treat that as normal.
        c.update(Feedback {
            loss: 0.0,
            rtt_ms: 200.0,
        });
        c.update(Feedback {
            loss: 0.0,
            rtt_ms: 80.0,
        });
        // 80 is now the baseline, so 80 is not queueing and must not back off.
        for _ in 0..10 {
            c.update(Feedback {
                loss: 0.0,
                rtt_ms: 82.0,
            });
        }
        assert_eq!(c.target_bps(), MAX);
    }
}
