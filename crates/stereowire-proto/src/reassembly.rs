//! Reassembles fragmented video frames arriving out of order over UDP.

use std::collections::HashMap;

use crate::fec;
use crate::packet::{flags, Header};

/// How many frames behind the newest arrival we keep trying to complete.
/// Anything older is stale for a live stream and is dropped.
const STALE_DISTANCE: u64 = 8;

struct Partial {
    header: Header,
    fragments: Vec<Option<Vec<u8>>>,
    received: usize,
    /// Parity blocks, keyed by the group they cover and which block they are.
    parity: HashMap<(u16, u8), Vec<u8>>,
}

impl Partial {
    /// Rebuilds any group that is missing exactly one fragment.
    ///
    /// Runs after every arrival, because the arrival may be either the parity
    /// that completes a group or the second-to-last fragment that makes one
    /// recoverable.
    fn repair(&mut self, group: usize) -> usize {
        let starts: Vec<u16> = {
            let mut starts: Vec<u16> = self.parity.keys().map(|(start, _)| *start).collect();
            starts.sort_unstable();
            starts.dedup();
            starts
        };

        let mut repaired = 0;
        for start in starts {
            let first = start as usize;
            let end = (first + group).min(self.fragments.len());
            if first >= end {
                continue;
            }
            let missing = (first..end)
                .filter(|&i| self.fragments[i].is_none())
                .count();
            // Nothing to do, or more holes than two parity blocks can describe.
            if missing == 0 || missing > fec::MAX_PARITY {
                continue;
            }

            let mut window: Vec<Option<Vec<u8>>> = self.fragments[first..end].to_vec();
            let p = self.parity.get(&(start, 0)).map(Vec::as_slice);
            let q = self.parity.get(&(start, 1)).map(Vec::as_slice);
            let rebuilt = fec::recover_group(&mut window, p, q);
            if rebuilt > 0 {
                self.fragments[first..end].clone_from_slice(&window);
                repaired += rebuilt;
            }
        }
        self.received += repaired;
        repaired
    }
}

pub struct Reassembler {
    partials: HashMap<u64, Partial>,
    /// Fragments rebuilt from parity rather than received.
    pub repaired: u64,
    newest_seq: u64,
    /// Frames abandoned because fragments never arrived.
    pub dropped: u64,
}

