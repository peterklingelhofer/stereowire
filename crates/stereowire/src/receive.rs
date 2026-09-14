//! Receiving side: reassemble, decode, display, and play.
//!
//! Packets are read on a background thread so a slow frame never stalls the
//! socket, while AppKit and the display layer stay on the main thread where
//! they belong.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSEventMask};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use stereowire_proto::congestion::{Controller, Feedback};
use stereowire_proto::packet::{
    control, control_datagram, control_feedback_datagram, Header, Kind, MTU,
};
use stereowire_proto::rtt::{format_ms, Rtt};
use stereowire_proto::video::{Depacketizer, Received};

use crate::audio_out::AudioOut;
use crate::mac::display::Display;
use crate::mac::menu;
use crate::mac::menu::Menu;
use crate::net::Link;
use crate::wav::WavWriter;

/// Counters shared between the network thread and the reporting loop.
#[derive(Default)]
struct Stats {
    datagrams: AtomicU64,
    video_frames: AtomicU64,
    audio_packets: AtomicU64,
    dropped_out_of_sync: AtomicU64,
    keyframe_requests: AtomicU64,
    synced: AtomicBool,
    /// Bitrate most recently requested of the sender.
    target_bps: AtomicU64,
}

/// A frame ready to hand to the display layer.
struct VideoFrame {
    params: Option<Vec<Vec<u8>>>,
    data: Vec<u8>,
    pts_micros: u64,
}

