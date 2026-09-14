//! Packing encoded video onto the wire, and getting it back off.
//!
//! Both ends of this live here rather than in the sender and receiver, so the
//! self-test exercises the same code the app runs instead of a lookalike.

use std::collections::BTreeMap;

use crate::fec;
use crate::packet::{flags, read_params, write_params, Header, Kind, HEADER_LEN, MTU};
use crate::reassembly::Reassembler;

/// One compressed frame, borrowed from whatever the encoder produced.
pub struct Frame<'a> {
    pub data: &'a [u8],
    /// VPS/SPS/PPS. Sent with every frame: they total about a hundred bytes,
    /// and carrying them only on keyframes means one lost keyframe fragment
    /// leaves the receiver unable to build a decoder until the next one.
    pub params: &'a [Vec<u8>],
    pub keyframe: bool,
    pub pts_micros: u64,
}

/// Numbers frames, splits them into datagrams, and adds parity.
pub struct Framer {
    seq: u64,
    /// Reused across frames so a 60 fps stream is not allocating per frame.
    payload: Vec<u8>,
    scratch: Vec<u8>,
    /// Parity is emitted once per this many fragments. Zero disables it.
    fec_group: usize,
    /// Parity blocks per group: one repairs a single loss, two repair any pair.
    parity_blocks: usize,
}

impl Default for Framer {
    fn default() -> Self {
        Framer {
            seq: 0,
            payload: Vec::new(),
            scratch: Vec::new(),
            fec_group: fec::GROUP,
            parity_blocks: 1,
        }
    }
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Turns parity off, trading loss resilience for about 10% less traffic.
    pub fn without_fec() -> Self {
        Framer {
            fec_group: 0,
            ..Self::default()
        }
    }

    /// Sets how many parity blocks each group carries.
    ///
    /// One rebuilds a single lost fragment per group and costs about 10%. Two
    /// rebuilds any pair in a group and costs about 20%, which is worth paying
    /// only while the link is actually losing packets.
    pub fn set_parity(&mut self, blocks: usize) {
        self.parity_blocks = blocks.min(fec::MAX_PARITY);
    }

    /// Emits the frame's datagrams, and a parity datagram per group. Returns
    /// the sequence number used.
    pub fn send(&mut self, frame: Frame<'_>, mut emit: impl FnMut(&[u8])) -> u64 {
        let seq = self.seq;
        self.seq += 1;

        self.payload.clear();
        let mut header_flags = 0;
        if frame.keyframe {
            header_flags |= flags::KEYFRAME;
        }
        if !frame.params.is_empty() {
            header_flags |= flags::HAS_PARAMS;
            write_params(frame.params, &mut self.payload);
        }
        self.payload.extend_from_slice(frame.data);

        // Every fragment carries its own length, so a rebuilt one knows its
        // size. That costs two bytes and sets the usable block width.
        let chunks: Vec<&[u8]> = if self.payload.is_empty() {
            vec![&[]]
        } else {
            self.payload.chunks(fec::BLOCK).collect()
        };
        let count = chunks.len() as u16;

        let mut datagram = [0u8; MTU];
        let mut parity = [[0u8; fec::PARITY_LEN]; fec::MAX_PARITY];
        let mut group_start = 0usize;

        for (index, chunk) in chunks.iter().enumerate() {
            let header = Header {
                kind: Kind::Video,
                fragment_index: index as u16,
                fragment_count: count,
                seq,
                pts_micros: frame.pts_micros,
                flags: header_flags,
                rate: None,
            };
            let mut head = [0u8; HEADER_LEN];
            header.write(&mut head);
            fec::encode_fragment(chunk, &mut self.scratch);
            datagram[..HEADER_LEN].copy_from_slice(&head);
            datagram[HEADER_LEN..HEADER_LEN + self.scratch.len()].copy_from_slice(&self.scratch);
            emit(&datagram[..HEADER_LEN + self.scratch.len()]);

            if self.fec_group > 0 && self.parity_blocks > 0 {
                // Position within the group is what weights the second parity
                // block, and so what makes the two equations independent.
                for (level, block) in parity.iter_mut().enumerate().take(self.parity_blocks) {
                    fec::accumulate_at(block, chunk, index - group_start, level);
                }
                let group_full = index + 1 - group_start == self.fec_group;
                if group_full || index + 1 == chunks.len() {
                    // A group of one is just a copy of the fragment, so it buys
                    // nothing and is not worth the bandwidth.
                    if index + 1 - group_start > 1 {
                        for (level, block) in parity.iter().enumerate().take(self.parity_blocks) {
                            let header = Header {
                                kind: Kind::Video,
                                fragment_index: group_start as u16,
                                fragment_count: count,
                                seq,
                                pts_micros: frame.pts_micros,
                                flags: header_flags | flags::PARITY,
                                rate: None,
                            };
                            let mut head = [0u8; HEADER_LEN];
                            header.write(&mut head);
                            datagram[..HEADER_LEN].copy_from_slice(&head);
                            // Which parity block this is, so P and Q can be told
                            // apart however they arrive.
                            datagram[HEADER_LEN] = level as u8;
                            let body = HEADER_LEN + fec::LEVEL_PREFIX;
                            datagram[body..body + fec::PARITY_LEN].copy_from_slice(block);
                            emit(&datagram[..body + fec::PARITY_LEN]);
                        }
                    }
                    parity = [[0u8; fec::PARITY_LEN]; fec::MAX_PARITY];
                    group_start = index + 1;
                }
            }
        }
        seq
    }
}

