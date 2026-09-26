//! Wire format for the stereowire link.
//!
//! The transport is plain UDP inside a Tailscale (WireGuard) tunnel, so the
//! protocol carries no encryption or authentication of its own. Tailscale's
//! default MTU is 1280, which sets the datagram budget below.

/// Total datagram budget, matching Tailscale's default 1280-byte MTU.
pub const MTU: usize = 1280;
/// Fixed header prefixed to every datagram.
pub const HEADER_LEN: usize = 24;
/// Largest payload that still fits a single datagram.
pub const MAX_PAYLOAD: usize = MTU - HEADER_LEN;

pub const VERSION: u8 = 1;

/// Audio frames per packet. 64 frames at 48 kHz is 1.33 ms. The size is capped
/// by the FEC scheme: a packet carries its own block plus a copy of the
/// previous one, and both must still fit a single datagram.
pub const AUDIO_FRAMES_PER_PACKET: usize = 64;
pub const AUDIO_CHANNELS: usize = 2;
/// Bytes of one packet's worth of interleaved f32 stereo. Independent of the
/// sample rate: only the packet *rate* changes with it.
pub const AUDIO_BLOCK_BYTES: usize = AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS * 4;

/// Sample rates the link can carry.
///
/// The rate travels in every audio packet rather than being agreed up front, so
/// a receiver that joins mid-stream, or after a restart, is never left guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SampleRate {
    Hz44100 = 1,
    Hz48000 = 2,
    Hz88200 = 3,
    Hz96000 = 4,
}

impl SampleRate {
    pub fn hz(self) -> u32 {
        match self {
            SampleRate::Hz44100 => 44_100,
            SampleRate::Hz48000 => 48_000,
            SampleRate::Hz88200 => 88_200,
            SampleRate::Hz96000 => 96_000,
        }
    }

    pub fn from_hz(hz: u32) -> Option<Self> {
        match hz {
            44_100 => Some(SampleRate::Hz44100),
            48_000 => Some(SampleRate::Hz48000),
            88_200 => Some(SampleRate::Hz88200),
            96_000 => Some(SampleRate::Hz96000),
            _ => None,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(SampleRate::Hz44100),
            2 => Some(SampleRate::Hz48000),
            3 => Some(SampleRate::Hz88200),
            4 => Some(SampleRate::Hz96000),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Video = 0,
    Audio = 1,
    /// Receiver -> sender. Currently only used to request a fresh keyframe.
    Control = 2,
}

/// Video codecs the wire format can carry.
///
/// The codec travels in every video packet rather than being agreed up front,
/// the same way [`SampleRate`] travels with audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    Hevc = 1,
    H264 = 2,
}

impl Codec {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Codec::Hevc),
            2 => Some(Codec::H264),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::Hevc => "HEVC",
            Codec::H264 => "H.264",
        }
    }
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Kind::Video),
            1 => Some(Kind::Audio),
            2 => Some(Kind::Control),
            _ => None,
        }
    }
}

pub mod flags {
    /// Video: this frame is an IDR.
    pub const KEYFRAME: u8 = 1 << 0;
    /// Video: payload begins with a parameter-set blob, VPS/SPS/PPS for HEVC
    /// or SPS/PPS for H.264 (see `params`).
    pub const HAS_PARAMS: u8 = 1 << 1;
    /// Audio: payload carries the previous packet's block after the current one.
    pub const HAS_FEC: u8 = 1 << 2;
    /// Video: this datagram is a parity block covering the group of fragments
    /// starting at its `fragment_index`.
    pub const PARITY: u8 = 1 << 7;
}

/// Flags for [`Kind::Control`] packets.
///
/// These deliberately use different bits from the media flags above, so a
/// packet read with the wrong kind in mind is inert rather than ambiguous.
pub mod control {
    /// Receiver lost sync and needs a fresh IDR.
    pub const KEYFRAME_REQUEST: u8 = 1 << 3;
    /// Latency probe. `pts_micros` carries the sender's clock reading.
    pub const PING: u8 = 1 << 4;
    /// Echo of a [`PING`], carrying its `pts_micros` back unchanged.
    pub const PONG: u8 = 1 << 5;
    /// Receiver's feedback: requested video bitrate and parity blocks, as two
    /// little-endian u32s in the body.
    pub const BITRATE: u8 = 1 << 6;
}