pub fn run(
    port: u16,
    buffer_ms: u32,
    play_audio: bool,
    record: Option<PathBuf>,
    show_latency: bool,
    max_mbps: u32,
    min_mbps: u32,
) -> Result<()> {
    let mtm = MainThreadMarker::new().context("the receiver must run on the main thread")?;
    // The receiver never captures, so Screen Recording is not required here.
    crate::preflight::report(&crate::preflight::check(None, false));

    // Playback and recording both need the sender's sample rate, which only the
    // first audio packet reveals, so they are opened once it arrives.
    let jitter = Arc::new(std::sync::Mutex::new(
        stereowire_proto::jitter::AudioJitter::new(buffer_ms),
    ));
    let mut audio: Option<AudioOut> = None;
    let link = Arc::new(Link::listen(port)?);
    println!(
        "stereowire: listening on udp/{port}, audio buffer {buffer_ms} ms{}",
        if play_audio { "" } else { " (playback off)" }
    );
    println!("window open; waiting for the sender");

    // A short queue: if the display falls behind, dropping is better than
    // building a backlog of stale frames.
    let (tx, rx) = sync_channel::<VideoFrame>(8);
    let stats = Arc::new(Stats::default());
    let rtt = Arc::new(std::sync::Mutex::new(Rtt::default()));
    let net_link = link.clone();
    let net_jitter = jitter.clone();
    let net_stats = stats.clone();
    let net_rtt = rtt.clone();
    let congestion = Controller::new(max_mbps * 1_000_000, min_mbps.min(max_mbps) * 1_000_000);
    std::thread::spawn(move || {
        receive_loop(net_link, tx, net_jitter, net_stats, net_rtt, congestion)
    });

    let app = NSApplication::sharedApplication(mtm);
    // A plain binary is not launched the way a bundled app is, so AppKit needs
    // to be told to finish starting up before it will deliver events or draw.
    app.finishLaunching();
    // Open the window straight away rather than on the first frame, so there is
    // something on screen while waiting for the sender.
    let mut display = Display::open(mtm, "stereowire", 1920, 1200)?;
    let menu = Menu::install(mtm, show_latency);
    let mut title_timer = Instant::now() - Duration::from_secs(1);
    let mut shown_title: Option<String> = None;
    let mut last_report = Instant::now();
    let mut presented = 0u64;
    let mut av_skew_us: i64 = 0;
    let mut recorder: Option<WavWriter> = None;
    // Set once playback and any recorder have been opened for the stream.
    let mut audio_started = false;

    loop {
        pump_events(&app);

        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(frame) => {
                if let Some(params) = &frame.params {
                    display.set_params(params)?;
                }
                if display.present(&frame.data, frame.pts_micros)? {
                    presented += 1;
                    // Both streams are stamped from the sender's clock, so the
                    // difference is the real audio/video offset rather than an
                    // estimate. Positive means the picture leads the sound.
                    if let Some(audio_pts) =
                        jitter.lock().ok().and_then(|j| j.playback_pts_micros())
                    {
                        av_skew_us = frame.pts_micros as i64 - audio_pts as i64;
                    }
                }
            }
            // Timing out is the idle case: fall through and pump AppKit.
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Open the audio side as soon as the stream states its rate.
        if !audio_started {
            if let Some(rate) = jitter.lock().ok().and_then(|j| j.rate()) {
                audio_started = true;
                println!("audio: {} Hz stereo from the sender", rate.hz());
                if play_audio {
                    audio = Some(AudioOut::start(jitter.clone(), rate)?);
                }
                if let Some(path) = &record {
                    println!("recording received audio to {}", path.display());
                    recorder = Some(WavWriter::create(path, rate)?);
                }
            }
        }

        // With playback off nothing consumes the audio, so drain it here to
        // keep the buffer bounded and to feed the recorder steadily.
        if audio.is_none() {
            if let Ok(mut buffer) = jitter.lock() {
                let depth = buffer.depth();
                if depth > 0 {
                    let mut scratch = vec![0.0; depth];
                    buffer.pull(&mut scratch);
                    if let Some(recorder) = recorder.as_mut() {
                        recorder.write(&scratch)?;
                    }
                }
            }
        }

        if !display.is_open() || menu.should_quit() {
            break;
        }

        // Once a second is often enough to read and cheap enough to be free;
        // doing this per frame would be pure waste.
        if title_timer.elapsed() >= Duration::from_secs(1) {
            title_timer = Instant::now();
            menu.sync_check_mark();
            let detail = menu.show_latency().then(|| describe_link(&rtt, &jitter));
            if shown_title.as_deref() != detail.as_deref() {
                display.set_title(&menu::title("stereowire", detail.as_deref()));
                shown_title = detail;
            }
        }

        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            println!(
                "link: {} datagrams, {} video frames ({presented} shown), {} audio packets, {} frames dropped out of sync, {} keyframe requests, {}, video {:.0} Mbit/s",
                stats.datagrams.load(Ordering::Relaxed),
                stats.video_frames.load(Ordering::Relaxed),
                stats.audio_packets.load(Ordering::Relaxed),
                stats.dropped_out_of_sync.load(Ordering::Relaxed),
                stats.keyframe_requests.load(Ordering::Relaxed),
                if stats.synced.load(Ordering::Relaxed) {
                    "in sync"
                } else {
                    "WAITING FOR SYNC"
                },
                stats.target_bps.load(Ordering::Relaxed) as f64 / 1e6,
            );
            if let Ok(rtt) = rtt.lock() {
                if rtt.has_measurement() {
                    println!(
                        "latency: {} ms round trip (about {} ms each way), jitter {} ms",
                        format_ms(rtt.smoothed_ms()),
                        format_ms(rtt.one_way_ms()),
                        format_ms(rtt.jitter_ms())
                    );
                }
            }
            if let Ok(buffer) = jitter.lock() {
                let stats = buffer.stats;
                if stats.recovered + stats.concealed + stats.underruns > 0 {
                    println!(
                        "audio: {} recovered, {} concealed, {} underruns",
                        stats.recovered, stats.concealed, stats.underruns
                    );
                }
                // Only meaningful while audio is actually being played: with
                // playback off the buffer is drained as fast as it arrives, so
                // the audio timeline races ahead of any real playback position.
                if audio.is_some() && presented > 0 && av_skew_us != 0 {
                    println!(
                        "a/v offset: picture is {:.0} ms {} the sound",
                        (av_skew_us.abs() as f64) / 1000.0,
                        if av_skew_us > 0 { "ahead of" } else { "behind" }
                    );
                }
                if stats.drift_ppm != 0 {
                    // Holding the buffer against the sender's clock. Well under
                    // a cent of pitch, and far preferable to a dropout.
                    println!("audio: correcting clock drift by {} ppm", stats.drift_ppm);
                }
            }
        }
    }
    if let Some(recorder) = recorder {
        recorder.finish()?;
    }
    Ok(())
}

/// The next frame the depacketizer will release, or `None` when it has nothing
/// more that is in order.
fn next_ready(video: &mut Depacketizer) -> Option<Received> {
    match video.poll() {
        Received::Pending => None,
        ready => Some(ready),
    }
}

/// Builds the one-line link summary shown in the title bar.
fn describe_link(
    rtt: &Arc<std::sync::Mutex<Rtt>>,
    jitter: &Arc<std::sync::Mutex<stereowire_proto::jitter::AudioJitter>>,
) -> String {
    let Ok(rtt) = rtt.lock() else {
        return "latency unavailable".into();
    };
    if !rtt.has_measurement() {
        return "measuring latency".into();
    }
    // The audio buffer is the other half of what you actually hear, so it is
    // worth showing next to the network figure rather than on its own.
    let audio_ms = jitter
        .lock()
        .map(|buffer| match buffer.rate() {
            Some(rate) => {
                buffer.depth() as f64
                    / stereowire_proto::packet::AUDIO_CHANNELS as f64
                    / rate.hz() as f64
                    * 1000.0
            }
            None => 0.0,
        })
        .unwrap_or(0.0);
    format!(
        "rtt {} ms  ±{}  ·  audio buffer {audio_ms:.0} ms",
        format_ms(rtt.smoothed_ms()),
        format_ms(rtt.jitter_ms())
    )
}

