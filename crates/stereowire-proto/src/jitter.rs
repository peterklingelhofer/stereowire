//! Audio jitter buffer.
//!
//! Audio is sent as uncompressed interleaved f32, so there is no codec to hide
//! packet loss. Instead each packet piggybacks a copy of the previous packet's
//! block, which makes any isolated loss fully recoverable at the cost of
//! doubling a stream that is only ~3 Mbit/s to begin with.

use std::collections::{BTreeMap, VecDeque};

use crate::packet::{flags, Header, SampleRate, AUDIO_CHANNELS};

/// Samples faded out over a concealed gap, to avoid a click on the seam.
const FADE_SAMPLES: usize = 64;

/// How far the buffer may sit from its target before playback speed is nudged.
///
/// Inside this band the samples are played exactly as they arrived, so ordinary
/// operation stays bit-exact. Correction engages only once the two machines'
/// clocks have actually pulled the buffer off target.
///
/// This has to comfortably exceed one audio callback, since the buffer
/// naturally swings by a whole block as it is filled and drained. At 48 kHz a
/// 512-frame callback is about 11 ms, so a narrower band would read ordinary
/// scheduling as drift and correct constantly for no reason.
const DRIFT_DEADBAND_MS: f64 = 15.0;

/// How sharply the correction ramps once past the deadband.
///
/// A purely proportional response reaches useful authority only when the buffer
/// is already dangerously far off target. This gets there sooner, so the buffer
/// settles close to its target rather than near empty.
const DRIFT_GAIN: f64 = 3.0;

/// Largest playback speed adjustment, as a fraction.
///
/// 0.1% is about 1.7 cents, far below what anyone can hear, and roughly twenty
/// times typical crystal error, so there is ample authority to correct without
/// the correction itself being audible.
const MAX_DRIFT: f64 = 0.001;

/// Smoothing applied to the measured depth, so a single jittery arrival does
/// not swing the playback rate.
const DEPTH_SMOOTHING: f64 = 32.0;

/// Packets held aside waiting for an earlier one that has not arrived yet.
///
/// Long network paths deliver packets out of order, and treating a late arrival
/// as lost throws away audio that is sitting right there. Eight packets is about
/// 11 ms at 48 kHz, which covers the reordering a real path produces while
/// bounding how long a lost packet can hold up playback.
const REORDER_WINDOW: usize = 8;

pub struct AudioJitter {
    pending: VecDeque<f32>,
    /// Sequence number we expect to consume next.
    next_seq: Option<u64>,
    /// Packets that arrived before their predecessors, keyed by sequence.
    staged: BTreeMap<u64, Vec<u8>>,
    /// How much to buffer before playback starts. Converted to samples once
    /// the first packet reveals the sender's rate.
    target_ms: u32,
    target_samples: usize,
    /// Learned from the stream, so the sender can pick a rate that matches its
    /// project and avoid a conversion.
    rate: Option<SampleRate>,
    started: bool,
    last_sample: [f32; AUDIO_CHANNELS],
    /// Smoothed buffer depth, used to detect clock drift.
    smoothed_depth: f64,
    /// Samples consumed per sample emitted. Exactly 1.0 unless correcting.
    ratio: f64,
    /// Fractional read position, in frames, while correcting.
    phase: f64,
    /// Sender-clock timestamp of the next sample due to be played, which is
    /// what a video frame's timestamp has to be compared against to know
    /// whether the two streams are aligned.
    read_pts_micros: Option<u64>,
    pub stats: Stats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Packets that arrived but were already played past.
    pub late: u64,
    /// Blocks rebuilt from a following packet's FEC copy.
    pub recovered: u64,
    /// Blocks that were lost outright and concealed.
    pub concealed: u64,
    /// Times the consumer asked for audio the buffer did not have.
    pub underruns: u64,
    /// Current playback speed correction in parts per million. Zero means the
    /// audio is being played exactly as it arrived.
    pub drift_ppm: i32,
}

