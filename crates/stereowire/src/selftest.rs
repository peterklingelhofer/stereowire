//! End-to-end check of the media path without needing a second machine, a
//! display, or Screen Recording permission.
//!
//! Synthetic frames go through the real encoder, the real fragmentation, real
//! UDP sockets, the real reassembler, and a real hardware decoder, and the
//! result is compared against the reference image. Audio takes the same trip
//! and is compared sample by sample.
//!
//! `--dump` captures the datagrams a live run produced, and `--replay` feeds
//! them back through the same delivery-and-verify path without an encoder, so
//! a platform that cannot encode yet can still be checked against a Mac's
//! output.

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use stereowire_proto::jitter::AudioJitter;
use stereowire_proto::packet::{fragment, Codec, Header, SampleRate, AUDIO_CHANNELS, MTU};
use stereowire_proto::packetize::AudioPacketizer;
use stereowire_proto::video::{Depacketizer, Frame, Framer, Received};

#[cfg(target_os = "macos")]
use crate::mac::decoder::Decoder;
#[cfg(target_os = "macos")]
use crate::mac::encoder::Encoder;
use crate::pattern::{psnr, TestPattern};
#[cfg(windows)]
use crate::win::decoder::Decoder;
#[cfg(windows)]
use crate::win::encoder::Encoder;

/// Below this, compression artifacts become visible. At the bitrates this
/// tool uses, a clean path scores far higher.
const MIN_PSNR_DB: f64 = 30.0;

/// Identifies a stereowire self-test dump, and the version of its layout.
const DUMP_MAGIC: &[u8; 8] = b"SWDUMP1\0";

pub struct Options {
    pub frames: u32,
    pub width: usize,
    pub height: usize,
    pub mbps: u32,
    pub fps: i32,
    /// Drop this percentage of datagrams to exercise loss handling.
    pub loss_percent: u32,
    /// Deliver datagrams up to this many positions out of order, as network
    /// jitter over a long path does.
    pub reorder: u32,
    /// Parity blocks per fragment group: one repairs a single loss, two repair
    /// any pair. Only `live_video` reads this: a replay carries its own
    /// parity, already baked into the dump.
    pub parity: usize,
    pub codec: Codec,
    /// Write every video datagram the encoder produced to this file.
    pub dump: Option<PathBuf>,
    /// Replay video datagrams from a file written with `dump`, instead of
    /// running an encoder.
    pub replay: Option<PathBuf>,
}

/// A pair of connected loopback sockets, standing in for the real link.
struct Loopback {
    sender: UdpSocket,
    receiver: UdpSocket,
    dropped: u64,
    loss_percent: u32,
    /// Seeded so a failing run can be reproduced exactly.
    rng: u64,
    /// Datagrams held back to arrive out of order.
    held: Vec<(u64, Vec<u8>)>,
    reorder: u32,
    tick: u64,
    pub reordered: u64,
}

impl Loopback {
    fn new(loss_percent: u32, reorder: u32) -> Result<Self> {
        let receiver =
            UdpSocket::bind("127.0.0.1:0").context("could not bind loopback receiver")?;
        // A large buffer keeps the kernel from discarding bursts of fragments
        // from a single large keyframe.
        receiver.set_read_timeout(Some(Duration::from_millis(50)))?;
        let sender = UdpSocket::bind("127.0.0.1:0").context("could not bind loopback sender")?;
        sender
            .connect(receiver.local_addr()?)
            .context("could not connect loopback sockets")?;
        Ok(Loopback {
            sender,
            receiver,
            dropped: 0,
            loss_percent,
            rng: 0x2545_F491_4F6C_DD1D,
            held: Vec::new(),
            reorder,
            tick: 0,
            reordered: 0,
        })
    }