/// Builds a control datagram. `stamp_micros` is echoed verbatim by a pong, so
/// round-trip time can be derived without the two clocks agreeing.
pub fn control_datagram(flag: u8, stamp_micros: u64) -> [u8; HEADER_LEN] {
    let header = Header {
        kind: Kind::Control,
        fragment_index: 0,
        fragment_count: 1,
        seq: 0,
        pts_micros: stamp_micros,
        flags: flag,
        rate: None,
        codec: None,
    };
    let mut out = [0u8; HEADER_LEN];
    header.write(&mut out);
    out
}

/// Fixed 24-byte header. All integers little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    /// Index of this fragment within its frame.
    pub fragment_index: u16,
    /// Total fragments the frame was split into.
    pub fragment_count: u16,
    /// Video: frame number. Audio: packet sequence number.
    pub seq: u64,
    /// Capture time on the sender's monotonic clock.
    pub pts_micros: u64,
    pub flags: u8,
    /// Audio: sample rate. Ignored for other kinds.
    pub rate: Option<SampleRate>,
    /// Video: the codec `data` is encoded with. Ignored for other kinds.
    pub codec: Option<Codec>,
}

impl Header {
    pub fn write(&self, out: &mut [u8; HEADER_LEN]) {
        out[0] = VERSION;
        out[1] = self.kind as u8;
        out[2..4].copy_from_slice(&self.fragment_index.to_le_bytes());
        out[4..6].copy_from_slice(&self.fragment_count.to_le_bytes());
        out[6..14].copy_from_slice(&self.seq.to_le_bytes());
        out[14..22].copy_from_slice(&self.pts_micros.to_le_bytes());
        out[22] = self.flags;
        // Byte 23's meaning depends on the kind: a sample rate for audio, a
        // codec for video, and nothing at all for control.
        out[23] = match self.kind {
            Kind::Audio => self.rate.map_or(0, |rate| rate as u8),
            Kind::Video => self.codec.map_or(0, |codec| codec as u8),
            Kind::Control => 0,
        };
    }