impl AudioJitter {
    /// `target_ms` trades latency against tolerance for network jitter.
    pub fn new(target_ms: u32) -> Self {
        AudioJitter {
            pending: VecDeque::new(),
            next_seq: None,
            staged: BTreeMap::new(),
            target_ms,
            target_samples: usize::MAX,
            rate: None,
            started: false,
            last_sample: [0.0; AUDIO_CHANNELS],
            smoothed_depth: 0.0,
            ratio: 1.0,
            phase: 0.0,
            read_pts_micros: None,
            stats: Stats::default(),
        }
    }

    /// Splits a payload into the current block and, if present, the FEC copy of
    /// the preceding block.
    fn split<'a>(header: &Header, payload: &'a [u8]) -> Option<(&'a [u8], Option<&'a [u8]>)> {
        let stride = AUDIO_CHANNELS * 4;
        if payload.is_empty() || !payload.len().is_multiple_of(stride) {
            return None;
        }
        if !header.has(flags::HAS_FEC) {
            return Some((payload, None));
        }
        let half = payload.len() / 2;
        if !payload.len().is_multiple_of(2) || !half.is_multiple_of(stride) || half == 0 {
            return None;
        }
        Some((&payload[..half], Some(&payload[half..])))
    }

    fn append(&mut self, block: &[u8]) {
        // `as_chunks` yields fixed-size arrays, so no fallible conversion is
        // needed to read each sample.
        for chunk in block.as_chunks::<4>().0 {
            self.pending.push_back(f32::from_le_bytes(*chunk));
        }
        // Remember the final frame, so concealing a gap can hold it and fade
        // rather than cutting straight to silence.
        let n = self.pending.len();
        if n >= AUDIO_CHANNELS {
            for (channel, slot) in self.last_sample.iter_mut().enumerate() {
                *slot = self.pending[n - AUDIO_CHANNELS + channel];
            }
        }
    }

    /// Writes one block's worth of concealment: hold the last sample, fading out.
    fn conceal(&mut self, samples: usize) {
        let last = self.last_sample;
        for i in 0..samples / AUDIO_CHANNELS {
            let gain = 1.0 - (i as f32 / FADE_SAMPLES as f32).min(1.0);
            for sample in last {
                self.pending.push_back(sample * gain);
            }
        }
        self.stats.concealed += 1;
    }

    /// Sender-clock timestamp of the audio about to be played.
    pub fn playback_pts_micros(&self) -> Option<u64> {
        self.read_pts_micros
    }

    /// The sender's sample rate, once a packet has revealed it.
    pub fn rate(&self) -> Option<SampleRate> {
        self.rate
    }

    pub fn push(&mut self, header: Header, payload: &[u8]) {
        let Some((current, fec)) = Self::split(&header, payload) else {
            return;
        };

        // The stream states its rate in every packet, which also settles how
        // many samples the pre-roll target corresponds to.
        if let Some(rate) = header.rate {
            if self.rate != Some(rate) {
                self.rate = Some(rate);
                let frames = (rate.hz() as u64 * self.target_ms as u64 / 1000) as usize;
                self.target_samples = frames * AUDIO_CHANNELS;
            }
            let queued_frames = (self.pending.len() / AUDIO_CHANNELS) as u64;
            let behind = queued_frames * 1_000_000 / rate.hz() as u64;
            self.read_pts_micros = Some(header.pts_micros.saturating_sub(behind));
        }

        let expected = *self.next_seq.get_or_insert(header.seq);
        if header.seq < expected {
            // Too late to use: this audio has already been played past.
            self.stats.late += 1;
            return;
        }

        // Hold the packet by sequence, so reordering is absorbed.
        self.staged.insert(header.seq, current.to_vec());

        // The previous packet's copy rides along; keep it if that packet has
        // not shown up, which is what makes an isolated loss free to repair.
        if let Some(fec) = fec {
            if let Some(previous) = header.seq.checked_sub(1) {
                if previous >= expected && !self.staged.contains_key(&previous) {
                    self.staged.insert(previous, fec.to_vec());
                    self.stats.recovered += 1;
                }
            }
        }

        self.drain_staged(current.len());
    }

    /// Moves staged packets into the playback buffer, in order.
    ///
    /// Waits for a missing packet only while the backlog is small; past that
    /// the packet is treated as lost and concealed, because continuing to wait
    /// would stall playback for something that is not coming.
    fn drain_staged(&mut self, block_bytes: usize) {
        loop {
            let next = self.next_seq.expect("set by push");
            if let Some(block) = self.staged.remove(&next) {
                self.append(&block);
                self.next_seq = Some(next + 1);
                continue;
            }
            if self.staged.len() > REORDER_WINDOW {
                self.conceal(block_bytes / 4);
                self.next_seq = Some(next + 1);
                continue;
            }
            break;
        }
    }

    /// Decides the playback speed needed to hold the buffer at its target.
    ///
    /// The two machines' sample clocks are independent, so without this the
    /// buffer slowly fills or empties until it glitches. Typical crystal error
    /// of a few tens of parts per million drains a 30 ms buffer in well under
    /// an hour, which is exactly the length of a working session.
    fn update_drift(&mut self) {
        // With no target depth there is nothing to hold, so play verbatim.
        // This is the case the self-test and the offline tests use.
        let (Some(rate), true) = (self.rate, self.target_samples > 0) else {
            self.ratio = 1.0;
            self.stats.drift_ppm = 0;
            return;
        };

        let depth = self.pending.len() as f64;
        self.smoothed_depth += (depth - self.smoothed_depth) / DEPTH_SMOOTHING;

        let deadband = DRIFT_DEADBAND_MS / 1000.0 * rate.hz() as f64 * AUDIO_CHANNELS as f64;
        let error = self.smoothed_depth - self.target_samples as f64;

        if error.abs() <= deadband {
            // Close enough: play the samples exactly as they arrived.
            self.ratio = 1.0;
            self.stats.drift_ppm = 0;
            return;
        }

        // Too full means consume slightly faster, too empty slightly slower.
        // Scaled by how far past the deadband we are, and hard-limited.
        let excess = (error.abs() - deadband) / deadband;
        let correction = (excess * DRIFT_GAIN * MAX_DRIFT).min(MAX_DRIFT) * error.signum();
        self.ratio = 1.0 + correction;
        self.stats.drift_ppm = (correction * 1_000_000.0) as i32;
    }

    /// Fills `out` with interleaved samples, padding with silence on underrun.
    pub fn pull(&mut self, out: &mut [f32]) {
        // Wait until enough is buffered that normal jitter will not starve us.
        if !self.started {
            if self.pending.len() < self.target_samples {
                out.fill(0.0);
                return;
            }
            self.started = true;
            self.smoothed_depth = self.pending.len() as f64;
        }

        self.update_drift();
        // The timestamp lives on the *input* timeline, so it has to advance by
        // the frames consumed rather than the frames produced. Those differ
        // whenever drift is being corrected.
        let consumed = if self.ratio == 1.0 {
            self.pull_exact(out)
        } else {
            self.pull_resampled(out)
        };

        if let (Some(pts), Some(rate)) = (self.read_pts_micros, self.rate) {
            self.read_pts_micros = Some(pts + consumed * 1_000_000 / rate.hz() as u64);
        }
    }

    /// The ordinary path: samples are handed over untouched. Returns the input
    /// frames consumed.
    fn pull_exact(&mut self, out: &mut [f32]) -> u64 {
        let available = self.pending.len().min(out.len());
        for slot in out[..available].iter_mut() {
            *slot = self.pending.pop_front().expect("available <= len");
        }
        if available < out.len() {
            self.starve(&mut out[available..]);
        }
        (available / AUDIO_CHANNELS) as u64
    }

    /// The correcting path, reading at a fractional rate to absorb clock drift.
    fn pull_resampled(&mut self, out: &mut [f32]) -> u64 {
        let frames_out = out.len() / AUDIO_CHANNELS;
        for frame in 0..frames_out {
            let index = self.phase.floor() as usize;
            // Interpolation needs the frame after the one we are reading.
            if (index + 2) * AUDIO_CHANNELS > self.pending.len() {
                self.starve(&mut out[frame * AUDIO_CHANNELS..]);
                return index as u64;
            }
            let fraction = (self.phase - index as f64) as f32;
            for channel in 0..AUDIO_CHANNELS {
                let a = self.pending[index * AUDIO_CHANNELS + channel];
                let b = self.pending[(index + 1) * AUDIO_CHANNELS + channel];
                out[frame * AUDIO_CHANNELS + channel] = a + (b - a) * fraction;
            }
            self.phase += self.ratio;
        }

        // Discard whole frames that have been read past.
        let consumed = self.phase.floor() as usize;
        for _ in 0..consumed * AUDIO_CHANNELS {
            self.pending.pop_front();
        }
        self.phase -= consumed as f64;
        consumed as u64
    }

    /// Handles running out of audio: fill with silence and re-buffer.
    fn starve(&mut self, rest: &mut [f32]) {
        rest.fill(0.0);
        self.stats.underruns += 1;
        self.pending.clear();
        // The timeline restarts with the next block that arrives.
        self.read_pts_micros = None;
        self.phase = 0.0;
        self.ratio = 1.0;
        // Re-buffer rather than limping along one sample from empty.
        self.started = false;
    }

    /// Interleaved samples currently buffered, for latency reporting.
    pub fn depth(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{Kind, AUDIO_FRAMES_PER_PACKET};

    fn block(value: f32) -> Vec<u8> {
        std::iter::repeat_n(value, AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS)
            .flat_map(f32::to_le_bytes)
            .collect()
    }

    fn header(seq: u64, fec: bool) -> Header {
        Header {
            kind: Kind::Audio,
            fragment_index: 0,
            fragment_count: 1,
            seq,
            pts_micros: 0,
            flags: if fec { flags::HAS_FEC } else { 0 },
            rate: Some(SampleRate::Hz48000),
        }
    }

    /// Sends packet `seq` carrying its own block plus the previous as FEC.
    fn packet(seq: u64) -> (Header, Vec<u8>) {
        let mut payload = block(seq as f32);
        payload.extend_from_slice(&block(seq.wrapping_sub(1) as f32));
        (header(seq, true), payload)
    }

    #[test]
    fn in_order_audio_passes_through_untouched() {
        let mut j = AudioJitter::new(0);
        for seq in 0..4 {
            let (h, p) = packet(seq);
            j.push(h, &p);
        }
        let mut out = vec![0.0; AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 4];
        j.pull(&mut out);
        for (i, chunk) in out
            .chunks(AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS)
            .enumerate()
        {
            assert!(chunk.iter().all(|&s| s == i as f32), "block {i} intact");
        }
        assert_eq!(j.stats, Stats::default(), "clean stream needs no repair");
    }

    #[test]
    fn single_loss_is_fully_recovered_by_fec() {
        let mut j = AudioJitter::new(0);
        for seq in [0u64, 1, /* 2 dropped */ 3] {
            let (h, p) = packet(seq);
            j.push(h, &p);
        }
        let mut out = vec![0.0; AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 4];
        j.pull(&mut out);
        let third = &out[AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 2..]
            [..AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS];
        assert!(
            third.iter().all(|&s| s == 2.0),
            "lost block rebuilt exactly"
        );
        assert_eq!(j.stats.recovered, 1);
        assert_eq!(j.stats.concealed, 0, "FEC recovery is not concealment");
    }

    #[test]
    fn burst_loss_is_concealed_only_once_it_is_clearly_lost() {
        let mut j = AudioJitter::new(0);
        let (h, p) = packet(0);
        j.push(h, &p);
        // Drop 1 and 2. Packet 3 carries a copy of 2, so 2 is free to repair.
        let (h, p) = packet(3);
        j.push(h, &p);
        assert_eq!(j.stats.recovered, 1, "the adjacent block is recovered");
        assert_eq!(
            j.stats.concealed, 0,
            "packet 1 might still be merely reordered, so do not give up yet"
        );

        // Once the backlog outgrows the reorder window, it is gone for good.
        for seq in 4..4 + REORDER_WINDOW as u64 + 2 {
            let (h, p) = packet(seq);
            j.push(h, &p);
        }
        assert_eq!(j.stats.concealed, 1, "the lost block is concealed");
    }

    #[test]
    fn reordered_packets_are_played_in_their_right_order() {
        let mut j = AudioJitter::new(0);
        // Arriving 0, 2, 1, 3: nothing is lost, only shuffled. FEC is off here
        // so this measures reordering alone rather than repair.
        for seq in [0u64, 2, 1, 3] {
            j.push(header(seq, false), &block(seq as f32));
        }
        let mut out = vec![0.0; AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 4];
        j.pull(&mut out);
        for (i, chunk) in out
            .chunks(AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS)
            .enumerate()
        {
            assert!(
                chunk.iter().all(|&v| v == i as f32),
                "block {i} must play in sequence order"
            );
        }
        assert_eq!(j.stats.concealed, 0, "reordering is not loss");
        assert_eq!(j.stats.late, 0);
    }

    #[test]
    fn late_and_duplicate_packets_are_dropped() {
        let mut j = AudioJitter::new(0);
        for seq in 0..3 {
            let (h, p) = packet(seq);
            j.push(h, &p);
        }
        let depth = j.depth();
        let (h, p) = packet(1);
        j.push(h, &p);
        assert_eq!(j.depth(), depth, "a late packet must not inject audio");
        assert_eq!(j.stats.late, 1);
    }

    #[test]
    fn malformed_payloads_are_ignored() {
        let mut j = AudioJitter::new(0);
        for bad in [vec![], vec![0u8; 3], vec![0u8; 9]] {
            j.push(header(0, false), &bad);
            assert_eq!(j.depth(), 0, "garbage must not enter the buffer");
        }
        // Odd number of blocks cannot be split into current + FEC halves.
        j.push(header(0, true), &[0u8; AUDIO_CHANNELS * 4 * 3]);
    }

    #[test]
    fn underrun_pads_with_silence_and_rebuffers() {
        let mut j = AudioJitter::new(0);
        let (h, p) = packet(0);
        j.push(h, &p);
        let mut out = vec![9.0; AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 2];
        j.pull(&mut out);
        assert!(out[AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS..]
            .iter()
            .all(|&s| s == 0.0));
        assert_eq!(j.stats.underruns, 1);
        assert!(!j.started, "should re-buffer after starving");
    }

    /// Plays a stream in real-time-sized blocks while the sender's clock runs
    /// at `drift_ppm` relative to ours, and reports what the buffer did.
    fn simulate_drift(drift_ppm: f64, seconds: f64) -> (Stats, usize) {
        use crate::packetize::AudioPacketizer;
        let rate = SampleRate::Hz48000;
        let mut sender = AudioPacketizer::new(false, rate);
        let mut j = AudioJitter::new(30);

        // A typical CoreAudio callback size.
        let block_frames = 512usize;
        let pulls = (seconds * rate.hz() as f64 / block_frames as f64) as usize;
        let mut out = vec![0.0f32; block_frames * AUDIO_CHANNELS];

        // Start at the buffer's own target depth, which is where a real
        // receiver settles once playback begins.
        let mut produced = rate.hz() as f64 * 0.030;
        let mut delivered = 0usize;

        for _ in 0..pulls {
            // The sender's clock differs slightly from ours.
            produced += block_frames as f64 * (1.0 + drift_ppm / 1_000_000.0);
            let want = produced as usize;
            if want > delivered {
                let samples: Vec<f32> = (delivered..want)
                    .flat_map(|f| {
                        let v = (f % 1000) as f32 / 1000.0;
                        [v, -v]
                    })
                    .collect();
                sender.push(&samples, 0, |h, p| j.push(h, p));
                delivered = want;
            }
            j.pull(&mut out);
        }
        (j.stats, j.depth())
    }

    #[test]
    fn a_slow_sender_does_not_starve_the_buffer() {
        // 300 ppm is several times a realistic crystal error, and over two
        // minutes it exceeds the whole 30 ms buffer. Without compensation this
        // is an audible dropout; with it, playback simply slows imperceptibly.
        let (stats, depth) = simulate_drift(-300.0, 120.0);
        assert_eq!(stats.underruns, 0, "clock drift must not cause dropouts");
        assert!(depth > 0, "buffer should still hold audio, got {depth}");
    }

    #[test]
    fn a_fast_sender_does_not_let_latency_grow_without_bound() {
        let (stats, depth) = simulate_drift(300.0, 120.0);
        assert_eq!(stats.underruns, 0);
        // Two minutes at 300 ppm is 36 ms of extra audio. Held near the 30 ms
        // target it stays well under that; uncorrected it would simply pile up.
        let target = (48_000.0 * 0.030) as usize * AUDIO_CHANNELS;
        assert!(
            depth < target * 3,
            "latency ran away: depth {depth} against target {target}"
        );
    }

    #[test]
    fn a_matched_clock_plays_bit_exactly() {
        let (stats, _) = simulate_drift(0.0, 30.0);
        assert_eq!(stats.underruns, 0);
        assert_eq!(
            stats.drift_ppm, 0,
            "with clocks in step the audio must be passed through untouched"
        );
    }

    #[test]
    fn the_correction_stays_inaudible() {
        // Even against drift far beyond anything real, the speed change is
        // clamped well below what anyone could hear.
        for drift in [-5000.0, 5000.0] {
            let (stats, _) = simulate_drift(drift, 30.0);
            let limit = (MAX_DRIFT * 1_000_000.0) as i32;
            assert!(
                stats.drift_ppm.abs() <= limit,
                "correction {} ppm exceeded the {limit} ppm limit",
                stats.drift_ppm
            );
        }
    }

    #[test]
    fn playback_timestamp_tracks_the_audio_actually_being_played() {
        use crate::packetize::AudioPacketizer;
        let rate = SampleRate::Hz48000;
        let mut sender = AudioPacketizer::new(false, rate);
        let mut j = AudioJitter::new(0);
        assert_eq!(j.playback_pts_micros(), None, "nothing playing yet");

        // Five packets' worth, timestamped by the real packetizer.
        let frames = AUDIO_FRAMES_PER_PACKET * 5;
        let samples = vec![0.25f32; frames * AUDIO_CHANNELS];
        sender.push(&samples, 0, |h, p| j.push(h, p));

        // However much is queued, the read head is still at the start.
        assert_eq!(
            j.playback_pts_micros(),
            Some(0),
            "the read head is at the oldest audio held"
        );

        // Consuming two packets' worth advances it by exactly that duration.
        let consumed_frames = AUDIO_FRAMES_PER_PACKET * 2;
        let mut out = vec![0.0; consumed_frames * AUDIO_CHANNELS];
        j.pull(&mut out);
        let expected = consumed_frames as u64 * 1_000_000 / rate.hz() as u64;
        assert_eq!(j.playback_pts_micros(), Some(expected));
    }

    #[test]
    fn holds_playback_until_target_depth() {
        let mut j = AudioJitter::new(20);
        let (h, p) = packet(0);
        j.push(h, &p);
        let mut out = vec![9.0; 64];
        j.pull(&mut out);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "must not start under target depth"
        );
    }
}