    /// Sends one datagram, dropping a random share of them.
    ///
    /// The loss must be random rather than every Nth packet: periodic loss
    /// guarantees that any frame larger than the period loses a fragment, which
    /// is far harsher than a real link and hides how the stream actually behaves.
    fn send(&mut self, datagram: &[u8]) {
        if self.loss_percent > 0 && self.next_random() % 100 < u64::from(self.loss_percent) {
            self.dropped += 1;
            return;
        }
        self.tick += 1;

        // Jitter on a long path means packets do not arrive in the order they
        // were sent. Hold some back briefly so the receiver has to cope.
        if self.reorder > 0 {
            let delay = self.next_random() % u64::from(self.reorder + 1);
            if delay > 0 {
                self.reordered += 1;
                self.held.push((self.tick + delay, datagram.to_vec()));
            } else {
                let _ = self.sender.send(datagram);
            }
        } else {
            let _ = self.sender.send(datagram);
        }
        self.release_due();
    }

    /// Sends anything whose hold has expired.
    fn release_due(&mut self) {
        let tick = self.tick;
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].0 <= tick {
                let (_, datagram) = self.held.remove(i);
                let _ = self.sender.send(&datagram);
            } else {
                i += 1;
            }
        }
    }

    /// Flushes everything still held, for the end of a run.
    fn flush(&mut self) {
        for (_, datagram) in std::mem::take(&mut self.held) {
            let _ = self.sender.send(&datagram);
        }
    }

    /// xorshift64*, enough for shaping test traffic.
    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32
    }

    /// Drains whatever has arrived, without blocking for long.
    fn drain(&self, mut handle: impl FnMut(Header, &[u8])) {
        let mut buf = vec![0u8; MTU];
        while let Ok(len) = self.receiver.recv(&mut buf) {
            if let Some((header, body)) = Header::parse(&buf[..len]) {
                handle(header, body);
            }
        }
    }
}

pub fn run(options: Options) -> Result<()> {
    if options.dump.is_some() && options.replay.is_some() {
        bail!("--dump and --replay cannot both be given");
    }
    println!("stereowire self-test");
    if options.replay.is_none() {
        println!(
            "  {}x{}, {} frames at {} fps, {} Mbit/s, {} codec, {}% simulated packet loss",
            options.width,
            options.height,
            options.frames,
            options.fps,
            options.mbps,
            options.codec.name(),
            options.loss_percent
        );
    } else {
        println!("  {}% simulated packet loss", options.loss_percent);
    }

    let video = video_round_trip(&options)?;
    let audio = audio_round_trip(&options)?;

    println!();
    if video.passed && audio.passed {
        println!("PASS: video and audio both survived the round trip");
        Ok(())
    } else {
        bail!("self-test failed");
    }
}

struct Outcome {
    passed: bool,
}

/// Counts describing how the simulated link treated one run's datagrams.
struct DeliveryStats {
    discarded: u64,
    resyncs: u64,
    dropped: u64,
    reordered: u64,
    repaired: u64,
}

/// Accumulates what the depacketizer releases, shared by the encoder-driven
/// and the file-driven sources: only how the datagrams are produced differs
/// between them.
struct Delivery {
    video: Depacketizer,
    params: Option<Vec<Vec<u8>>>,
    codec: Option<Codec>,
    /// Decodable frames, as (bytes, timestamp).
    received: Vec<(Vec<u8>, u64)>,
    /// Set when a gap forced a resync; the encoder-driven source honours this
    /// by forcing a keyframe, the same recovery request the live sender gets.
    wants_keyframe: bool,
}

impl Delivery {
    fn new() -> Self {
        Delivery {
            video: Depacketizer::new(),
            params: None,
            codec: None,
            received: Vec::new(),
            wants_keyframe: false,
        }
    }

    /// Drains whatever the loopback has delivered so far into the depacketizer.
    fn drain(&mut self, link: &Loopback) {
        link.drain(|header, body| self.video.push(header, body));
        // Frames come out in order, and one arrival can release several when it
        // fills a gap, so keep polling until nothing more is ready.
        loop {
            match self.video.poll() {
                Received::Pending => break,
                Received::NeedKeyframe => self.wants_keyframe = true,
                Received::Frame {
                    params,
                    data,
                    pts_micros,
                    codec,
                    ..
                } => {
                    if let Some(sets) = params {
                        if self.params.is_none() {
                            self.params = Some(sets);
                            self.codec = Some(codec);
                        }
                    }
                    self.received.push((data, pts_micros));
                }
            }
        }
    }

