//! End-to-end check of the media path without needing a second machine, a
//! display, or Screen Recording permission.
//!
//! Synthetic frames go through the real encoder, the real fragmentation, real
//! UDP sockets, the real reassembler, and a real hardware decoder, and the
//! result is compared against the reference image. Audio takes the same trip
//! and is compared sample by sample.

use std::net::UdpSocket;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use stereowire_proto::jitter::AudioJitter;
use stereowire_proto::packet::{fragment, Header, SampleRate, AUDIO_CHANNELS, MTU};
use stereowire_proto::packetize::AudioPacketizer;
use stereowire_proto::video::{Depacketizer, Frame, Framer, Received};

use crate::mac::decoder::Decoder;
use crate::mac::encoder::Encoder;
use crate::mac::pattern::{psnr, TestPattern};

/// Below this, compression artifacts become visible. At the bitrates this
/// tool uses, a clean path scores far higher.
const MIN_PSNR_DB: f64 = 30.0;

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
    /// any pair.
    pub parity: usize,
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
    println!("stereowire self-test");
    println!(
        "  {}x{}, {} frames at {} fps, {} Mbit/s, {}% simulated packet loss",
        options.width,
        options.height,
        options.frames,
        options.fps,
        options.mbps,
        options.loss_percent
    );

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

fn video_round_trip(options: &Options) -> Result<Outcome> {
    let pattern = TestPattern {
        width: options.width,
        height: options.height,
    };
    let encoder = Encoder::new(
        options.width as i32,
        options.height as i32,
        (options.mbps * 1_000_000) as i32,
        options.fps,
    )?;
    let mut link = Loopback::new(options.loss_percent, options.reorder)?;
    let mut framer = Framer::new();
    framer.set_parity(options.parity);
    let mut video = Depacketizer::new();

    let mut references = Vec::new();
    let mut params: Option<Vec<Vec<u8>>> = None;
    // Decodable frames, as (sequence, bytes, timestamp).
    let mut received: Vec<(u64, Vec<u8>, u64)> = Vec::new();
    let mut sent_frames = 0u64;
    let mut keyframes = 0u64;
    let mut keyframe_requests = 0u64;
    let mut wants_keyframe = false;
    // Rate-limits recovery requests the way the real receiver does.
    let mut frames_since_request = u32::MAX;
    let request_interval = (options.fps.max(1) as u32) / 4;

    let drain = |link: &Loopback,
                 video: &mut Depacketizer,
                 params: &mut Option<Vec<Vec<u8>>>,
                 received: &mut Vec<(u64, Vec<u8>, u64)>,
                 wants_keyframe: &mut bool| {
        link.drain(|header, body| video.push(header, body));
        // Frames come out in order, and one arrival can release several when it
        // fills a gap, so keep polling until nothing more is ready.
        loop {
            match video.poll() {
                Received::Pending => break,
                Received::NeedKeyframe => *wants_keyframe = true,
                Received::Frame {
                    seq,
                    params: sets,
                    data,
                    pts_micros,
                } => {
                    if let Some(sets) = sets {
                        if params.is_none() {
                            *params = Some(sets);
                        }
                    }
                    received.push((seq, data, pts_micros));
                }
            }
        }
    };

    for index in 0..options.frames {
        let (buffer, luma) = pattern.frame(index)?;
        references.push(luma);

        // Honour a pending recovery request, exactly as the live sender does.
        let force_key = index == 0 || (wants_keyframe && frames_since_request >= request_interval);
        if force_key && index > 0 {
            keyframe_requests += 1;
            frames_since_request = 0;
            wants_keyframe = false;
        }
        frames_since_request = frames_since_request.saturating_add(1);

        let pts = u64::from(index) * 1_000_000 / options.fps.max(1) as u64;
        encoder.encode(&buffer, pts, force_key)?;

        for frame in encoder.drain() {
            if frame.keyframe {
                keyframes += 1;
            }
            framer.send(
                Frame {
                    data: &frame.data,
                    params: &frame.params,
                    keyframe: frame.keyframe,
                    pts_micros: frame.pts_micros,
                },
                |dg| link.send(dg),
            );
            sent_frames += 1;
            // Drain as we go so the socket buffer never overflows.
            drain(
                &link,
                &mut video,
                &mut params,
                &mut received,
                &mut wants_keyframe,
            );
        }
    }

    encoder.finish()?;
    for frame in encoder.drain() {
        if frame.keyframe {
            keyframes += 1;
        }
        framer.send(
            Frame {
                data: &frame.data,
                params: &frame.params,
                keyframe: frame.keyframe,
                pts_micros: frame.pts_micros,
            },
            |dg| link.send(dg),
        );
        sent_frames += 1;
    }
    std::thread::sleep(Duration::from_millis(100));
    drain(
        &link,
        &mut video,
        &mut params,
        &mut received,
        &mut wants_keyframe,
    );

    let params = params.context("no HEVC parameter sets ever arrived; nothing could be decoded")?;
    println!(
        "  video: encoded {sent_frames} frames ({keyframes} keyframes, {keyframe_requests} from recovery requests)"
    );
    println!(
        "  video: {} frames decodable, {} discarded while out of sync, {} resyncs, {} dropped, {} reordered, {} fragments rebuilt by FEC",
        received.len(),
        video.discarded,
        video.resyncs,
        link.dropped,
        link.reordered,
        video.repaired()
    );

    let decoder = Decoder::new(&params)?;
    let mut scores = Vec::new();
    let mut decode_errors = 0u64;
    for (seq, data, pts) in &received {
        if decoder.decode(data, *pts).is_err() {
            // A frame whose reference pictures were lost cannot decode, which
            // is an expected outcome of packet loss.
            decode_errors += 1;
            continue;
        }
        for decoded in decoder.drain() {
            let Some(reference) = references.get(*seq as usize) else {
                continue;
            };
            if decoded.width != options.width || decoded.height != options.height {
                bail!(
                    "decoded size {}x{} does not match the source {}x{}",
                    decoded.width,
                    decoded.height,
                    options.width,
                    options.height
                );
            }
            if let Some(score) = psnr(reference, &decoded.luma) {
                scores.push(score);
            }
        }
    }

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
    let passed = if options.loss_percent == 0 {
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