/// What arrived, once a frame finished reassembling.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    /// Still collecting fragments.
    Pending,
    /// Decodable. Parameter sets are present whenever they were carried.
    Frame {
        /// Frame number, which is dense and increasing across released frames.
        seq: u64,
        params: Option<Vec<Vec<u8>>>,
        data: Vec<u8>,
        pts_micros: u64,
    },
    /// A frame completed but cannot be decoded yet, because no keyframe has
    /// been seen since the stream started or since a frame went missing. The
    /// caller should ask the sender for a keyframe.
    NeedKeyframe,
}

/// Frames held waiting for an earlier one that has not arrived yet.
///
/// Video must be decoded in order, so a frame that completes early cannot
/// simply be shown: doing that strands its predecessor and forces a resync.
/// Three frames is 50 ms at 60 fps, which covers the reordering a long path
/// produces and is far cheaper than the freeze a resync causes while waiting
/// for the next keyframe.
const REORDER_WINDOW: usize = 3;

/// A frame held back until its turn.
struct Staged {
    params: Option<Vec<Vec<u8>>>,
    data: Vec<u8>,
    pts_micros: u64,
    keyframe: bool,
}

/// Reassembles frames, puts them back in order, and tracks whether the stream
/// can actually be decoded.
#[derive(Default)]
pub struct Depacketizer {
    reassembler: Reassembler,
    have_params: bool,
    synced: bool,
    /// Next frame sequence due for release.
    next_seq: Option<u64>,
    /// Completed frames waiting for their predecessors.
    staged: BTreeMap<u64, Staged>,
    /// Stop waiting on gaps and release everything held.
    flushing: bool,
    /// Times a missing frame forced the stream to wait for a new keyframe.
    pub resyncs: u64,
    /// Frames dropped because nothing could decode them yet.
    pub discarded: u64,
    /// Frames that finished reassembly since the last loss report.
    window_completed: u64,
    /// Sequence range covered by the current loss window.
    window_first: Option<u64>,
    window_last: u64,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fragments rebuilt from parity rather than received.
    pub fn repaired(&self) -> u64 {
        self.reassembler.repaired
    }