    fn stats(&self, link: &Loopback) -> DeliveryStats {
        DeliveryStats {
            discarded: self.video.discarded,
            resyncs: self.video.resyncs,
            dropped: link.dropped,
            reordered: link.reordered,
            repaired: self.video.repaired(),
        }
    }
}

/// Captures the framer's raw output for later replay with `--replay`. The
/// file write itself only happens in `finish`, so recording a datagram can
/// never fail partway through a run. Only `live_video` builds one.
struct DumpWriter {
    path: PathBuf,
    width: usize,
    height: usize,
    fps: i32,
    frames: u32,
    body: Vec<u8>,
    count: u64,
}

impl DumpWriter {
    fn new(path: PathBuf, width: usize, height: usize, fps: i32, frames: u32) -> Self {
        DumpWriter {
            path,
            width,
            height,
            fps,
            frames,
            body: Vec::new(),
            count: 0,
        }
    }

    /// Records one datagram exactly as the framer emitted it, before it goes
    /// anywhere near the loopback's loss and reorder simulation.
    fn record(&mut self, datagram: &[u8]) {
        self.body
            .extend_from_slice(&(datagram.len() as u16).to_le_bytes());
        self.body.extend_from_slice(datagram);
        self.count += 1;
    }

    fn finish(self) -> Result<()> {
        let mut out = Vec::with_capacity(DUMP_MAGIC.len() + 16 + self.body.len());
        out.extend_from_slice(DUMP_MAGIC);
        out.extend_from_slice(&(self.width as u32).to_le_bytes());
        out.extend_from_slice(&(self.height as u32).to_le_bytes());
        out.extend_from_slice(&(self.fps as u32).to_le_bytes());
        out.extend_from_slice(&self.frames.to_le_bytes());
        out.extend_from_slice(&self.body);
        std::fs::write(&self.path, &out)
            .with_context(|| format!("could not write {}", self.path.display()))?;
        println!(
            "  video: wrote {} datagrams to {}",
            self.count,
            self.path.display()
        );
        Ok(())
    }
}

fn video_round_trip(options: &Options) -> Result<Outcome> {
    if let Some(path) = &options.replay {
        return replay_video(path, options);
    }
    live_video(options)
}

/// The encoder's input for frame `index`, in the form the platform's
/// capture delivers.
#[cfg(target_os = "macos")]
fn pattern_frame(
    pattern: &TestPattern,
    index: u32,
) -> Result<objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>> {
    Ok(pattern.frame(index)?.0)
}

#[cfg(windows)]
fn pattern_frame(pattern: &TestPattern, index: u32) -> Result<crate::pattern::Nv12Frame> {
    Ok(pattern.nv12(index))
}

