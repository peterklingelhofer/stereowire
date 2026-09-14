//! Turns a stream of captured audio into fixed-size packets.
//!
//! ScreenCaptureKit delivers audio in whatever chunk size it likes, so this
//! re-blocks it to a constant packet size and attaches the previous block as
//! forward error correction.

use crate::packet::{
    flags, Header, Kind, SampleRate, AUDIO_BLOCK_BYTES, AUDIO_CHANNELS, AUDIO_FRAMES_PER_PACKET,
};

pub struct AudioPacketizer {
    /// Samples not yet forming a whole packet.
    pending: Vec<f32>,
    seq: u64,
    /// Previous packet's bytes, sent alongside the next one as FEC.
    previous: Option<Vec<u8>>,
    /// Capture-clock timestamp of the first sample still held in `pending`.
    ///
    /// Timestamps come from the capture itself rather than a count of frames,
    /// so audio and video share one timebase and their alignment can actually
    /// be measured.
    head_pts_micros: u64,
    fec: bool,
    /// Stamped on every packet so the receiver never has to assume a rate.
    rate: SampleRate,
}

impl AudioPacketizer {
    pub fn new(fec: bool, rate: SampleRate) -> Self {
        AudioPacketizer {
            pending: Vec::new(),
            seq: 0,
            previous: None,
            head_pts_micros: 0,
            fec,
            rate,
        }
    }

    /// Accepts interleaved stereo samples, calling `emit` per whole packet.
    ///
    /// `capture_pts_micros` is the capture timestamp of the first sample in
    /// `samples`. Re-deriving the head timestamp from it on every call keeps
    /// the stream anchored to the capture clock and follows any discontinuity
    /// rather than drifting away from it.
    pub fn push(
        &mut self,
        samples: &[f32],
        capture_pts_micros: u64,
        mut emit: impl FnMut(Header, &[u8]),
    ) {
        let held_frames = (self.pending.len() / AUDIO_CHANNELS) as u64;
        let held_micros = held_frames * 1_000_000 / self.rate.hz() as u64;
        self.head_pts_micros = capture_pts_micros.saturating_sub(held_micros);

        self.pending.extend_from_slice(samples);

        let per_packet = AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS;
        let mut consumed = 0;
        while self.pending.len() - consumed >= per_packet {
            let block: Vec<u8> = self.pending[consumed..consumed + per_packet]
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            consumed += per_packet;

            let mut payload = block.clone();
            let has_fec = match (&self.previous, self.fec) {
                (Some(previous), true) => {
                    payload.extend_from_slice(previous);
                    true
                }
                _ => false,
            };

            let pts_micros = self.head_pts_micros;
            let header = Header {
                kind: Kind::Audio,
                fragment_index: 0,
                fragment_count: 1,
                seq: self.seq,
                pts_micros,
                flags: if has_fec { flags::HAS_FEC } else { 0 },
                rate: Some(self.rate),
            };
            emit(header, &payload);

            self.seq += 1;
            self.head_pts_micros +=
                AUDIO_FRAMES_PER_PACKET as u64 * 1_000_000 / self.rate.hz() as u64;
            self.previous = Some(block);
        }
        self.pending.drain(..consumed);
    }
}