    /// Fraction of frames lost since the previous call, and resets the window.
    ///
    /// Derived from gaps in the sequence numbers, because a frame that loses
    /// any fragment never surfaces at all.
    pub fn take_loss(&mut self) -> Option<f32> {
        let first = self.window_first.take()?;
        let expected = self.window_last.saturating_sub(first) + 1;
        let completed = self.window_completed.min(expected);
        self.window_completed = 0;
        // One frame tells us nothing about a loss rate.
        if expected < 2 {
            return None;
        }
        Some(1.0 - completed as f32 / expected as f32)
    }

    /// Absorbs one datagram. Frames it completes are released by [`poll`].
    ///
    /// [`poll`]: Depacketizer::poll
    pub fn push(&mut self, header: Header, body: &[u8]) {
        let Some(frame) = self.reassembler.push(header, body) else {
            return;
        };
        let seq = frame.header.seq;

        // Already released. A duplicate, or so late it cannot be used.
        if self.next_seq.is_some_and(|next| seq < next) {
            return;
        }

        let (params, data) = if frame.header.has(flags::HAS_PARAMS) {
            match read_params(&frame.data) {
                Some((sets, rest)) => (Some(sets), rest.to_vec()),
                // A frame whose parameter blob will not parse is unusable.
                None => return,
            }
        } else {
            (None, frame.data)
        };

        self.flushing = false;
        self.next_seq.get_or_insert(seq);
        self.staged.insert(
            seq,
            Staged {
                params,
                data,
                pts_micros: frame.header.pts_micros,
                keyframe: frame.header.has(flags::KEYFRAME),
            },
        );
    }

    /// Returns the next frame due, in sequence order.
    ///
    /// Call until it reports [`Received::Pending`]: one arrival can release
    /// several frames at once when it fills a gap.
    pub fn poll(&mut self) -> Received {
        let Some(next) = self.next_seq else {
            return Received::Pending;
        };

        // Nothing held means nothing to release or give up on, whatever the
        // gap policy says.
        if self.staged.is_empty() {
            return Received::Pending;
        }

        let Some(frame) = self.staged.remove(&next) else {
            // Wait for the missing frame while the backlog stays small. Past
            // that it is lost for good, and everything after it references
            // pictures we will never have.
            if self.flushing || self.staged.len() > REORDER_WINDOW {
                // Counted in the window's range but not among its arrivals,
                // which is exactly what makes it register as loss.
                self.mark_window(next);
                self.next_seq = Some(next + 1);
                self.synced = false;
                self.resyncs += 1;
                self.discarded += 1;
                return Received::NeedKeyframe;
            }
            return Received::Pending;
        };

        // It arrived, so it counts as delivered even if nothing can decode it
        // yet; decodability is the sync layer's concern.
        self.mark_window(next);
        self.window_completed += 1;

        self.next_seq = Some(next + 1);
        if frame.params.is_some() {
            self.have_params = true;
        }
        if frame.keyframe {
            self.synced = true;
        }
        if !(self.have_params && self.synced) {
            self.discarded += 1;
            return Received::NeedKeyframe;
        }

        Received::Frame {
            seq: next,
            params: frame.params,
            data: frame.data,
            pts_micros: frame.pts_micros,
        }
    }

    /// Widens the loss window to include this sequence number.
    fn mark_window(&mut self, seq: u64) {
        self.window_first.get_or_insert(seq);
        self.window_last = self.window_last.max(seq);
    }

