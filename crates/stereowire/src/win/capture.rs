//! Screen and system-audio capture on Windows.
//!
//! Mirrors `mac::capture`, with the same sink and options, so `send.rs`
//! drives both platforms the same way. Video comes from Desktop Duplication
//! (see `desktop`) and audio from WASAPI through cpal (see `audio_in`). Both
//! are stamped from `QueryPerformanceCounter` in microseconds, the one clock
//! that makes the two streams comparable at the receiver.

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use stereowire_proto::packet::SampleRate;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use super::audio_in::AudioCapture;
use super::desktop::DesktopCapture;

/// A captured video frame: limited-range BT.709 NV12, what the encoder takes.
pub type CapturedFrame = crate::pattern::Nv12Frame;

/// Pixel dimensions of a captured frame.
pub fn frame_size(frame: &CapturedFrame) -> (i32, i32) {
    (frame.width as i32, frame.height as i32)
}

/// Receives capture output. `on_video` runs on the capture thread and
/// `on_audio` on cpal's audio thread, so implementations must not block.
pub trait CaptureSink: Send + Sync {
    fn on_video(&self, frame: &CapturedFrame, pts_micros: u64);
    /// Interleaved stereo f32, at whatever rate the device is running.
    ///
    /// `pts_micros` is on the same capture clock as [`CaptureSink::on_video`],
    /// which is what makes the two streams comparable.
    fn on_audio(&self, interleaved: &[f32], rate: SampleRate, pts_micros: u64);
}

pub struct CaptureOptions {
    pub fps: i32,
    /// The rate asked for. WASAPI shared mode captures at the device's own
    /// rate, so Windows leaves this unused, and `send.rs` reports it when the
    /// two differ.
    #[allow(dead_code)]
    pub sample_rate: SampleRate,
    /// Halves the capture if the desktop is wider than this, to keep the
    /// encoder and the link within budget.
    pub max_width: i32,
    pub show_cursor: bool,
    /// Captures this device, named by any part of its name, instead of the
    /// default output's loopback: the route for a DAW that plays through
    /// ASIO or WASAPI exclusive mode and so bypasses the Windows mixer.
    pub audio_device: Option<String>,
}

/// A running capture. Dropping it stops both streams.
pub struct Capture {
    /// Pixel dimensions of the frames the capture delivers.
    pub size: (i32, i32),
    _video: DesktopCapture,
    _audio: AudioCapture,
}

impl Capture {
    pub fn start(options: &CaptureOptions, sink: Arc<dyn CaptureSink>) -> Result<Self> {
        // Video first: it is what remote sessions and virtual machines
        // refuse, and its message is the one that explains the setup
        let video = super::desktop::start(options, sink.clone())?;
        let audio = super::audio_in::start(options.audio_device.as_deref(), sink)?;
        Ok(Capture {
            size: video.size,
            _video: video,
            _audio: audio,
        })
    }
}

/// A performance counter reading in microseconds.
pub(super) fn qpc_micros(ticks: i64) -> u64 {
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut frequency = 0;
        // Documented never to fail on Windows XP and later
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        frequency.max(1)
    });
    (i128::from(ticks.max(0)) * 1_000_000 / i128::from(frequency)) as u64
}

/// The performance counter now, in microseconds.
pub(super) fn now_micros() -> u64 {
    let mut ticks = 0;
    let _ = unsafe { QueryPerformanceCounter(&mut ticks) };
    qpc_micros(ticks)
}
