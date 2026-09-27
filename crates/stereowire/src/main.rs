//! stereowire: a high-fidelity, low-latency screen-and-system-audio link between
//! Macs and Windows PCs on the same Tailscale network.
//!
//! Audio is sent uncompressed. For two people, a 48 kHz stereo float stream is
//! about 3 Mbit/s, which is small enough that giving up any fidelity to a codec
//! would be a poor trade.

#[cfg(any(target_os = "macos", windows))]
mod audio_out;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(any(target_os = "macos", windows))]
mod net;
#[cfg(any(target_os = "macos", windows))]
mod pattern;
#[cfg(any(target_os = "macos", windows))]
mod preflight;
#[cfg(any(target_os = "macos", windows))]
mod receive;
#[cfg(any(target_os = "macos", windows))]
mod selftest;
#[cfg(any(target_os = "macos", windows))]
mod send;
#[cfg(any(target_os = "macos", windows))]
mod wav;
#[cfg(windows)]
mod win;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "stereowire",
    about = "High-fidelity screen and system-audio link between Macs and Windows PCs"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Share this machine's screen and system audio with a peer.
    Send {
        /// Peer's Tailscale address or hostname.
        #[arg(long)]
        to: String,
        #[arg(long, default_value_t = 9000)]
        port: u16,
        /// Video bitrate in megabits per second.
        #[arg(long, default_value_t = 40)]
        mbps: u32,
        #[arg(long, default_value_t = 60)]
        fps: i32,
        /// Video codec, hevc by default on a Mac and h264 on Windows. Use
        /// h264 when the watcher is on Windows, which decodes it without
        /// extra software.
        #[arg(long, value_enum, default_value_t = DEFAULT_CODEC)]
        codec: CodecArg,
        /// Cap on capture width in pixels; the display is scaled down to fit.
        #[arg(long, default_value_t = 2560)]
        max_width: i32,
        /// Disable audio forward error correction, halving audio bandwidth.
        #[arg(long)]
        no_fec: bool,
        /// Lowest video bitrate congestion control may fall to, in Mbit/s.
        #[arg(long, default_value_t = 8)]
        min_mbps: u32,
        /// Hold the bitrate fixed instead of adapting to loss and delay.
        #[arg(long)]
        no_congestion_control: bool,
        /// Send to an address outside the tailnet. The stream is unencrypted,
        /// so only use this where something else already encrypts the path.
        #[arg(long)]
        allow_untunnelled: bool,
        /// Audio sample rate in Hz (44100, 48000, 88200, 96000). Defaults to
        /// the output device's own rate, so nothing is resampled. Set this to
        /// match your project if it differs.
        #[arg(long)]
        sample_rate: Option<u32>,
        #[arg(long)]
        hide_cursor: bool,
        /// Windows: capture this audio device (a substring of its name)
        /// instead of the default output's loopback, for example a virtual
        /// cable that carries a DAW running on ASIO.
        #[arg(long, value_name = "NAME")]
        audio_device: Option<String>,
    },
    /// Check that Tailscale and permissions are set up, and say how to fix
    /// whatever is not.
    Doctor {
        /// Optionally check that this peer is reachable and directly connected.
        #[arg(long)]
        peer: Option<String>,
    },
    /// Run the media path end to end locally, with no peer and no permissions.
    Selftest {
        #[arg(long, default_value_t = 90)]
        frames: u32,
        #[arg(long, default_value_t = 1280)]
        width: usize,
        #[arg(long, default_value_t = 720)]
        height: usize,
        #[arg(long, default_value_t = 20)]
        mbps: u32,
        #[arg(long, default_value_t = 60)]
        fps: i32,
        /// Percentage of datagrams to drop, to exercise loss handling.
        #[arg(long, default_value_t = 0)]
        loss: u32,
        /// Deliver datagrams up to this many positions out of order.
        #[arg(long, default_value_t = 0)]
        reorder: u32,
        /// Parity blocks per fragment group: 1 repairs a single loss, 2 any pair.
        #[arg(long, default_value_t = 1)]
        fec_parity: usize,
        #[arg(long, value_enum, default_value_t = DEFAULT_CODEC)]
        codec: CodecArg,
        /// Write every encoded video datagram to this file, exactly as sent,
        /// so `--replay` can feed them back in later without an encoder.
        #[arg(long)]
        dump: Option<std::path::PathBuf>,
        /// Replay video datagrams from a file written by `--dump`, instead of
        /// running the encoder. --width, --height, --fps, --frames, --codec
        /// and --mbps are ignored: those come from the dump itself.
        #[arg(long)]
        replay: Option<std::path::PathBuf>,
    },
    /// Watch and listen to a peer's shared screen.
    Receive {
        #[arg(long, default_value_t = 9000)]
        port: u16,
        /// Audio buffered before playback starts. Raise it on a jittery link.
        #[arg(long, default_value_t = 30)]
        buffer_ms: u32,
        /// Watch without playing audio.
        #[arg(long)]
        no_audio: bool,
        /// Record the received audio to a WAV file.
        #[arg(long)]
        record: Option<std::path::PathBuf>,
        /// Start with the latency readout hidden (toggle it with View > Show
        /// Latency in Title, or Command-L).
        #[arg(long)]
        hide_latency: bool,
        /// Highest video bitrate to ask the sender for, in Mbit/s.
        #[arg(long, default_value_t = 40)]
        max_mbps: u32,
        /// Lowest video bitrate to fall back to on a congested link.
        #[arg(long, default_value_t = 8)]
        min_mbps: u32,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    Hevc,
    #[value(name = "h264")]
    H264,
}