    /// Stops waiting on missing frames and releases everything still held.
    ///
    /// For the end of a finite stream, where nothing more is coming and holding
    /// a gap open would simply strand the frames behind it.
    pub fn flush(&mut self) {
        self.flushing = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::MAX_PAYLOAD;

    fn params() -> Vec<Vec<u8>> {
        vec![vec![1u8, 2, 3], vec![4u8; 40], vec![5u8; 12]]
    }

    /// Pushes one frame through framing, the wire, and depacketizing.
    fn round_trip(d: &mut Depacketizer, f: &mut Framer, frame: Frame<'_>) -> Vec<Received> {
        let mut datagrams = Vec::new();
        f.send(frame, |dg| datagrams.push(dg.to_vec()));
        for dg in &datagrams {
            let (header, body) = Header::parse(dg).expect("parses");
            d.push(header, body);
        }
        drain(d)
    }

    /// Everything the depacketizer is willing to release right now.
    fn drain(d: &mut Depacketizer) -> Vec<Received> {
        let mut out = Vec::new();
        loop {
            match d.poll() {
                Received::Pending => return out,
                other => out.push(other),
            }
        }
    }

    /// The frame's outcome, which is whichever datagram completed it. The
    /// trailing parity datagrams correctly report nothing.
    fn last(results: Vec<Received>) -> Received {
        results.into_iter().next().unwrap_or(Received::Pending)
    }

    #[test]
    fn keyframe_round_trips_with_its_parameter_sets() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let data = vec![9u8; MAX_PAYLOAD * 3 + 11];
        let sets = params();
        let got = last(round_trip(
            &mut d,
            &mut f,
            Frame {
                data: &data,
                params: &sets,
                keyframe: true,
                pts_micros: 4242,
            },
        ));
        assert_eq!(
            got,
            Received::Frame {
                seq: 0,
                params: Some(sets),
                data,
                pts_micros: 4242
            },
            "a keyframe must survive fragmentation intact"
        );
    }

    #[test]
    fn frames_before_the_first_keyframe_are_refused() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        // A P-frame arriving first references pictures we never saw.
        let got = last(round_trip(
            &mut d,
            &mut f,
            Frame {
                data: &[7u8; 64],
                params: &sets,
                keyframe: false,
                pts_micros: 0,
            },
        ));
        assert_eq!(got, Received::NeedKeyframe);

