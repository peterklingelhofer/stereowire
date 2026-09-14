//! Plays the received audio stream through the default output device.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use stereowire_proto::jitter::AudioJitter;
use stereowire_proto::packet::{SampleRate, AUDIO_CHANNELS};

/// Keeps the stream alive; dropping this stops playback.
pub struct AudioOut {
    _stream: cpal::Stream,
}

impl AudioOut {
    /// Opens playback at the sender's rate.
    ///
    /// This happens once the first packet has revealed that rate, rather than
    /// at startup, so the output never has to resample a stream whose format
    /// we had guessed.
    pub fn start(jitter: Arc<Mutex<AudioJitter>>, rate: SampleRate) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no audio output device")?;

        let config = cpal::StreamConfig {
            channels: AUDIO_CHANNELS as u16,
            sample_rate: rate.hz(),
            // Matching the device's own preference keeps the callback short and
            // avoids CoreAudio doing its own buffering on top of ours.
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [f32], _| match jitter.lock() {
                    Ok(mut buffer) => buffer.pull(out),
                    // Never leave the callback without writing something.
                    Err(_) => out.fill(0.0),
                },
                |err| eprintln!("audio output error: {err}"),
                None,
            )
            .with_context(|| format!("could not open a {} Hz stereo output stream", rate.hz()))?;
        stream.play().context("could not start audio playback")?;

        Ok(AudioOut { _stream: stream })
    }
}

/// The default output device's own rate, used as the sending default so the
/// capture matches the hardware and nothing is resampled on the way out.
pub fn default_device_rate() -> Option<SampleRate> {
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    SampleRate::from_hz(config.sample_rate())
}