/// HEVC between Macs, where every encoder and decoder handles it. H.264 on
/// Windows, where Microsoft's own encoder handles nothing else.
#[cfg(not(windows))]
const DEFAULT_CODEC: CodecArg = CodecArg::Hevc;
#[cfg(windows)]
const DEFAULT_CODEC: CodecArg = CodecArg::H264;

impl From<CodecArg> for stereowire_proto::packet::Codec {
    fn from(value: CodecArg) -> Self {
        match value {
            CodecArg::Hevc => stereowire_proto::packet::Codec::Hevc,
            CodecArg::H264 => stereowire_proto::packet::Codec::H264,
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
fn main() -> anyhow::Result<()> {
    // Launched from Finder with no arguments, the useful default is to listen.
    // Finder also appends a process-serial argument that clap would reject.
    let args: Vec<std::ffi::OsString> = std::env::args_os()
        .filter(|a| !a.to_string_lossy().starts_with("-psn_"))
        .collect();
    let cli = if args.len() == 1 {
        Cli {
            command: Command::Receive {
                port: 9000,
                buffer_ms: 30,
                no_audio: false,
                record: None,
                hide_latency: false,
                max_mbps: 40,
                min_mbps: 8,
            },
        }
    } else {
        Cli::parse_from(args)
    };
    match cli.command {
        Command::Send {
            to,
            port,
            mbps,
            fps,
            codec,
            max_width,
            no_fec,
            hide_cursor,
            sample_rate,
            min_mbps,
            no_congestion_control,
            allow_untunnelled,
            audio_device,
        } => send::run(send::Options {
            peer: to,
            port,
            codec: codec.into(),
            bitrate_bps: (mbps * 1_000_000) as i32,
            fps,
            max_width,
            fec: !no_fec,
            show_cursor: !hide_cursor,
            sample_rate: resolve_sample_rate(sample_rate)?,
            min_bitrate_bps: (min_mbps.min(mbps) * 1_000_000) as i32,
            congestion_control: !no_congestion_control,
            allow_untunnelled,
            audio_device,
        }),
        Command::Receive {
            port,
            buffer_ms,
            no_audio,
            record,
            hide_latency,
            max_mbps,
            min_mbps,
        } => receive::run(
            port,
            buffer_ms,
            !no_audio,
            record,
            !hide_latency,
            max_mbps,
            min_mbps,
        ),
        Command::Doctor { peer } => {
            preflight::report(&preflight::check(peer.as_deref(), true));
            Ok(())
        }
        Command::Selftest {
            frames,
            width,
            height,
            mbps,
            fps,
            loss,
            reorder,
            fec_parity,
            codec,
            dump,
            replay,
        } => selftest::run(selftest::Options {
            frames,
            width,
            height,
            mbps,
            fps,
            loss_percent: loss,
            reorder,
            parity: fec_parity,
            codec: codec.into(),
            dump,
            replay,
        }),
    }
}

/// Picks the capture rate: an explicit request, else whatever the output
/// device is already running at, so the common case needs no flag.
#[cfg(any(target_os = "macos", windows))]
fn resolve_sample_rate(
    requested: Option<u32>,
) -> anyhow::Result<stereowire_proto::packet::SampleRate> {
    use stereowire_proto::packet::SampleRate;
    match requested {
        Some(hz) => SampleRate::from_hz(hz).ok_or_else(|| {
            anyhow::anyhow!("unsupported sample rate {hz}; use 44100, 48000, 88200 or 96000")
        }),
        None => Ok(audio_out::default_device_rate().unwrap_or(SampleRate::Hz48000)),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("stereowire currently supports macOS and Windows only");
    std::process::exit(1);
}