        // Once a keyframe lands, the stream opens up.
        let got = last(round_trip(
            &mut d,
            &mut f,
            Frame {
                data: &[8u8; 64],
                params: &sets,
                keyframe: true,
                pts_micros: 1,
            },
        ));
        assert!(matches!(got, Received::Frame { .. }));
    }

    #[test]
    fn a_lost_frame_forces_a_resync_until_the_next_keyframe() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        let frame = |keyframe, pts_micros| Frame {
            data: &[3u8; 64],
            params: &sets,
            keyframe,
            pts_micros,
        };

        assert!(matches!(
            last(round_trip(&mut d, &mut f, frame(true, 0))),
            Received::Frame { .. }
        ));
        assert!(matches!(
            last(round_trip(&mut d, &mut f, frame(false, 1))),
            Received::Frame { .. }
        ));

        // Drop the next frame entirely: frame it, then never deliver it.
        f.send(frame(false, 2), |_| {});

        // The gap is held open for a few frames in case it was merely
        // reordered, so nothing is released while we wait.
        for pts in 3..3 + REORDER_WINDOW as u64 {
            assert!(
                round_trip(&mut d, &mut f, frame(false, pts)).is_empty(),
                "must wait rather than skip a frame that might be reordered"
            );
        }

        // Once the backlog outgrows the window it is lost for good, and
        // everything after it references pictures we do not have.
        let results = round_trip(&mut d, &mut f, frame(false, 99));
        assert_eq!(
            results.first(),
            Some(&Received::NeedKeyframe),
            "gap gives up"
        );
        assert_eq!(d.resyncs, 1);

        // A keyframe restores the stream.
        let results = round_trip(&mut d, &mut f, frame(true, 100));
        assert!(
            results.iter().any(|r| matches!(r, Received::Frame { .. })),
            "a keyframe must restore playback"
        );
    }

    #[test]
    fn frames_completing_out_of_order_are_released_in_order() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        // Frame each of three, but deliver the middle one last.
        let mut wire: Vec<Vec<Vec<u8>>> = Vec::new();
        for (i, keyframe) in [true, false, false].into_iter().enumerate() {
            let data = vec![i as u8 + 1; 64];
            let mut dgs = Vec::new();
            f.send(
                Frame {
                    data: &data,
                    params: &sets,
                    keyframe,
                    pts_micros: i as u64,
                },
                |dg| dgs.push(dg.to_vec()),
            );
            wire.push(dgs);
        }

        let mut released = Vec::new();
        for order in [0usize, 2, 1] {
            for dg in &wire[order] {
                let (h, b) = Header::parse(dg).expect("parses");
                d.push(h, b);
            }
            released.extend(drain(&mut d));
        }

        let stamps: Vec<u64> = released
            .iter()
            .filter_map(|r| match r {
                Received::Frame { pts_micros, .. } => Some(*pts_micros),
                _ => None,
            })
            .collect();
        assert_eq!(
            stamps,
            vec![0, 1, 2],
            "frames must come out in sequence order"
        );
        assert_eq!(d.resyncs, 0, "reordering is not loss");
    }

    #[test]
    fn a_duplicated_frame_is_released_only_once() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        let mut dgs = Vec::new();
        f.send(
            Frame {
                data: &[4u8; 64],
                params: &sets,
                keyframe: true,
                pts_micros: 7,
            },
            |dg| dgs.push(dg.to_vec()),
        );
        let mut released = Vec::new();
        for dg in dgs.iter().chain(dgs.iter()) {
            let (h, b) = Header::parse(dg).expect("parses");
            d.push(h, b);
            released.extend(drain(&mut d));
        }
        let frames = released
            .iter()
            .filter(|r| matches!(r, Received::Frame { .. }))
            .count();
        assert_eq!(frames, 1, "a retransmitted frame must not be shown twice");
    }

    #[test]
    fn loss_is_measured_from_sequence_gaps() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        let frame = |k| Frame {
            data: &[1u8; 32],
            params: &sets,
            keyframe: k,
            pts_micros: 0,
        };

        assert_eq!(d.take_loss(), None, "nothing seen yet");

        // Ten frames, of which three never arrive.
        for seq in 0..10 {
            let mut datagrams = Vec::new();
            f.send(frame(seq == 0), |dg| datagrams.push(dg.to_vec()));
            if matches!(seq, 3 | 5 | 7) {
                continue;
            }
            for dg in &datagrams {
                let (h, b) = Header::parse(dg).expect("parses");
                d.push(h, b);
            }
            drain(&mut d);
        }
        // Frames still waiting behind the gap have to be let out before the
        // window can be judged.
        d.flush();
        drain(&mut d);
        let loss = d.take_loss().expect("a window was measured");
        assert!(
            (loss - 0.3).abs() < 0.01,
            "expected about 30% loss, got {loss}"
        );

        // The window resets, so a clean stretch reads as clean.
        for seq in 10..20 {
            let mut datagrams = Vec::new();
            f.send(frame(seq == 10), |dg| datagrams.push(dg.to_vec()));
            for dg in &datagrams {
                let (h, b) = Header::parse(dg).expect("parses");
                d.push(h, b);
            }
            drain(&mut d);
        }
        d.flush();
        drain(&mut d);
        assert_eq!(
            d.take_loss(),
            Some(0.0),
            "a clean window must read as clean"
        );
    }

    #[test]
    fn parity_rebuilds_a_lost_fragment() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        // Large enough to span several fragments and so carry parity.
        let data: Vec<u8> = (0..fec::BLOCK * 4 + 9).map(|i| (i % 251) as u8).collect();

        let mut datagrams = Vec::new();
        f.send(
            Frame {
                data: &data,
                params: &sets,
                keyframe: true,
                pts_micros: 11,
            },
            |dg| datagrams.push(dg.to_vec()),
        );
        let parity_count = datagrams
            .iter()
            .filter(|dg| Header::parse(dg).expect("parses").0.has(flags::PARITY))
            .count();
        assert!(parity_count > 0, "a multi-fragment frame must carry parity");

        // Drop the second picture-carrying datagram outright.
        let mut dropped = false;
        let mut results = Vec::new();
        for dg in &datagrams {
            let (header, body) = Header::parse(dg).expect("parses");
            if !header.has(flags::PARITY) && header.fragment_index == 1 && !dropped {
                dropped = true;
                continue;
            }
            d.push(header, body);
            results.extend(drain(&mut d));
        }

        assert_eq!(
            last(results),
            Received::Frame {
                seq: 0,
                params: Some(sets),
                data,
                pts_micros: 11
            },
            "the lost fragment must be rebuilt from parity, byte for byte"
        );
    }

    #[test]
    fn two_losses_in_one_group_are_rebuilt_with_two_parity_blocks() {
        let mut f = Framer::new();
        f.set_parity(2);
        let mut d = Depacketizer::new();
        let sets = params();
        let data: Vec<u8> = (0..fec::BLOCK * 5).map(|i| (i % 251) as u8).collect();
        let mut datagrams = Vec::new();
        f.send(
            Frame {
                data: &data,
                params: &sets,
                keyframe: true,
                pts_micros: 3,
            },
            |dg| datagrams.push(dg.to_vec()),
        );

        // Drop two picture-carrying datagrams from the same group.
        for dg in &datagrams {
            let (header, body) = Header::parse(dg).expect("parses");
            if !header.has(flags::PARITY) && matches!(header.fragment_index, 1 | 3) {
                continue;
            }
            d.push(header, body);
        }
        assert_eq!(
            last(drain(&mut d)),
            Received::Frame {
                seq: 0,
                params: Some(sets),
                data,
                pts_micros: 3
            },
            "two losses in one group must be rebuilt exactly"
        );
    }

    #[test]
    fn two_losses_in_one_group_are_not_recoverable_with_one_parity_block() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        let data: Vec<u8> = (0..fec::BLOCK * 4).map(|i| (i % 251) as u8).collect();
        let mut datagrams = Vec::new();
        f.send(
            Frame {
                data: &data,
                params: &sets,
                keyframe: true,
                pts_micros: 0,
            },
            |dg| datagrams.push(dg.to_vec()),
        );

        let mut results = Vec::new();
        for dg in &datagrams {
            let (header, body) = Header::parse(dg).expect("parses");
            if !header.has(flags::PARITY) && matches!(header.fragment_index, 1 | 2) {
                continue;
            }
            d.push(header, body);
            results.extend(drain(&mut d));
        }
        assert_eq!(
            last(results),
            Received::Pending,
            "one parity block cannot fix two holes"
        );
    }

    #[test]
    fn a_single_fragment_frame_carries_no_parity() {
        let mut f = Framer::new();
        let sets = params();
        let mut count = 0;
        // Parity for a lone fragment would just be a copy of it.
        f.send(
            Frame {
                data: &[1u8; 16],
                params: &sets,
                keyframe: false,
                pts_micros: 0,
            },
            |_| count += 1,
        );
        assert_eq!(count, 1, "no parity is worth sending for one fragment");
    }

    #[test]
    fn sequence_numbers_are_dense_and_start_at_zero() {
        let mut f = Framer::new();
        let sets = params();
        for expected in 0..5u64 {
            let seq = f.send(
                Frame {
                    data: &[0u8; 16],
                    params: &sets,
                    keyframe: false,
                    pts_micros: 0,
                },
                |_| {},
            );
            assert_eq!(seq, expected);
        }
    }

    #[test]
    fn a_frame_with_no_parameter_sets_still_carries_its_payload() {
        let (mut f, mut d) = (Framer::new(), Depacketizer::new());
        let sets = params();
        round_trip(
            &mut d,
            &mut f,
            Frame {
                data: &[1u8; 32],
                params: &sets,
                keyframe: true,
                pts_micros: 0,
            },
        );
        let got = last(round_trip(
            &mut d,
            &mut f,
            Frame {
                data: &[2u8; 32],
                params: &[],
                keyframe: false,
                pts_micros: 99,
            },
        ));
        assert_eq!(
            got,
            Received::Frame {
                seq: 1,
                params: None,
                data: vec![2u8; 32],
                pts_micros: 99
            }
        );
    }
}
