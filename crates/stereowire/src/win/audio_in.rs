//! System audio capture on Windows, through WASAPI via cpal.
//!
//! By default this records the default output device in loopback, which is
//! the system mix: cpal turns an input stream on an output device into
//! loopback capture by itself. A DAW playing through ASIO or WASAPI exclusive
//! mode bypasses that mix, so `--audio-device` picks any device by name
//! instead, typically a virtual cable the DAW also plays into.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use stereowire_proto::packet::{SampleRate, AUDIO_CHANNELS};

use super::capture::{now_micros, CaptureSink};

/// Keeps the capture streams alive. Dropping this stops them.
pub(super) struct AudioCapture {
    _input: cpal::Stream,
    /// Silence played into a loopback device, which keeps loopback
    /// delivering while nothing else plays.
    _keep_alive: Option<cpal::Stream>,
}

/// Starts capturing the device `requested` names, or the default output's
/// loopback, delivering to `sink`.
pub(super) fn start(requested: Option<&str>, sink: Arc<dyn CaptureSink>) -> Result<AudioCapture> {
    let host = cpal::default_host();
    let device = match requested {
        Some(wanted) => find_device(&host, wanted)?,
        None => host.default_output_device().context(
            "this PC has no audio output device to capture. Pass --audio-device to capture an \
             input instead",
        )?,
    };
    let name = device_name(&device);
    let loopback = device.supports_output();
    // cpal refuses an input configuration on an output device, and loopback
    // runs at the output's mix format anyway
    let config = if loopback {
        device.default_output_config()
    } else {
        device.default_input_config()
    }
    .with_context(|| format!("could not read the audio format of {name}"))?;
    let hz = config.sample_rate();
    let rate = SampleRate::from_hz(hz).with_context(|| {
        format!(
            "{name} runs at {hz} Hz, which the link cannot carry. Set it to 44100, 48000, 88200 \
             or 96000 Hz under the device's properties in Settings > System > Sound"
        )
    })?;
    let channels = usize::from(config.channels());
    if channels == 0 {
        bail!("{name} reports no audio channels");
    }

    let stream_config = config.config();
    let input = match config.sample_format() {
        SampleFormat::F32 => capture(&device, stream_config, channels, rate, sink, |s: f32| s),
        SampleFormat::I16 => capture(&device, stream_config, channels, rate, sink, from_i16),
        SampleFormat::I32 => capture(&device, stream_config, channels, rate, sink, from_i32),
        other => bail!(
            "{name} delivers {other} samples, which the sender does not convert. Choose a \
             32-bit or 16-bit format under the device's properties in Sound settings"
        ),
    }
    .with_context(|| {
        if loopback {
            format!(
                "could not capture {name}. If a DAW holds it in exclusive mode, route the DAW \
                 through a virtual cable and pass --audio-device"
            )
        } else {
            format!("could not capture {name}")
        }
    })?;
    input
        .play()
        .with_context(|| format!("could not start capturing {name}"))?;
    let keep_alive = if loopback {
        keep_alive(&device, &name)
    } else {
        None
    };

    println!(
        "audio: capturing {name} ({}) at {hz} Hz, {channels} channels",
        if loopback {
            "loopback of the output"
        } else {
            "input"
        }
    );
    Ok(AudioCapture {
        _input: input,
        _keep_alive: keep_alive,
    })
}

fn capture<T: SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    rate: SampleRate,
    sink: Arc<dyn CaptureSink>,
    convert: fn(T) -> f32,
) -> Result<cpal::Stream, cpal::Error> {
    let mut stereo = Vec::new();
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let frames = data.len() / channels;
            if frames == 0 {
                return;
            }
            // The callback runs once the whole block has arrived, so its
            // first sample is the block's duration old
            let duration = frames as u64 * 1_000_000 / u64::from(rate.hz());
            let pts_micros = now_micros().saturating_sub(duration);
            to_stereo(data, channels, convert, &mut stereo);
            sink.on_audio(&stereo, rate, pts_micros);
        },
        |error| eprintln!("audio capture error: {error}"),
        None,
    )
}

fn from_i16(sample: i16) -> f32 {
    f32::from(sample) / 32_768.0
}

fn from_i32(sample: i32) -> f32 {
    sample as f32 / 2_147_483_648.0
}

/// The first two channels of interleaved `samples` as f32 stereo, with a
/// mono channel on both sides, written into `out`.
fn to_stereo<T: Copy>(samples: &[T], channels: usize, convert: fn(T) -> f32, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(samples.len() / channels * AUDIO_CHANNELS);
    for frame in samples.chunks_exact(channels) {
        let left = convert(frame[0]);
        let right = frame.get(1).map_or(left, |&sample| convert(sample));
        out.push(left);
        out.push(right);
    }
}

/// Plays silence into a loopback device, since WASAPI loopback delivers
/// nothing while nothing plays. Without it capture pauses in every silence.
fn keep_alive(device: &cpal::Device, name: &str) -> Option<cpal::Stream> {
    let started = device
        .default_output_config()
        .map_err(|error| error.to_string())
        .and_then(|config| {
            device
                .build_output_stream_raw(
                    config.config(),
                    config.sample_format(),
                    // cpal hands the callback a buffer already filled with
                    // silence, so there is nothing to write
                    |_: &mut cpal::Data, _: &cpal::OutputCallbackInfo| {},
                    |error| eprintln!("audio keep-alive error: {error}"),
                    None,
                )
                .map_err(|error| error.to_string())
        })
        .and_then(|stream| {
            stream
                .play()
                .map(|()| stream)
                .map_err(|error| error.to_string())
        });
    match started {
        Ok(stream) => Some(stream),
        Err(error) => {
            println!(
                "note: could not play silence into {name} ({error}), so capture pauses whenever \
                 nothing is playing"
            );
            None
        }
    }
}

/// The first device, input or output, whose name contains `wanted`,
/// ignoring case.
fn find_device(host: &cpal::Host, wanted: &str) -> Result<cpal::Device> {
    let needle = wanted.to_lowercase();
    let mut names = Vec::new();
    for device in host.devices().context("could not list the audio devices")? {
        let name = device_name(&device);
        if name.to_lowercase().contains(&needle) {
            return Ok(device);
        }
        let kind = if device.supports_output() {
            "output"
        } else {
            "input"
        };
        names.push(format!("{name} ({kind})"));
    }
    if names.is_empty() {
        bail!("no audio device matches \"{wanted}\", and this PC reports no audio devices at all");
    }
    bail!(
        "no audio device name contains \"{wanted}\". The devices on this PC are:\n  {}",
        names.join("\n  ")
    )
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| "an unnamed audio device".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_keeps_the_first_two_channels() {
        let mut out = Vec::new();
        to_stereo(&[1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0], 3, |s| s, &mut out);
        assert_eq!(out, vec![1.0, 2.0, 4.0, 5.0]);
    }

    #[test]
    fn mono_goes_to_both_sides() {
        let mut out = vec![9.0];
        to_stereo(&[0.25f32, -0.5], 1, |s| s, &mut out);
        assert_eq!(out, vec![0.25, 0.25, -0.5, -0.5]);
    }

    #[test]
    fn integer_samples_scale_to_the_float_range() {
        let mut out = Vec::new();
        to_stereo(&[i16::MIN, 16_384], 2, from_i16, &mut out);
        assert_eq!(out, vec![-1.0, 0.5]);
        to_stereo(&[i32::MIN, 0], 2, from_i32, &mut out);
        assert_eq!(out, vec![-1.0, 0.0]);
    }
}