/// Encodes the test pattern and delivers it, exactly as a live run does.
/// Optionally captures every datagram to a dump file along the way.
fn live_video(options: &Options) -> Result<Outcome> {
    let pattern = TestPattern {
        width: options.width,
        height: options.height,
    };
    let encoder = Encoder::new(
        options.codec,
        options.width as i32,
        options.height as i32,
        (options.mbps * 1_000_000) as i32,
        options.fps,
    )?;
    let mut link = Loopback::new(options.loss_percent, options.reorder)?;
    let mut framer = Framer::new();
    framer.set_parity(options.parity);
    let mut delivery = Delivery::new();
    let mut dump = options.dump.clone().map(|path| {
        DumpWriter::new(
            path,
            options.width,
            options.height,
            options.fps,
            options.frames,
        )
    });

    let mut sent_frames = 0u64;
    let mut keyframes = 0u64;
    let mut keyframe_requests = 0u64;
    let mut dump_forced_keyframes = 0u64;
    // Inputs submitted when the first output came back: how many frames the
    // encoder holds before it releases any
    let mut first_output_after = None;
    // Rate-limits recovery requests the way the real receiver does.
    let mut frames_since_request = u32::MAX;
    let request_interval = (options.fps.max(1) as u32) / 4;
    // A replay can never ask for a fresh keyframe, so a dump forces one more
    // often than live recovery would, trading a little size now for
    // resilience to whatever loss gets injected later at replay time.
    let dump_interval = (options.fps.max(1) as u32 / 4).max(1);

    for index in 0..options.frames {
        let input = pattern_frame(&pattern, index)?;

        let wants_recovery = delivery.wants_keyframe && frames_since_request >= request_interval;
        let dump_forced = dump.is_some() && index % dump_interval == 0;
        let force_key = index == 0 || wants_recovery || dump_forced;
        if force_key && index > 0 {
            if wants_recovery {
                keyframe_requests += 1;
                frames_since_request = 0;
                delivery.wants_keyframe = false;
            } else {
                dump_forced_keyframes += 1;
            }
        }
        frames_since_request = frames_since_request.saturating_add(1);

        let pts = u64::from(index) * 1_000_000 / options.fps.max(1) as u64;
        encoder.encode(&input, pts, force_key)?;

        for frame in encoder.drain() {
            first_output_after.get_or_insert(index + 1);
            if frame.keyframe {
                keyframes += 1;
            }
            framer.send(
                Frame {
                    data: &frame.data,
                    params: &frame.params,
                    keyframe: frame.keyframe,
                    pts_micros: frame.pts_micros,
                    codec: options.codec,
                },
                |dg| {
                    if let Some(dump) = dump.as_mut() {
                        dump.record(dg);
                    }
                    link.send(dg);
                },
            );
            sent_frames += 1;
            // Drain as we go so the socket buffer never overflows.
            delivery.drain(&link);
        }
    }

    encoder.finish()?;
    for frame in encoder.drain() {
        first_output_after.get_or_insert(options.frames);
        if frame.keyframe {
            keyframes += 1;
        }
        framer.send(
            Frame {
                data: &frame.data,
                params: &frame.params,
                keyframe: frame.keyframe,
                pts_micros: frame.pts_micros,
                codec: options.codec,
            },
            |dg| {
                if let Some(dump) = dump.as_mut() {
                    dump.record(dg);
                }
                link.send(dg);
            },
        );
        sent_frames += 1;
    }
    std::thread::sleep(Duration::from_millis(100));
    delivery.drain(&link);

    let latency = first_output_after.map_or_else(
        || "no output at all".to_string(),
        |inputs| format!("first output after {inputs} input frames"),
    );
    println!(
        "  video: encoded {sent_frames} frames ({keyframes} keyframes, {keyframe_requests} from \
         recovery requests, {dump_forced_keyframes} forced for the dump), {latency}"
    );

    if let Some(dump) = dump {
        dump.finish()?;
    }

    let stats = delivery.stats(&link);
    let params = delivery
        .params
        .context("no parameter sets ever arrived; nothing could be decoded")?;
    let codec = delivery.codec.expect("set alongside params");
    verify_received(
        &pattern,
        options.fps,
        options.loss_percent,
        codec,
        params,
        &delivery.received,
        stats,
    )
}

/// Reads a dump written by `--dump` and delivers its datagrams through the
/// same loopback and depacketizer a live run uses, without an encoder.
fn replay_video(path: &Path, options: &Options) -> Result<Outcome> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let header_len = DUMP_MAGIC.len() + 16;
    if bytes.len() < header_len || &bytes[..DUMP_MAGIC.len()] != DUMP_MAGIC {
        bail!("{} is not a stereowire self-test dump", path.display());
    }
    let field = |at: usize| -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("checked length"))
    };
    let at = DUMP_MAGIC.len();
    let width = field(at) as usize;
    let height = field(at + 4) as usize;
    let fps = field(at + 8) as i32;
    let frame_count = field(at + 12);

    println!(
        "  replaying {} ({width}x{height}, {frame_count} frames at {fps} fps); \
         --width, --height, --frames, --fps, --codec and --mbps are ignored on replay",
        path.display()
    );

    let mut link = Loopback::new(options.loss_percent, options.reorder)?;
    let mut delivery = Delivery::new();
    let mut datagram_count = 0u64;
    let mut cursor = header_len;
    while cursor + 2 <= bytes.len() {
        let len =
            u16::from_le_bytes(bytes[cursor..cursor + 2].try_into().expect("checked")) as usize;
        cursor += 2;
        if cursor + len > bytes.len() {
            break;
        }
        link.send(&bytes[cursor..cursor + len]);
        cursor += len;
        datagram_count += 1;
        delivery.drain(&link);
    }
    link.flush();
    std::thread::sleep(Duration::from_millis(100));
    delivery.drain(&link);

    println!("  video: replayed {datagram_count} datagrams from the dump");

    let stats = delivery.stats(&link);
    let params = delivery
        .params
        .context("no parameter sets in the dump; nothing could be decoded")?;
    let codec = delivery.codec.expect("set alongside params");
    let pattern = TestPattern { width, height };
    verify_received(
        &pattern,
        fps,
        options.loss_percent,
        codec,
        params,
        &delivery.received,
        stats,
    )
}

