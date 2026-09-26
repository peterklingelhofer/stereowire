//! Sending side: capture, encode, packetize, transmit.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use objc2_core_video::CVImageBuffer;
use stereowire_proto::packet::{
    control, control_datagram, control_feedback, fragment, Codec, Header, Kind, SampleRate,
};
use stereowire_proto::packetize::AudioPacketizer;
use stereowire_proto::video::{Frame, Framer};

use crate::mac::capture::{Capture, CaptureOptions, CaptureSink};
use crate::mac::encoder::Encoder;
use crate::net::Link;

struct EncoderSettings {
    codec: Codec,
    bitrate_bps: i32,
    fps: i32,
    fec: bool,
    min_bitrate_bps: i32,
    congestion_control: bool,
    /// What we asked the capture for; the stream may say otherwise.
    sample_rate: SampleRate,
}

pub struct Options {
    pub peer: String,
    pub port: u16,
    pub codec: Codec,
    pub bitrate_bps: i32,
    pub fps: i32,
    pub max_width: i32,
    pub fec: bool,
    pub show_cursor: bool,
    pub sample_rate: SampleRate,
    /// Floor the receiver may not push the bitrate below.
    pub min_bitrate_bps: i32,
    pub congestion_control: bool,
    /// Send to an address outside the tunnel anyway.
    pub allow_untunnelled: bool,
}

struct Sender {
    link: Arc<Link>,
    /// Built from the first frame, so the encoder matches exactly what
    /// ScreenCaptureKit delivers rather than what we predicted it would.
    encoder: Mutex<Option<Encoder>>,
    settings: EncoderSettings,
    /// Built once the capture reveals the rate it is actually running at.
    audio: Mutex<Option<AudioPacketizer>>,
    framer: Mutex<Framer>,
    /// Set when the receiver asks for a fresh keyframe.
    keyframe_wanted: Arc<AtomicBool>,
    /// Set by the control thread when the receiver asks for a different rate.
    requested_bps: Arc<AtomicU64>,
    /// Parity blocks per group the receiver has asked for.
    requested_parity: Arc<std::sync::atomic::AtomicUsize>,
    frames_sent: AtomicU64,
    bytes_sent: AtomicU64,
    audio_packets: AtomicU64,
    /// Bitrate currently configured on the encoder.
    current_bps: std::sync::atomic::AtomicI32,
    /// Loudest sample seen since the last report, scaled to 0..10000.
    audio_peak: AtomicU64,
}

impl Sender {
    /// Adopts a bitrate the receiver asked for, clamped to what this sender was
    /// configured to allow. Small changes are ignored so the encoder is not
    /// reconfigured constantly.
    fn apply_bitrate_request(&self, encoder: &Encoder) {
        let requested = self.requested_bps.swap(0, Ordering::Relaxed);
        if requested == 0 || !self.settings.congestion_control {
            return;
        }
        let target =
            (requested as i32).clamp(self.settings.min_bitrate_bps, self.settings.bitrate_bps);
        let current = self.current_bps.load(Ordering::Relaxed);
        if (target - current).abs() * 20 < current {
            return;
        }
        match encoder.set_bitrate(target) {
            Ok(()) => {
                self.current_bps.store(target, Ordering::Relaxed);
                println!("video bitrate now {:.1} Mbit/s", target as f64 / 1e6);
            }
            Err(error) => eprintln!("could not change bitrate: {error}"),
        }
    }
}