/// A packet carrying its block plus the FEC copy must never need fragmenting;
/// splitting audio across datagrams would make a single loss destroy both.
const _: () = assert!(AUDIO_BLOCK_BYTES * 2 <= crate::packet::MAX_PAYLOAD);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jitter::AudioJitter;

    /// A ramp makes any reordering, gap, or duplication visible in the output.
    fn ramp(count: usize, start: usize) -> Vec<f32> {
        (start..start + count).map(|i| i as f32).collect()
    }

    #[test]
    fn repacketizes_irregular_input_into_fixed_blocks() {
        let mut p = AudioPacketizer::new(true, SampleRate::Hz48000);
        let mut packets = Vec::new();
        // Deliberately awkward chunk sizes, as ScreenCaptureKit would deliver.
        let mut produced = 0;
        for chunk in [100usize, 1, 999, 4096, 37] {
            let samples = ramp(chunk * AUDIO_CHANNELS, produced);
            produced += chunk * AUDIO_CHANNELS;
            p.push(&samples, 0, |h, payload| {
                packets.push((h, payload.to_vec()))
            });
        }
        assert!(!packets.is_empty());
        for (i, (h, payload)) in packets.iter().enumerate() {
            assert_eq!(h.seq, i as u64, "sequence numbers are dense");
            let expected = if i == 0 {
                AUDIO_BLOCK_BYTES
            } else {
                AUDIO_BLOCK_BYTES * 2
            };
            assert_eq!(payload.len(), expected, "packet {i} carries block + FEC");
        }
    }

    /// The whole audio path: capture chunks in, samples out, with loss applied.
    fn round_trip(drop: impl Fn(u64) -> bool) -> Vec<f32> {
        let mut p = AudioPacketizer::new(true, SampleRate::Hz48000);
        let mut j = AudioJitter::new(0);
        let total_frames = AUDIO_FRAMES_PER_PACKET * 20;
        p.push(&ramp(total_frames * AUDIO_CHANNELS, 0), 0, |h, payload| {
            if !drop(h.seq) {
                j.push(h, payload);
            }
        });
        let mut out = vec![-1.0; total_frames * AUDIO_CHANNELS];
        j.pull(&mut out);
        out
    }

    #[test]
    fn every_packet_states_the_sample_rate() {
        // Without this the receiver cannot size its buffer or open an output,
        // and the stream is silently dead.
        for rate in [SampleRate::Hz44100, SampleRate::Hz48000] {
            let mut p = AudioPacketizer::new(true, rate);
            let mut seen = 0;
            p.push(
                &ramp(AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 4, 0),
                0,
                |h: Header, _: &[u8]| {
                    assert_eq!(h.rate, Some(rate));
                    seen += 1;
                },
            );
            assert!(seen > 0, "no packets were produced");
        }
    }

    #[test]
    fn timestamps_start_at_the_capture_clock_and_advance_with_the_audio() {
        for rate in [SampleRate::Hz44100, SampleRate::Hz48000] {
            let mut p = AudioPacketizer::new(false, rate);
            let mut stamps = Vec::new();
            // A capture timestamp is a host clock reading, so it starts large.
            let capture = 9_876_543_210u64;
            p.push(
                &ramp(AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 3, 0),
                capture,
                |h: Header, _: &[u8]| stamps.push(h.pts_micros),
            );
            let step = AUDIO_FRAMES_PER_PACKET as u64 * 1_000_000 / rate.hz() as u64;
            assert_eq!(stamps[0], capture, "the stream starts where capture said");
            assert_eq!(stamps[1], capture + step, "at {} Hz", rate.hz());
        }
    }

    #[test]
    fn a_partial_packet_keeps_its_place_on_the_capture_clock() {
        let rate = SampleRate::Hz48000;
        let mut p = AudioPacketizer::new(false, rate);
        let mut stamps = Vec::new();
        let capture = 1_000_000u64;
        let half = AUDIO_FRAMES_PER_PACKET / 2;

        // Half a packet arrives, then the rest one buffer later. The emitted
        // packet must carry the capture time of its *first* sample, even though
        // a later buffer completed it.
        p.push(
            &ramp(half * AUDIO_CHANNELS, 0),
            capture,
            |_: Header, _: &[u8]| {},
        );
        let later = capture + half as u64 * 1_000_000 / rate.hz() as u64;
        p.push(
            &ramp(half * AUDIO_CHANNELS, 0),
            later,
            |h: Header, _: &[u8]| stamps.push(h.pts_micros),
        );
        assert_eq!(stamps.len(), 1);
        assert_eq!(
            stamps[0], capture,
            "stamped from the oldest sample it holds"
        );
    }

    #[test]
    fn lossless_path_is_sample_exact() {
        let out = round_trip(|_| false);
        let expected = ramp(out.len(), 0);
        assert_eq!(out, expected, "audio must survive the round trip bit-exact");
    }

    #[test]
    fn isolated_losses_are_repaired_exactly() {
        // Drop scattered single packets; each is recoverable from the next
        // packet's piggybacked copy.
        let out = round_trip(|seq| matches!(seq, 3 | 7 | 12 | 15));
        let expected = ramp(out.len(), 0);
        assert_eq!(out, expected, "single-packet loss must be fully recovered");
    }

    #[test]
    fn back_to_back_loss_degrades_but_stays_aligned() {
        // Two in a row: the second is recovered from the next packet's copy,
        // the first is concealed once the reorder window makes clear it is gone.
        let out = round_trip(|seq| matches!(seq, 5 | 6));
        let expected = ramp(out.len(), 0);
        assert_eq!(out.len(), expected.len(), "timeline length is preserved");
        let per_packet = AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS;
        // Everything before the damaged block is untouched.
        assert_eq!(out[..5 * per_packet], expected[..5 * per_packet]);
        // And the stream realigns afterwards rather than drifting.
        let tail = 10 * per_packet;
        assert_eq!(
            out[tail..],
            expected[tail..],
            "playback realigns after the gap"
        );
    }
}