/// Decodes one frame and scores whatever comes out against the reference
/// pattern.
///
/// Frames are matched to their reference by timestamp rather than by
/// position: a decoder on another platform may release frames a call later,
/// so position-based matching would silently compare the wrong frames.
fn score_frame(
    decoder: &mut Decoder,
    pattern: &TestPattern,
    fps: i32,
    data: &[u8],
    pts_micros: u64,
    scores: &mut Vec<f64>,
) -> Result<bool> {
    if decoder.decode(data, pts_micros).is_err() {
        // A frame whose reference pictures were lost cannot decode, which is
        // an expected outcome of packet loss.
        return Ok(false);
    }
    score_drained(decoder, pattern, fps, scores)?;
    Ok(true)
}

/// Scores whatever the decoder currently has queued, without decoding
/// anything new. A decoder that pipelines internally (VideoToolbox does
/// not; Media Foundation can) only releases its last few frames once told
/// the stream has ended, which is what `finish` is for.
fn score_drained(
    decoder: &mut Decoder,
    pattern: &TestPattern,
    fps: i32,
    scores: &mut Vec<f64>,
) -> Result<()> {
    for decoded in decoder.drain() {
        if decoded.width != pattern.width || decoded.height != pattern.height {
            bail!(
                "decoded size {}x{} does not match the source {}x{}",
                decoded.width,
                decoded.height,
                pattern.width,
                pattern.height
            );
        }
        let index = ((decoded.pts_micros as f64) * f64::from(fps) / 1e6).round() as u32;
        if let Some(score) = psnr(&pattern.luma(index), &decoded.luma) {
            scores.push(score);
        }
    }
    Ok(())
}

/// Decodes every received frame, scores it, and prints the summary lines
/// shared by the encoder-driven and file-driven sources.
fn verify_received(
    pattern: &TestPattern,
    fps: i32,
    loss_percent: u32,
    codec: Codec,
    params: Vec<Vec<u8>>,
    received: &[(Vec<u8>, u64)],
    stats: DeliveryStats,
) -> Result<Outcome> {
    println!(
        "  video: {} frames decodable, {} discarded while out of sync, {} resyncs, {} dropped, {} reordered, {} fragments rebuilt by FEC",
        received.len(),
        stats.discarded,
        stats.resyncs,
        stats.dropped,
        stats.reordered,
        stats.repaired
    );

    let mut decoder = Decoder::new(codec, &params)?;
    let mut scores = Vec::new();
    let mut decode_errors = 0u64;
    for (data, pts) in received {
        if !score_frame(&mut decoder, pattern, fps, data, *pts, &mut scores)? {
            decode_errors += 1;
        }
    }
    // Nothing more is coming: release whatever the decoder was still
    // holding onto internally.
    decoder.finish()?;
    score_drained(&mut decoder, pattern, fps, &mut scores)?;

    if scores.is_empty() {
        bail!("no frames decoded; the video path is broken");
    }
    let worst = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let below_floor = scores.iter().filter(|&&s| s < MIN_PSNR_DB).count();
    println!(
        "  video: decoded {} frames ({decode_errors} undecodable), PSNR mean {mean:.1} dB, worst {worst:.1} dB",
        scores.len()
    );

    // On a clean link every frame must be good. With loss, frames referencing
    // lost data will be damaged; what matters is that the stream recovers and
    // the great majority land above the floor.
    let passed = if loss_percent == 0 {
        if worst < MIN_PSNR_DB {
            println!("  video: FAIL, worst frame below the {MIN_PSNR_DB} dB floor");
        }
        worst >= MIN_PSNR_DB
    } else {
        let good_ratio = 1.0 - below_floor as f64 / scores.len() as f64;
        if good_ratio < 0.9 {
            println!(
                "  video: FAIL, only {:.0}% of frames cleared the floor",
                good_ratio * 100.0
            );
        }
        good_ratio >= 0.9
    };
    Ok(Outcome { passed })
}