    pub fn parse(buf: &[u8]) -> Option<(Header, &[u8])> {
        if buf.len() < HEADER_LEN || buf[0] != VERSION {
            return None;
        }
        let kind = Kind::from_u8(buf[1])?;
        let header = Header {
            kind,
            fragment_index: u16::from_le_bytes([buf[2], buf[3]]),
            fragment_count: u16::from_le_bytes([buf[4], buf[5]]),
            seq: u64::from_le_bytes(buf[6..14].try_into().ok()?),
            pts_micros: u64::from_le_bytes(buf[14..22].try_into().ok()?),
            flags: buf[22],
            rate: (kind == Kind::Audio)
                .then(|| SampleRate::from_code(buf[23]))
                .flatten(),
            // A v0.1.0 sender never wrote this byte and left it zero, which is
            // not a valid codec code, so it parses as None here; the caller
            // decides how to treat that absence.
            codec: (kind == Kind::Video)
                .then(|| Codec::from_code(buf[23]))
                .flatten(),
        };
        if header.fragment_count == 0 || header.fragment_index >= header.fragment_count {
            return None;
        }
        Some((header, &buf[HEADER_LEN..]))
    }

    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// Splits `payload` across datagrams, invoking `emit` once per fragment.
pub fn fragment(header: Header, payload: &[u8], mut emit: impl FnMut(&[u8])) {
    let count = payload.len().div_ceil(MAX_PAYLOAD).max(1);
    let mut buf = [0u8; MTU];
    for (index, chunk) in payload
        .chunks(MAX_PAYLOAD)
        .chain(
            // A zero-length payload still needs one datagram to carry its header.
            std::iter::once(&[][..]).take(usize::from(payload.is_empty())),
        )
        .enumerate()
    {
        let header = Header {
            fragment_index: index as u16,
            fragment_count: count as u16,
            ..header
        };
        let mut head = [0u8; HEADER_LEN];
        header.write(&mut head);
        buf[..HEADER_LEN].copy_from_slice(&head);
        buf[HEADER_LEN..HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        emit(&buf[..HEADER_LEN + chunk.len()]);
    }
}

/// Builds the receiver's feedback datagram: the video bitrate it wants, and how
/// many parity blocks each fragment group should carry.
pub fn control_feedback_datagram(flag: u8, bitrate_bps: u32, parity: u32) -> [u8; HEADER_LEN + 8] {
    let mut out = [0u8; HEADER_LEN + 8];
    out[..HEADER_LEN].copy_from_slice(&control_datagram(flag, 0));
    out[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&bitrate_bps.to_le_bytes());
    out[HEADER_LEN + 4..].copy_from_slice(&parity.to_le_bytes());
    out
}

/// Reads a [`control_feedback_datagram`] body as (bitrate, parity blocks).
pub fn control_feedback(body: &[u8]) -> Option<(u32, u32)> {
    let bitrate = u32::from_le_bytes(body.get(..4)?.try_into().ok()?);
    let parity = u32::from_le_bytes(body.get(4..8)?.try_into().ok()?);
    Some((bitrate, parity))
}

/// Encodes parameter sets (VPS/SPS/PPS for HEVC, or SPS/PPS for H.264) as
/// `count:u8` then `len:u32 || bytes` per set.
pub fn write_params(sets: &[Vec<u8>], out: &mut Vec<u8>) {
    out.push(sets.len() as u8);
    for set in sets {
        out.extend_from_slice(&(set.len() as u32).to_le_bytes());
        out.extend_from_slice(set);
    }
}

/// Inverse of [`write_params`], returning the sets and the remaining frame data.
pub fn read_params(buf: &[u8]) -> Option<(Vec<Vec<u8>>, &[u8])> {
    let (&count, mut rest) = buf.split_first()?;
    let mut sets = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if rest.len() < 4 {
            return None;
        }
        let (len_bytes, tail) = rest.split_at(4);
        let len = u32::from_le_bytes(len_bytes.try_into().ok()?) as usize;
        if tail.len() < len {
            return None;
        }
        sets.push(tail[..len].to_vec());
        rest = &tail[len..];
    }
    Some((sets, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: Kind, seq: u64) -> Header {
        Header {
            kind,
            fragment_index: 0,
            fragment_count: 1,
            seq,
            pts_micros: 42,
            flags: 0,
            rate: None,
            codec: None,
        }
    }

    #[test]
    fn header_roundtrips() {
        let h = Header {
            kind: Kind::Video,
            fragment_index: 3,
            fragment_count: 9,
            seq: u64::MAX / 3,
            pts_micros: 1234567,
            flags: flags::KEYFRAME | flags::HAS_PARAMS,
            rate: None,
            codec: None,
        };
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);
        let (parsed, rest) = Header::parse(&buf).expect("parses");
        assert_eq!(parsed, h);
        assert!(rest.is_empty());
    }

    #[test]
    fn rejects_bad_headers() {
        let mut buf = [0u8; HEADER_LEN];
        header(Kind::Video, 0).write(&mut buf);
        assert!(
            Header::parse(&buf[..HEADER_LEN - 1]).is_none(),
            "short buffer"
        );

        let mut wrong_version = buf;
        wrong_version[0] = VERSION + 1;
        assert!(Header::parse(&wrong_version).is_none(), "version mismatch");

        let mut bad_kind = buf;
        bad_kind[1] = 7;
        assert!(Header::parse(&bad_kind).is_none(), "unknown kind");

        // fragment_index must be inside fragment_count
        let mut oob = buf;
        oob[2..4].copy_from_slice(&5u16.to_le_bytes());
        oob[4..6].copy_from_slice(&2u16.to_le_bytes());
        assert!(Header::parse(&oob).is_none(), "index past count");

        let mut zero_count = buf;
        zero_count[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert!(Header::parse(&zero_count).is_none(), "zero fragment count");
    }

    #[test]
    fn fragments_cover_payload_exactly() {
        for len in [
            0usize,
            1,
            MAX_PAYLOAD - 1,
            MAX_PAYLOAD,
            MAX_PAYLOAD + 1,
            100_000,
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut seen = Vec::new();
            fragment(header(Kind::Video, 1), &payload, |dg| {
                assert!(dg.len() <= MTU, "datagram exceeds MTU");
                let (h, body) = Header::parse(dg).expect("parses");
                seen.push((h.fragment_index, h.fragment_count, body.to_vec()));
            });
            let count = seen[0].1;
            assert_eq!(
                seen.len(),
                count as usize,
                "emitted fragment count matches header"
            );
            for (i, entry) in seen.iter().enumerate() {
                assert_eq!(entry.0 as usize, i, "fragments are in order");
            }
            let rejoined: Vec<u8> = seen.iter().flat_map(|s| s.2.clone()).collect();
            assert_eq!(rejoined, payload, "len {len} roundtrips");
        }
    }

    #[test]
    fn sample_rate_survives_the_header() {
        for rate in [
            SampleRate::Hz44100,
            SampleRate::Hz48000,
            SampleRate::Hz88200,
            SampleRate::Hz96000,
        ] {
            let mut h = header(Kind::Audio, 3);
            h.rate = Some(rate);
            let mut buf = [0u8; HEADER_LEN];
            h.write(&mut buf);
            let (parsed, _) = Header::parse(&buf).expect("parses");
            assert_eq!(parsed.rate, Some(rate));
            assert_eq!(
                rate.hz(),
                SampleRate::from_hz(rate.hz()).expect("known").hz()
            );
        }
    }

    #[test]
    fn an_unknown_rate_code_reads_as_absent_rather_than_wrong() {
        let mut buf = [0u8; HEADER_LEN];
        header(Kind::Audio, 1).write(&mut buf);
        buf[23] = 200;
        let (parsed, _) = Header::parse(&buf).expect("parses");
        assert_eq!(
            parsed.rate, None,
            "a rate we do not know must not be guessed"
        );
        assert!(SampleRate::from_hz(37_000).is_none());
    }

    #[test]
    fn video_header_round_trips_the_codec() {
        for codec in [Codec::Hevc, Codec::H264] {
            let mut h = header(Kind::Video, 5);
            h.codec = Some(codec);
            let mut buf = [0u8; HEADER_LEN];
            h.write(&mut buf);
            let (parsed, _) = Header::parse(&buf).expect("parses");
            assert_eq!(parsed.codec, Some(codec));
        }
    }

    #[test]
    fn audio_header_round_trips_the_rate_and_carries_no_codec() {
        let mut h = header(Kind::Audio, 3);
        h.rate = Some(SampleRate::Hz48000);
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);
        let (parsed, _) = Header::parse(&buf).expect("parses");
        assert_eq!(parsed.rate, Some(SampleRate::Hz48000));
        assert_eq!(parsed.codec, None, "byte 23 means something else for audio");
    }

    #[test]
    fn a_video_header_with_byte_23_zero_reads_as_no_codec() {
        // A v0.1.0 sender never wrote this byte, leaving it zero.
        let mut buf = [0u8; HEADER_LEN];
        header(Kind::Video, 1).write(&mut buf);
        buf[23] = 0;
        let (parsed, _) = Header::parse(&buf).expect("parses");
        assert_eq!(parsed.codec, None);
    }

    #[test]
    fn control_datagrams_roundtrip() {
        for flag in [control::KEYFRAME_REQUEST, control::PING, control::PONG] {
            let stamp = 1234567890u64;
            let datagram = control_datagram(flag, stamp);
            let (header, body) = Header::parse(&datagram).expect("parses");
            assert_eq!(header.kind, Kind::Control);
            assert_eq!(header.flags, flag);
            assert_eq!(
                header.pts_micros, stamp,
                "the stamp must survive for RTT maths"
            );
            assert!(body.is_empty());
        }
    }

    #[test]
    fn control_feedback_roundtrips() {
        for (bitrate, parity) in [(0u32, 0u32), (5_000_000, 1), (u32::MAX, 2)] {
            let datagram = control_feedback_datagram(control::BITRATE, bitrate, parity);
            let (header, body) = Header::parse(&datagram).expect("parses");
            assert_eq!(header.kind, Kind::Control);
            assert!(header.has(control::BITRATE));
            assert_eq!(control_feedback(body), Some((bitrate, parity)));
        }
        // A truncated body must be refused rather than read as garbage.
        assert_eq!(control_feedback(&[1, 2, 3, 4, 5]), None);
        assert_eq!(control_feedback(&[]), None);
    }

    #[test]
    fn control_flags_do_not_collide_with_media_flags() {
        let media = flags::KEYFRAME | flags::HAS_PARAMS | flags::HAS_FEC | flags::PARITY;
        let ctrl = control::KEYFRAME_REQUEST | control::PING | control::PONG | control::BITRATE;
        assert_eq!(
            media & ctrl,
            0,
            "a stray flag must never read as the other kind"
        );
    }

    #[test]
    fn params_roundtrip() {
        let sets = vec![vec![1u8, 2, 3], vec![9u8; 300], vec![]];
        let mut buf = Vec::new();
        write_params(&sets, &mut buf);
        buf.extend_from_slice(b"framedata");
        let (parsed, rest) = read_params(&buf).expect("parses");
        assert_eq!(parsed, sets);
        assert_eq!(rest, b"framedata");
    }

    #[test]
    fn truncated_params_rejected() {
        let sets = vec![vec![1u8, 2, 3], vec![7u8; 50]];
        let mut buf = Vec::new();
        write_params(&sets, &mut buf);
        for cut in 1..buf.len() {
            // Truncation must be detected, never panic or over-read.
            let _ = read_params(&buf[..cut]);
        }
        assert!(read_params(&buf[..buf.len() - 1]).is_none());
    }
}