/// A fully reassembled frame.
pub struct Frame {
    pub header: Header,
    pub data: Vec<u8>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Reassembler {
            partials: HashMap::new(),
            repaired: 0,
            newest_seq: 0,
            dropped: 0,
        }
    }

    /// Feeds one fragment in. Returns the frame once every fragment has arrived.
    pub fn push(&mut self, header: Header, body: &[u8]) -> Option<Frame> {
        // A frame this old is past its display time; ignore it rather than
        // resurrecting an entry we just evicted. Ordering and duplicate
        // suppression happen downstream, where release order is decided.
        if header.seq + STALE_DISTANCE < self.newest_seq {
            return None;
        }
        self.newest_seq = self.newest_seq.max(header.seq);

        let count = header.fragment_count as usize;
        let entry = self.partials.entry(header.seq).or_insert_with(|| Partial {
            header,
            fragments: vec![None; count],
            received: 0,
            parity: HashMap::new(),
        });

        // A peer that restarts could reuse a sequence number with a new shape.
        if entry.fragments.len() != count {
            *entry = Partial {
                header,
                fragments: vec![None; count],
                received: 0,
                parity: HashMap::new(),
            };
        }

        if header.has(flags::PARITY) {
            // The first byte says which parity block this is.
            let (&level, block) = body.split_first()?;
            entry
                .parity
                .insert((header.fragment_index, level), block.to_vec());
        } else {
            // Fragments carry their own length so a rebuilt one knows its size.
            let data = fec::decode_fragment(body)?;
            let slot = &mut entry.fragments[header.fragment_index as usize];
            if slot.is_none() {
                *slot = Some(data.to_vec());
                entry.received += 1;
            }
        }
        self.repaired += entry.repair(fec::GROUP) as u64;
        let entry = self.partials.get_mut(&header.seq).expect("just inserted");
        // Parameter sets ride only on the first fragment, so keep the flags
        // from whichever fragment actually carried them.
        if header.has(crate::packet::flags::HAS_PARAMS) {
            entry.header.flags |= crate::packet::flags::HAS_PARAMS;
        }

        if entry.received != count {
            self.evict_stale();
            return None;
        }

        let entry = self
            .partials
            .remove(&header.seq)
            .expect("just checked complete");
        let mut data = Vec::with_capacity(entry.fragments.iter().flatten().map(Vec::len).sum());
        for fragment in entry.fragments.into_iter() {
            data.extend_from_slice(&fragment.expect("all fragments present"));
        }
        self.evict_stale();
        Some(Frame {
            header: entry.header,
            data,
        })
    }

    fn evict_stale(&mut self) {
        let cutoff = self.newest_seq.saturating_sub(STALE_DISTANCE);
        let before = self.partials.len();
        self.partials.retain(|&seq, _| seq >= cutoff);
        self.dropped += (before - self.partials.len()) as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Header;
    use crate::video::{Frame as VideoFrame, Framer};

    /// The largest payload a protected fragment carries, which is what the
    /// framer chunks to.
    const MAX_PAYLOAD: usize = fec::BLOCK;

    /// Builds datagrams exactly as the sender does, parity included.
    fn datagrams(seq: u64, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut framer = Framer::new();
        let mut out = Vec::new();
        // Framer numbers frames itself, so skip forward to the sequence wanted.
        for _ in 0..seq {
            framer.send(
                VideoFrame {
                    data: &[],
                    params: &[],
                    keyframe: false,
                    pts_micros: 0,
                },
                |_| {},
            );
        }
        framer.send(
            VideoFrame {
                data: payload,
                params: &[],
                keyframe: true,
                pts_micros: seq,
            },
            |dg| out.push(dg.to_vec()),
        );
        out
    }

    /// Only the picture-carrying datagrams, for tests about plain reassembly.
    fn data_only(seq: u64, payload: &[u8]) -> Vec<Vec<u8>> {
        datagrams(seq, payload)
            .into_iter()
            .filter(|dg| {
                let (h, _) = Header::parse(dg).expect("parses");
                !h.has(crate::packet::flags::PARITY)
            })
            .collect()
    }

    fn feed(r: &mut Reassembler, dg: &[u8]) -> Option<Frame> {
        let (h, body) = Header::parse(dg).expect("parses");
        r.push(h, body)
    }

    #[test]
    fn reassembles_in_order() {
        let payload: Vec<u8> = (0..MAX_PAYLOAD * 3 + 17).map(|i| (i % 251) as u8).collect();
        let dgs = data_only(1, &payload);
        let mut r = Reassembler::new();
        let mut done = None;
        for dg in &dgs {
            done = feed(&mut r, dg).or(done);
        }
        assert_eq!(done.expect("completed").data, payload);
    }

    #[test]
    fn reassembles_fragments_arriving_out_of_order() {
        let payload: Vec<u8> = (0..MAX_PAYLOAD * 4).map(|i| (i % 251) as u8).collect();
        let mut dgs = data_only(7, &payload);
        dgs.reverse();
        let mut r = Reassembler::new();
        let mut completed = Vec::new();
        for dg in dgs.iter() {
            if let Some(f) = feed(&mut r, dg) {
                completed.push(f);
            }
        }
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].data, payload,
            "order of arrival must not matter"
        );
    }

    #[test]
    fn a_frame_completing_out_of_order_is_still_reassembled() {
        let mut r = Reassembler::new();
        // Frame 5 arrives first. Frame 4 must still be rebuilt when it lands,
        // because putting frames back in order is the depacketizer's job and it
        // cannot order what it never receives.
        assert!(feed(&mut r, &data_only(5, b"newer")[0]).is_some());
        assert!(feed(&mut r, &data_only(4, b"older")[0]).is_some());
        assert!(feed(&mut r, &data_only(6, b"newest")[0]).is_some());
    }

    #[test]
    fn incomplete_frame_never_emits() {
        let payload = vec![0u8; MAX_PAYLOAD * 3];
        let dgs = data_only(1, &payload);
        let mut r = Reassembler::new();
        for dg in &dgs[..dgs.len() - 1] {
            assert!(feed(&mut r, dg).is_none());
        }
    }

    #[test]
    fn stale_partials_are_evicted() {
        let big = vec![0u8; MAX_PAYLOAD * 3];
        let mut r = Reassembler::new();
        // Abandon frame 1 after a single fragment.
        feed(&mut r, &data_only(1, &big)[0]);
        // Push enough newer complete frames to move the cutoff past it.
        for seq in 2..20 {
            let dgs = data_only(seq, b"small");
            assert!(feed(&mut r, &dgs[0]).is_some());
        }
        assert!(r.partials.is_empty(), "stale partial should be gone");
        assert!(r.dropped > 0, "eviction should be counted");
        // A late fragment for the abandoned frame must not allocate anew.
        feed(&mut r, &data_only(1, &big)[1]);
        assert!(
            r.partials.is_empty(),
            "late fragment must not resurrect a stale frame"
        );
    }
}