fn audio_round_trip(options: &Options) -> Result<Outcome> {
    let mut link = Loopback::new(options.loss_percent, options.reorder)?;
    let rate = SampleRate::Hz48000;
    let mut packetizer = AudioPacketizer::new(true, rate);
    let mut jitter = AudioJitter::new(0);

    // One second of a 440 Hz tone, which makes any discontinuity obvious.
    let frames = rate.hz() as usize;
    let source: Vec<f32> = (0..frames * AUDIO_CHANNELS)
        .map(|i| {
            let frame = i / AUDIO_CHANNELS;
            let phase = frame as f32 * 440.0 * std::f32::consts::TAU / rate.hz() as f32;
            phase.sin() * 0.5
        })
        .collect();

    // Feed in irregular chunks, the way ScreenCaptureKit delivers audio.
    let mut offset = 0;
    for chunk in [1024usize, 512, 2048, 700].iter().cycle() {
        if offset >= source.len() {
            break;
        }
        let end = (offset + chunk * AUDIO_CHANNELS).min(source.len());
        packetizer.push(&source[offset..end], 0, |header, payload| {
            let mut datagrams = Vec::new();
            fragment(header, payload, |dg| datagrams.push(dg.to_vec()));
            for dg in &datagrams {
                link.send(dg);
            }
        });
        offset = end;
        link.drain(|header, body| jitter.push(header, body));
    }
    link.flush();
    std::thread::sleep(Duration::from_millis(50));
    link.drain(|header, body| jitter.push(header, body));

    let available = jitter.depth();
    let mut out = vec![0.0f32; available];
    jitter.pull(&mut out);

    let stats = jitter.stats;
    println!();
    println!(
        "  audio: {} samples delivered, {} recovered by FEC, {} concealed, {} dropped in transit",
        out.len(),
        stats.recovered,
        stats.concealed,
        link.dropped
    );

    // If the very first packets were lost the receiver cannot know they existed,
    // so its timeline legitimately starts later. Find that alignment before
    // comparing, otherwise an offset stream reads as total corruption.
    let per_block = stereowire_proto::packet::AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS;
    let (offset, mismatches) = (0..8)
        .map(|block| {
            let shift = block * per_block;
            let n = out.len().saturating_sub(shift).min(source.len());
            let differing = out[..n]
                .iter()
                .zip(&source[shift..shift + n])
                .filter(|(a, b)| a != b)
                .count();
            (block, differing)
        })
        .min_by_key(|(_, differing)| *differing)
        .expect("at least one alignment");

    let compared = out
        .len()
        .saturating_sub(offset * per_block)
        .min(source.len());
    if compared == 0 {
        bail!("no audio arrived");
    }
    let exact = mismatches == 0;
    if exact {
        println!(
            "  audio: bit-exact over {compared} samples{}",
            if offset > 0 {
                format!(" (stream began {offset} block(s) in, the first packets were lost)")
            } else {
                String::new()
            }
        );
    } else {
        println!(
            "  audio: {mismatches} of {compared} samples differ ({} concealed blocks)",
            stats.concealed
        );
    }

    // Concealment is legitimate when two packets in a row are lost; a clean
    // link must be perfect.
    let passed = if options.loss_percent == 0 {
        exact
    } else {
        stats.concealed > 0 || exact
    };
    Ok(Outcome { passed })
}