/// Drains pending AppKit events so the window stays responsive.
fn pump_events(app: &NSApplication) {
    // `distantPast` makes this non-blocking: take what is queued and return.
    while let Some(event) = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::distantPast()),
            NSDefaultRunLoopMode,
            true,
        )
    } {
        app.sendEvent(&event);
    }
}

/// How often to probe the link. Frequent enough to track a changing route,
/// rare enough to be invisible next to the media streams.
const PING_INTERVAL: Duration = Duration::from_millis(500);

fn receive_loop(
    link: Arc<Link>,
    frames: std::sync::mpsc::SyncSender<VideoFrame>,
    jitter: Arc<std::sync::Mutex<stereowire_proto::jitter::AudioJitter>>,
    stats: Arc<Stats>,
    rtt: Arc<std::sync::Mutex<Rtt>>,
    mut congestion: Controller,
) {
    let mut video = Depacketizer::new();
    let mut buf = vec![0u8; MTU];
    // All probe stamps are read from this one clock, so the two machines never
    // need their clocks to agree.
    let epoch = Instant::now();
    let mut peer_addr: Option<std::net::SocketAddr> = None;
    let mut last_ping = Instant::now() - PING_INTERVAL;
    let mut last_request = Instant::now() - Duration::from_secs(1);

    loop {
        // Probe first, so the many `continue`s below cannot starve it.
        if let Some(peer) = peer_addr {
            if last_ping.elapsed() >= PING_INTERVAL {
                last_ping = Instant::now();
                let stamp = epoch.elapsed().as_micros() as u64;
                link.send_to(&control_datagram(control::PING, stamp), peer);

                // The receiver holds every signal worth acting on, so it picks
                // the rate and asks the sender for it.
                if let Some(loss) = video.take_loss() {
                    let measured = rtt.lock().ok().filter(|r| r.has_measurement());
                    let rtt_ms = measured.map(|r| r.smoothed_ms()).unwrap_or(0.0);
                    congestion.update(Feedback { loss, rtt_ms });
                    let target = congestion.target_bps();
                    stats.target_bps.store(target as u64, Ordering::Relaxed);
                    link.send_to(
                        &control_feedback_datagram(
                            control::BITRATE,
                            target,
                            congestion.parity_blocks(),
                        ),
                        peer,
                    );
                }
            }
        }

        let Some((len, peer)) = link.recv(&mut buf) else {
            // Timed out: nothing to do but keep waiting.
            continue;
        };
        peer_addr = Some(peer);
        stats.datagrams.fetch_add(1, Ordering::Relaxed);
        let Some((header, body)) = Header::parse(&buf[..len]) else {
            continue;
        };

        match header.kind {
            Kind::Audio => {
                stats.audio_packets.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut buffer) = jitter.lock() {
                    buffer.push(header, body);
                }
            }
            Kind::Video => {
                video.push(header, body);
                // One arrival can release several frames when it fills a gap,
                // so drain until nothing more is in order.
                while let Some(ready) = next_ready(&mut video) {
                    match ready {
                        Received::Pending => {}
                        Received::Frame {
                            params,
                            data,
                            pts_micros,
                            ..
                        } => {
                            stats.video_frames.fetch_add(1, Ordering::Relaxed);
                            stats.synced.store(true, Ordering::Relaxed);
                            // A full queue means the display is behind; drop rather
                            // than block the socket and fall further behind.
                            let _ = frames.try_send(VideoFrame {
                                params,
                                data,
                                pts_micros,
                            });
                        }
                        Received::NeedKeyframe => {
                            stats.synced.store(false, Ordering::Relaxed);
                            stats.dropped_out_of_sync.fetch_add(1, Ordering::Relaxed);
                            if last_request.elapsed() >= Duration::from_millis(250) {
                                last_request = Instant::now();
                                stats.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                                request_keyframe(&link, peer);
                            }
                        }
                    }
                }
            }
            Kind::Control => {
                if header.has(control::PONG) {
                    // Both stamps come from our own clock, so this is a true
                    // round trip with no clock-offset error.
                    let now = epoch.elapsed().as_micros() as u64;
                    if let Some(elapsed) = now.checked_sub(header.pts_micros) {
                        if let Ok(mut rtt) = rtt.lock() {
                            rtt.record(elapsed);
                        }
                    }
                }
            }
        }
    }
}

fn request_keyframe(link: &Link, peer: std::net::SocketAddr) {
    link.send_to(&control_datagram(control::KEYFRAME_REQUEST, 0), peer);
}