impl CaptureSink for Sender {
    fn on_video(&self, image: &CVImageBuffer, pts_micros: u64) {
        let mut slot = self.encoder.lock().expect("encoder mutex not poisoned");
        let encoder = match &*slot {
            Some(encoder) => encoder,
            None => {
                let size = objc2_core_video::CVImageBufferGetEncodedSize(image);
                // Encoders require even dimensions.
                let width = (size.width as i32) & !1;
                let height = (size.height as i32) & !1;
                if width <= 0 || height <= 0 {
                    return;
                }
                match Encoder::new(
                    self.settings.codec,
                    width,
                    height,
                    self.settings.bitrate_bps,
                    self.settings.fps,
                ) {
                    Ok(encoder) => {
                        println!(
                            "encoding {width}x{height} {} at {} fps, {} Mbit/s",
                            self.settings.codec.name(),
                            self.settings.fps,
                            self.settings.bitrate_bps / 1_000_000
                        );
                        slot.insert(encoder)
                    }
                    Err(error) => {
                        eprintln!("could not start encoder: {error}");
                        return;
                    }
                }
            }
        };
        self.apply_bitrate_request(encoder);

        let force_key = self.keyframe_wanted.swap(false, Ordering::Relaxed);
        if let Err(error) = encoder.encode(image, pts_micros, force_key) {
            eprintln!("encode failed: {error}");
            return;
        }

        for frame in encoder.drain() {
            let mut framer = self.framer.lock().expect("framer mutex not poisoned");
            // Adopt whatever repair strength the receiver has asked for; it is
            // the side that can see how much is actually going missing.
            let parity = self.requested_parity.swap(0, Ordering::Relaxed);
            if parity > 0 && self.settings.congestion_control {
                framer.set_parity(parity);
            }
            framer.send(
                Frame {
                    data: &frame.data,
                    params: &frame.params,
                    keyframe: frame.keyframe,
                    pts_micros: frame.pts_micros,
                    codec: self.settings.codec,
                },
                |datagram| {
                    self.link.send(datagram);
                    self.bytes_sent
                        .fetch_add(datagram.len() as u64, Ordering::Relaxed);
                },
            );
            self.frames_sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_audio(&self, interleaved: &[f32], rate: SampleRate, pts_micros: u64) {
        let peak = interleaved.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        self.audio_peak
            .fetch_max((peak * 10_000.0) as u64, Ordering::Relaxed);

        let mut slot = self.audio.lock().expect("audio mutex not poisoned");
        let audio = match &mut *slot {
            Some(audio) => audio,
            None => {
                if rate != self.settings.sample_rate {
                    println!(
                        "audio: capturing at {} Hz (asked for {}; macOS chose the rate)",
                        rate.hz(),
                        self.settings.sample_rate.hz()
                    );
                }
                slot.insert(AudioPacketizer::new(self.settings.fec, rate))
            }
        };
        audio.push(interleaved, pts_micros, |header, payload| {
            self.audio_packets.fetch_add(1, Ordering::Relaxed);
            fragment(header, payload, |datagram| {
                self.link.send(datagram);
                self.bytes_sent
                    .fetch_add(datagram.len() as u64, Ordering::Relaxed);
            });
        });
    }
}

pub fn run(options: Options) -> Result<()> {
    // Surface a missing dependency now, before it turns into a silent dead link.
    crate::preflight::report(&crate::preflight::check(Some(&options.peer), true));

    let (link, peer_addr) = Link::connect(&options.peer, options.port)?;
    let link = Arc::new(link);

    // The security of this link is entirely the tunnel's, so a destination
    // outside it is refused: the cost of getting this wrong is the screen and
    // audio going out in the clear, and a warning scrolling past in a terminal
    // does nothing to prevent that.
    if !stereowire_proto::tailnet::is_protected(peer_addr.ip()) {
        if !options.allow_untunnelled {
            bail!(
                "{} is not a Tailscale address.\n\
                 stereowire carries no encryption of its own and relies on the WireGuard\n\
                 tunnel Tailscale provides. Sending here would put your screen and\n\
                 audio on the network in the clear, and let anyone who finds the port\n\
                 inject packets into the stream.\n\
                 Use the peer's Tailscale name or its 100.x address, or pass\n\
                 --allow-untunnelled if the path is already encrypted by other means.",
                peer_addr.ip()
            );
        }
        eprintln!();
        eprintln!(
            "WARNING: sending to {} in the clear, outside the tunnel.",
            peer_addr.ip()
        );
        eprintln!();
    }
    let keyframe_wanted = Arc::new(AtomicBool::new(true));
    // Zero means "no request yet"; the encoder keeps its configured rate.
    let requested_bps = Arc::new(AtomicU64::new(0));
    let requested_parity = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let capture_options = CaptureOptions {
        fps: options.fps,
        sample_rate: options.sample_rate,
        max_width: options.max_width,
        show_cursor: options.show_cursor,
    };

    println!(
        "stereowire: sharing screen and system audio to {}:{}",
        options.peer, options.port
    );
    if options.congestion_control {
        println!(
            "video: up to {} Mbit/s, falling back to {} Mbit/s if the link congests",
            options.bitrate_bps / 1_000_000,
            options.min_bitrate_bps / 1_000_000
        );
    } else {
        println!("video: fixed at {} Mbit/s", options.bitrate_bps / 1_000_000);
    }
    println!(
        "audio: {} Hz stereo, uncompressed{}",
        options.sample_rate.hz(),
        if options.fec { " with FEC" } else { "" }
    );

    let sender = Arc::new(Sender {
        link: link.clone(),
        encoder: Mutex::new(None),
        settings: EncoderSettings {
            codec: options.codec,
            bitrate_bps: options.bitrate_bps,
            fps: options.fps,
            fec: options.fec,
            min_bitrate_bps: options.min_bitrate_bps,
            congestion_control: options.congestion_control,
            sample_rate: options.sample_rate,
        },
        audio: Mutex::new(None),
        framer: Mutex::new(Framer::new()),
        keyframe_wanted: keyframe_wanted.clone(),
        requested_bps: requested_bps.clone(),
        requested_parity: requested_parity.clone(),
        frames_sent: AtomicU64::new(0),
        bytes_sent: AtomicU64::new(0),
        audio_packets: AtomicU64::new(0),
        current_bps: std::sync::atomic::AtomicI32::new(options.bitrate_bps),
        audio_peak: AtomicU64::new(0),
    });

    let capture = Capture::start(&capture_options, sender.clone())?;
    println!(
        "capturing {}x{} from the main display",
        capture.size.0, capture.size.1
    );

    // Serve the receiver's control channel: keyframe requests and latency
    // probes. Pongs are echoed straight back so the measurement reflects the
    // network rather than anything queued behind the encoder.
    std::thread::spawn(move || {
        let mut buf = [0u8; stereowire_proto::packet::MTU];
        loop {
            let Some((len, _)) = link.recv(&mut buf) else {
                continue;
            };
            let Some((header, body)) = Header::parse(&buf[..len]) else {
                continue;
            };
            if header.kind != Kind::Control {
                continue;
            }
            if header.has(control::KEYFRAME_REQUEST) {
                keyframe_wanted.store(true, Ordering::Relaxed);
            }
            if header.has(control::BITRATE) {
                if let Some((bps, parity)) = control_feedback(body) {
                    requested_bps.store(bps as u64, Ordering::Relaxed);
                    requested_parity.store(parity as usize, Ordering::Relaxed);
                }
            }
            if header.has(control::PING) {
                // The stamp is the receiver's own clock; echo it untouched.
                link.send(&control_datagram(control::PONG, header.pts_micros));
            }
        }
    });

    println!("streaming; press Ctrl-C to stop");
    let start = Instant::now();
    let mut last = (0u64, 0u64);
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let frames = sender.frames_sent.load(Ordering::Relaxed);
        let bytes = sender.bytes_sent.load(Ordering::Relaxed);
        let mbps = (bytes - last.1) as f64 * 8.0 / 5.0 / 1_000_000.0;
        let fps = (frames - last.0) as f64 / 5.0;
        last = (frames, bytes);
        let packets = sender.audio_packets.swap(0, Ordering::Relaxed);
        let peak = sender.audio_peak.swap(0, Ordering::Relaxed) as f64 / 10_000.0;
        let level = if peak > 0.0 {
            format!("{:.1} dBFS", 20.0 * peak.log10())
        } else {
            "silent".to_string()
        };
        println!(
            "[{:>5.0}s] video {fps:.0} fps, {mbps:.1} Mbit/s | audio {} pkt/s, peak {level}",
            start.elapsed().as_secs_f64(),
            packets / 5,
        );
    }
}
