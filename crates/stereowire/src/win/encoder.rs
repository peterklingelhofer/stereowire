//! H.264 and HEVC encoding via Media Foundation.
//!
//! Mirrors `mac::encoder::Encoder`, so `send.rs` and the self-test drive both
//! platforms the same way. Three kinds of encoder can turn up here. Hardware
//! encoders from Intel, NVIDIA and AMD are asynchronous MFTs driven by
//! events, Microsoft's software encoder is synchronous, and Wine's ignores
//! every setting. So the code takes nothing an encoder may decline for
//! granted:
//!
//! - Settings go through `ICodecAPI`, and the startup line names each one the
//!   encoder refused.
//! - Output timestamps are the input timestamps handed back in submission
//!   order (see `Timestamps`), since an encoder may rewrite sample times.
//! - Parameter sets are read from the in-band NAL units and sent with every
//!   frame, as the Mac sends the ones VideoToolbox keeps in its format
//!   description.
//! - `data` holds only slice and SEI NAL units, each behind a 4-byte
//!   big-endian length, the form VideoToolbox emits.
//!
//! The input is limited-range BT.709 NV12 and the input type says so, since
//! that is what decoders assume of a stream whose parameter sets carry no
//! colour description.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use stereowire_proto::packet::Codec;
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::{S_OK, VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use super::decoder::{ensure_media_foundation, micros_to_100ns};
use crate::pattern::Nv12Frame;

/// One compressed frame, ready to put on the wire.
pub struct EncodedFrame {
    /// Slice and SEI NAL units, each behind a 4-byte big-endian length.
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_micros: u64,
    /// SPS and PPS for H.264, with the VPS first for HEVC, carried on every
    /// frame for the reason `mac::encoder::EncodedFrame` gives.
    pub params: Vec<Vec<u8>>,
}

/// Microsoft's software H.264 encoder, created directly when nothing that
/// enumeration found will start. The windows crate does not define this
/// CLSID.
const CLSID_MS_H264_ENCODER: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);

/// Setting this environment variable, to any value, skips hardware encoders
/// and uses Microsoft's software encoder: the way out when a graphics driver
/// ships an encoder that misbehaves.
const SOFTWARE_ENCODER_VAR: &str = "STEREOWIRE_SOFTWARE_ENCODER";

/// How long an asynchronous encoder may take to ask for its first frame
/// before the next candidate is tried.
const FIRST_INPUT_TIMEOUT: Duration = Duration::from_millis(500);

/// Longest a frame waits for an asynchronous encoder to ask for input before
/// it is dropped.
const INPUT_WAIT_LIMIT: Duration = Duration::from_millis(100);

/// Longest `encode` waits for an asynchronous encoder's output, so a frame
/// usually leaves with the call that submitted it.
const OUTPUT_WAIT_LIMIT: Duration = Duration::from_millis(8);

/// Longest `finish` waits for an asynchronous encoder to drain.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// How far out of order outputs may arrive when B-frames could not be turned
/// off: the most reference frames H.264 or HEVC allows.
const REORDER_LIMIT: usize = 16;

/// How close an output's time must come to an input's to match it, in 100 ns
/// units. A millisecond is far below any frame interval.
const TIME_TOLERANCE: i64 = 10_000;

/// Dropped frames are reported at most this often, so a struggling encoder
/// does not flood the log.
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(5);

pub struct Encoder {
    state: Mutex<State>,
}

// SAFETY: every call into the transform goes through the mutex, one at a
// time. Encoder MFTs are free-threaded in-process objects (the asynchronous
// ones run worker threads of their own), so the calling thread does not
// matter, and the sender only ever uses an encoder from the capture thread
// that built it
unsafe impl Send for Encoder {}

impl Encoder {
    /// Starts the first encoder that takes the configuration: hardware
    /// encoders first, then software ones, then for H.264 Microsoft's
    /// software encoder by CLSID. `STEREOWIRE_SOFTWARE_ENCODER` skips the
    /// hardware ones.
    pub fn new(codec: Codec, width: i32, height: i32, bitrate_bps: i32, fps: i32) -> Result<Self> {
        ensure_media_foundation()?;
        let config = Config {
            codec,
            width: width.max(2) as u32,
            height: height.max(2) as u32,
            bitrate_bps: bitrate_bps.max(1) as u32,
            fps: fps.max(1) as u32,
        };
        let subtype = match codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
        };

        let mut candidates = Vec::new();
        if std::env::var_os(SOFTWARE_ENCODER_VAR).is_none() {
            candidates.extend(enumerate(
                &subtype,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            ));
        }
        candidates.extend(enumerate(
            &subtype,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        ));

        let mut last_error = None;
        for activate in candidates {
            let name = friendly_name(&activate);
            let label = name
                .clone()
                .unwrap_or_else(|| "an unnamed encoder".to_string());
            let started = unsafe { activate.ActivateObject::<IMFTransform>() }
                .context("it would not activate")
                .and_then(|mft| State::start(&mft, name, &config).inspect_err(|_| shut_down(&mft)));
            match started {
                Ok(state) => return Ok(Encoder::from(state)),
                Err(error) => {
                    println!("note: skipping {label}: {error:#}");
                    last_error = Some(error);
                }
            }
        }

        if codec == Codec::H264 {
            let created: windows::core::Result<IMFTransform> =
                unsafe { CoCreateInstance(&CLSID_MS_H264_ENCODER, None, CLSCTX_INPROC_SERVER) };
            match created {
                Ok(mft) => match State::start(&mft, None, &config) {
                    Ok(state) => return Ok(Encoder::from(state)),
                    Err(error) => {
                        shut_down(&mft);
                        last_error = Some(error);
                    }
                },
                Err(error) => {
                    last_error.get_or_insert_with(|| {
                        anyhow!("Microsoft's software encoder is not registered ({error})")
                    });
                }
            }
        }

        match (codec, last_error) {
            (Codec::Hevc, None) => bail!("this PC has no HEVC encoder, use --codec h264"),
            (Codec::Hevc, Some(error)) => {
                bail!("no HEVC encoder on this PC would start ({error:#}), use --codec h264")
            }
            (Codec::H264, Some(error)) => {
                bail!("no H.264 encoder on this PC would start ({error:#})")
            }
            (Codec::H264, None) => bail!("this PC has no H.264 encoder"),
        }
    }

    /// Submits a captured frame. Output arrives via [`Encoder::drain`].
    pub fn encode(&self, frame: &Nv12Frame, pts_micros: u64, force_keyframe: bool) -> Result<()> {
        self.lock().encode(frame, pts_micros, force_keyframe)
    }

    /// Changes the target bitrate on the running encoder.
    pub fn set_bitrate(&self, bitrate_bps: i32) -> Result<()> {
        let state = self.lock();
        let api = state
            .codec_api
            .as_ref()
            .context("this encoder has no ICodecAPI, so its bitrate is fixed")?;
        unsafe {
            api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &variant_u32(bitrate_bps.max(1) as u32),
            )
        }
        .context("the encoder refused the new bitrate")
    }

    /// Blocks until every submitted frame has been emitted.
    pub fn finish(&self) -> Result<()> {
        self.lock().finish()
    }

    /// Non-blocking: returns whatever the encoder has finished since last call.
    pub fn drain(&self) -> impl Iterator<Item = EncodedFrame> {
        std::mem::take(&mut self.lock().ready).into_iter()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("encoder state mutex not poisoned")
    }
}

impl From<State> for Encoder {
    fn from(state: State) -> Self {
        Encoder {
            state: Mutex::new(state),
        }
    }
}

/// What the sender asked the encoder for.
struct Config {
    codec: Codec,
    width: u32,
    height: u32,
    bitrate_bps: u32,
    fps: u32,
}

/// A frame on its way into the encoder.
struct Input {
    sample: IMFSample,
    time: i64,
    pts_micros: u64,
    force_keyframe: bool,
    since: Instant,
}

/// What one `ProcessOutput` call produced.
enum Output {
    Sample(IMFSample),
    NeedMoreInput,
    StreamChanged,
}

struct State {
    mft: IMFTransform,
    codec_api: Option<ICodecAPI>,
    /// Present for an asynchronous encoder, which is driven by its events.
    events: Option<IMFMediaEventGenerator>,
    codec: Codec,
    width: usize,
    height: usize,
    /// One frame at the configured rate, in 100 ns units.
    frame_duration: i64,
    stream_info: MFT_OUTPUT_STREAM_INFO,
    params: ParamSets,
    timestamps: Timestamps,
    /// Finished frames waiting for `drain`.
    ready: Vec<EncodedFrame>,
    /// Inputs an asynchronous encoder has asked for and not yet received.
    need_input: u32,
    /// A frame waiting for an asynchronous encoder to ask for input.
    pending: Option<Input>,
    /// A forced keyframe whose frame was dropped, owed to the next one.
    keyframe_owed: bool,
    /// Set once an asynchronous encoder reports its drain is complete.
    drained: bool,
    /// Frames dropped because an asynchronous encoder never asked for them.
    dropped: u64,
    reported_drops: u64,
    last_drop_report: Option<Instant>,
    keyframe_refusal_reported: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        shut_down(&self.mft);
    }
}

impl State {
    /// Configures `mft` and starts it streaming. Fails when it refuses the
    /// media types, or is asynchronous and never asks for input.
    fn start(mft: &IMFTransform, name: Option<String>, config: &Config) -> Result<State> {
        let attributes = unsafe { mft.GetAttributes() }.ok();
        let is_async = attributes
            .as_ref()
            .and_then(|attributes| unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
            .is_some_and(|value| value != 0);
        if is_async {
            // An asynchronous MFT refuses every other call until unlocked
            let attributes = attributes
                .as_ref()
                .context("it is asynchronous but has no attribute store")?;
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .context("it would not unlock")?;
        }

        let mut refused = Vec::new();
        let low_latency = attributes
            .as_ref()
            .is_some_and(|attributes| unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) }.is_ok());
        if !low_latency {
            refused.push("MF_LOW_LATENCY");
        }

        let codec_api = mft.cast::<ICodecAPI>().ok();
        let settings = settings(config.bitrate_bps, config.fps * 2);
        // Some encoders take these only before the media types are set and
        // others only after, so each is offered at both points
        let early = apply_settings(codec_api.as_ref(), &settings);
        set_output_type(mft, config)?;
        if !set_input_type(mft, config)? {
            refused.push("limited-range input");
        }
        let late = apply_settings(codec_api.as_ref(), &settings);
        for (setting, (early, late)) in settings.iter().zip(early.into_iter().zip(late)) {
            if !early && !late {
                refused.push(setting.name);
            }
        }
        // With B-frames on, outputs come in decode order, which is not the
        // order the timestamps went in
        let b_frames_off = !refused.contains(&B_FRAMES_SETTING);

        unsafe {
            // Best effort: only an asynchronous encoder needs these, and the
            // first-input wait below catches one that ignored them
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        let stream_info =
            unsafe { mft.GetOutputStreamInfo(0) }.context("GetOutputStreamInfo failed")?;
        let events = if is_async {
            Some(
                mft.cast::<IMFMediaEventGenerator>()
                    .context("it is asynchronous but has no event generator")?,
            )
        } else {
            None
        };

        let mut state = State {
            mft: mft.clone(),
            codec_api,
            events,
            codec: config.codec,
            width: config.width as usize,
            height: config.height as usize,
            frame_duration: 10_000_000 / i64::from(config.fps),
            stream_info,
            params: ParamSets::default(),
            timestamps: Timestamps::new(if b_frames_off { 0 } else { REORDER_LIMIT }),
            ready: Vec::new(),
            need_input: 0,
            pending: None,
            keyframe_owed: false,
            drained: false,
            dropped: 0,
            reported_drops: 0,
            last_drop_report: None,
            keyframe_refusal_reported: false,
        };
        state.read_sequence_header();
        if is_async {
            state.await_first_input()?;
        }

        println!(
            "encoder: {} {} via Media Foundation ({}), settings refused: {}",
            name.as_deref().unwrap_or("software"),
            config.codec.name(),
            if is_async { "async" } else { "sync" },
            if refused.is_empty() {
                "none".to_string()
            } else {
                refused.join(", ")
            }
        );
        Ok(state)
    }

    fn encode(&mut self, frame: &Nv12Frame, pts_micros: u64, force_keyframe: bool) -> Result<()> {
        if frame.width != self.width || frame.height != self.height {
            bail!(
                "a {}x{} frame reached an encoder set up for {}x{}",
                frame.width,
                frame.height,
                self.width,
                self.height
            );
        }
        let picture = frame
            .data
            .get(..self.width * self.height * 3 / 2)
            .context("the frame holds less than a whole NV12 picture")?;
        let time = micros_to_100ns(pts_micros);
        let input = Input {
            sample: input_sample(picture, time, self.frame_duration)?,
            time,
            pts_micros,
            force_keyframe,
            since: Instant::now(),
        };
        if self.events.is_some() {
            self.submit_async(input)?;
        } else {
            self.submit_sync(input)?;
        }
        self.report_drops();
        Ok(())
    }

    fn submit_sync(&mut self, input: Input) -> Result<()> {
        if input.force_keyframe {
            self.force_keyframe();
        }
        if let Err(error) = unsafe { self.mft.ProcessInput(0, &input.sample, 0) } {
            if error.code() != MF_E_NOTACCEPTING {
                return Err(error).context("ProcessInput failed");
            }
            // It wants its finished output collected before it takes more
            self.collect_sync()?;
            unsafe { self.mft.ProcessInput(0, &input.sample, 0) }.context("ProcessInput failed")?;
        }
        self.timestamps.push(input.time, input.pts_micros);
        self.collect_sync()
    }

    /// Queues the frame for the encoder's next request for input, then
    /// waits briefly for output so a frame the encoder finishes quickly goes
    /// out now. A frame still waiting from an earlier call is dropped in
    /// favour of this newer one.
    fn submit_async(&mut self, input: Input) -> Result<()> {
        self.pump()?;
        if let Some(stale) = self.pending.replace(input) {
            self.drop_pending(stale);
        }
        self.pump()?;
        let before = self.ready.len();
        let deadline = Instant::now() + OUTPUT_WAIT_LIMIT;
        while (self.pending.is_some() || self.ready.len() == before) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            self.pump()?;
        }
        Ok(())
    }

    /// Handles every event an asynchronous encoder has queued, without
    /// blocking, then feeds the waiting frame if the encoder asked for one.
    fn pump(&mut self) -> Result<()> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        loop {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(error).context("the encoder's event queue failed"),
            };
            let status = unsafe { event.GetStatus() }.unwrap_or(S_OK);
            if status.is_err() {
                bail!(
                    "the encoder reported an error ({})",
                    windows::core::Error::from_hresult(status)
                );
            }
            let kind = unsafe { event.GetType() }.unwrap_or(0);
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                match self.process_output()? {
                    Output::Sample(sample) => self.take_output(&sample)?,
                    Output::NeedMoreInput => {}
                    // The encoder sends another METransformHaveOutput for
                    // the frame once the new type is set
                    Output::StreamChanged => self.renegotiate()?,
                }
            } else if kind == METransformDrainComplete.0 as u32 {
                self.drained = true;
            }
        }
        self.feed_pending()
    }

    fn feed_pending(&mut self) -> Result<()> {
        if self
            .pending
            .as_ref()
            .is_some_and(|input| input.since.elapsed() > INPUT_WAIT_LIMIT)
        {
            if let Some(stale) = self.pending.take() {
                self.drop_pending(stale);
            }
        }
        if self.need_input == 0 {
            return Ok(());
        }
        let Some(input) = self.pending.take() else {
            return Ok(());
        };
        if input.force_keyframe || std::mem::take(&mut self.keyframe_owed) {
            self.force_keyframe();
        }
        self.need_input -= 1;
        unsafe { self.mft.ProcessInput(0, &input.sample, 0) }.context("ProcessInput failed")?;
        self.timestamps.push(input.time, input.pts_micros);
        Ok(())
    }

    fn drop_pending(&mut self, input: Input) {
        self.dropped += 1;
        self.keyframe_owed |= input.force_keyframe;
    }

    /// Collects output from a synchronous encoder until it needs more input.
    fn collect_sync(&mut self) -> Result<()> {
        let mut changes = 0;
        loop {
            match self.process_output()? {
                Output::Sample(sample) => self.take_output(&sample)?,
                Output::NeedMoreInput => return Ok(()),
                Output::StreamChanged => {
                    changes += 1;
                    if changes > 3 {
                        bail!("the encoder keeps changing its output type");
                    }
                    self.renegotiate()?;
                }
            }
        }
    }

    fn process_output(&mut self) -> Result<Output> {
        let flags = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0;
        let own_sample = if self.stream_info.dwFlags & flags as u32 != 0 {
            None
        } else {
            // An encoder that says nothing of its output size gets room for
            // a raw frame and then some, which no compressed frame exceeds
            let size = match self.stream_info.cbSize {
                0 => (self.width * self.height * 3 / 2 + 65_536) as u32,
                size => size,
            };
            Some(output_sample(size)?)
        };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(own_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let outcome = unsafe {
            self.mft
                .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
        };
        // Reclaim ownership so whatever ended up in the buffer is released
        // whichever branch below is taken
        let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
        let _events = unsafe { ManuallyDrop::take(&mut buffer.pEvents) };

        match outcome {
            Ok(()) => Ok(Output::Sample(
                sample.context("ProcessOutput succeeded without producing a sample")?,
            )),
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                Ok(Output::NeedMoreInput)
            }
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Output::StreamChanged),
            Err(error) => Err(error).context("ProcessOutput failed"),
        }
    }

    /// Turns one output sample into an `EncodedFrame`, with its parameter
    /// sets recorded and its timestamp taken from the input queue.
    fn take_output(&mut self, sample: &IMFSample) -> Result<()> {
        let bytes = sample_bytes(sample)?;
        let time = unsafe { sample.GetSampleTime() }.ok();
        let clean_point = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
            .is_ok_and(|value| value != 0);
        let unit = parse_access_unit(self.codec, &bytes, &mut self.params);
        if !self.params.complete(self.codec) {
            // Some encoders publish their parameter sets only on the output
            // type, and only once they have started
            self.read_sequence_header();
        }
        if !unit.has_slices {
            // Parameter sets on their own answer no input frame
            return Ok(());
        }
        let pts_micros = self
            .timestamps
            .pop(time)
            .unwrap_or_else(|| time.unwrap_or(0).max(0) as u64 / 10);
        self.ready.push(EncodedFrame {
            data: unit.data,
            keyframe: unit.keyframe || clean_point,
            pts_micros,
            params: self.params.list(self.codec),
        });
        Ok(())
    }

    /// Adopts the output type an encoder offers after a stream change.
    fn renegotiate(&mut self) -> Result<()> {
        let offered = unsafe { self.mft.GetOutputAvailableType(0, 0) }
            .context("GetOutputAvailableType failed after a stream change")?;
        unsafe { self.mft.SetOutputType(0, &offered, 0) }
            .context("SetOutputType failed after a stream change")?;
        self.stream_info =
            unsafe { self.mft.GetOutputStreamInfo(0) }.context("GetOutputStreamInfo failed")?;
        self.read_sequence_header();
        Ok(())
    }

    /// Records the parameter sets an encoder publishes on its output type,
    /// if it does. Wine's never does, so the in-band copies stay the main
    /// source.
    fn read_sequence_header(&mut self) {
        let Ok(current) = (unsafe { self.mft.GetOutputCurrentType(0) }) else {
            return;
        };
        let Ok(size) = (unsafe { current.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }) else {
            return;
        };
        let mut blob = vec![0u8; size as usize];
        if unsafe { current.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }.is_ok() {
            for nal in split_annex_b(&blob) {
                self.params.absorb(classify(self.codec, nal), nal);
            }
        }
    }

    fn await_first_input(&mut self) -> Result<()> {
        let deadline = Instant::now() + FIRST_INPUT_TIMEOUT;
        loop {
            self.pump()?;
            if self.need_input > 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "it never asked for input within {} ms",
                    FIRST_INPUT_TIMEOUT.as_millis()
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn force_keyframe(&mut self) {
        let result = match &self.codec_api {
            Some(api) => {
                unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) }
                    .map_err(|error| error.to_string())
            }
            None => Err("it has no ICodecAPI".to_string()),
        };
        if let Err(reason) = result {
            if !std::mem::replace(&mut self.keyframe_refusal_reported, true) {
                println!(
                    "encoder: keyframe requests are refused ({reason}), so a receiver that \
                     loses sync waits for the next scheduled keyframe"
                );
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        if self.events.is_some() {
            // Give a frame still waiting for room its chance first
            let deadline = Instant::now() + INPUT_WAIT_LIMIT;
            while self.pending.is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
                self.pump()?;
            }
        }
        unsafe {
            // Best effort: COMMAND_DRAIN is the one that matters
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
        }
        .context("ProcessMessage(COMMAND_DRAIN) failed")?;

        if self.events.is_none() {
            return self.collect_sync();
        }
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while !self.drained {
            if Instant::now() >= deadline {
                bail!(
                    "the encoder did not finish draining within {} s",
                    DRAIN_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(1));
            self.pump()?;
        }
        Ok(())
    }

    fn report_drops(&mut self) {
        let total = self.dropped + self.timestamps.skipped;
        if total == self.reported_drops
            || self
                .last_drop_report
                .is_some_and(|at| at.elapsed() < DROP_REPORT_INTERVAL)
        {
            return;
        }
        self.reported_drops = total;
        self.last_drop_report = Some(Instant::now());
        println!(
            "encoder: {total} frames dropped so far ({} the encoder had no room for, {} it \
             skipped)",
            self.dropped, self.timestamps.skipped
        );
    }
}

/// Releases an MFT's worker threads and event queue. Asynchronous encoders
/// need this, and it is harmless on the rest.
fn shut_down(mft: &IMFTransform) {
    if let Ok(shutdown) = mft.cast::<IMFShutdown>() {
        let _ = unsafe { shutdown.Shutdown() };
    }
}

/// Encoder MFTs taking NV12 in and producing `subtype`, in the order
/// `MFTEnumEx` ranks them.
fn enumerate(subtype: &GUID, flags: MFT_ENUM_FLAG) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let result = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    };
    let mut found = Vec::new();
    if activates.is_null() {
        return found;
    }
    if result.is_ok() {
        for index in 0..count as usize {
            // MFTEnumEx hands over one reference per entry, which reading
            // the entry out takes over
            if let Some(activate) = unsafe { std::ptr::read(activates.add(index)) } {
                found.push(activate);
            }
        }
    }
    unsafe { CoTaskMemFree(Some(activates as *const core::ffi::c_void)) };
    found
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut length) }
        .ok()?;
    let name = unsafe { value.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(value.0 as *const core::ffi::c_void)) };
    name.filter(|name| !name.trim().is_empty())
}

/// One `ICodecAPI` property the encoder is asked to take.
struct Setting {
    name: &'static str,
    api: GUID,
    value: u32,
    /// Documented as `VT_BOOL` for encoders, though implementations disagree.
    boolean: bool,
}

/// The setting whose refusal means outputs may arrive out of order.
const B_FRAMES_SETTING: &str = "AVEncMPVDefaultBPictureCount";

/// Constant bitrate, a keyframe every `gop` frames, no B-frames, and the
/// low-latency, real-time modes.
fn settings(bitrate_bps: u32, gop: u32) -> [Setting; 6] {
    [
        Setting {
            name: "AVEncCommonRateControlMode",
            api: CODECAPI_AVEncCommonRateControlMode,
            value: eAVEncCommonRateControlMode_CBR.0 as u32,
            boolean: false,
        },
        Setting {
            name: "AVEncCommonMeanBitRate",
            api: CODECAPI_AVEncCommonMeanBitRate,
            value: bitrate_bps,
            boolean: false,
        },
        Setting {
            name: "AVEncMPVGOPSize",
            api: CODECAPI_AVEncMPVGOPSize,
            value: gop,
            boolean: false,
        },
        Setting {
            name: B_FRAMES_SETTING,
            api: CODECAPI_AVEncMPVDefaultBPictureCount,
            value: 0,
            boolean: false,
        },
        Setting {
            name: "AVLowLatencyMode",
            api: CODECAPI_AVLowLatencyMode,
            value: 1,
            boolean: true,
        },
        Setting {
            name: "AVEncCommonRealTime",
            api: CODECAPI_AVEncCommonRealTime,
            value: 1,
            boolean: true,
        },
    ]
}

/// Offers each setting, returning which the encoder took.
fn apply_settings(api: Option<&ICodecAPI>, settings: &[Setting]) -> Vec<bool> {
    settings
        .iter()
        .map(|setting| {
            let Some(api) = api else {
                return false;
            };
            let first = if setting.boolean {
                variant_bool(setting.value != 0)
            } else {
                variant_u32(setting.value)
            };
            unsafe { api.SetValue(&setting.api, &first) }.is_ok()
                // Implementations disagree on the type of the boolean ones
                || (setting.boolean
                    && unsafe { api.SetValue(&setting.api, &variant_u32(setting.value)) }.is_ok())
        })
        .collect()
}

fn variant_u32(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        // VARIANT.Anonymous.Anonymous is a ManuallyDrop reached through a
        // union field, so assignments go through an explicit `*`
        (*variant.Anonymous.Anonymous).vt = VT_UI4;
        (*variant.Anonymous.Anonymous).Anonymous.ulVal = value;
    }
    variant
}

fn variant_bool(value: bool) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_BOOL;
        (*variant.Anonymous.Anonymous).Anonymous.boolVal =
            if value { VARIANT_TRUE } else { VARIANT_FALSE };
    }
    variant
}

/// Two 32-bit values packed the way Media Foundation's size and ratio
/// attributes store them.
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// A video media type carrying what the input and output types share.
fn video_type(subtype: &GUID, config: &Config) -> Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType failed")?;
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(config.fps, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(media_type)
}

/// Sets the compressed output type. Media Foundation encoders want this
/// before the input type.
fn set_output_type(mft: &IMFTransform, config: &Config) -> Result<()> {
    let subtype = match config.codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    };
    let output = video_type(&subtype, config)?;
    unsafe {
        output.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)?;
        if config.codec == Codec::H264 {
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        }
    }
    unsafe { mft.SetOutputType(0, &output, 0) }.with_context(|| {
        format!(
            "it refused {} output at {}x{}",
            config.codec.name(),
            config.width,
            config.height
        )
    })
}

/// Sets the NV12 input type, declaring limited range and BT.709. Returns
/// false when the encoder only took the type without that declaration.
fn set_input_type(mft: &IMFTransform, config: &Config) -> Result<bool> {
    for declare_range in [true, false] {
        let input = video_type(&MFVideoFormat_NV12, config)?;
        unsafe {
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, config.width)?;
            if declare_range {
                input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
                input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            }
        }
        if unsafe { mft.SetInputType(0, &input, 0) }.is_ok() {
            return Ok(declare_range);
        }
    }
    bail!(
        "it refused NV12 input at {}x{}",
        config.width,
        config.height
    )
}

/// Copies one NV12 picture into a sample. A fresh sample per frame, since
/// an encoder may keep hold of its input until it is done with it.
fn input_sample(picture: &[u8], time: i64, duration: i64) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(picture.len() as u32) }
        .context("MFCreateMemoryBuffer failed")?;
    unsafe {
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(picture.as_ptr(), ptr, picture.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(picture.len() as u32)?;
    }
    let sample = unsafe { MFCreateSample() }.context("MFCreateSample failed")?;
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
    }
    Ok(sample)
}

fn output_sample(size: u32) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(size) }.context("MFCreateMemoryBuffer failed")?;
    let sample = unsafe { MFCreateSample() }.context("MFCreateSample failed")?;
    unsafe { sample.AddBuffer(&buffer) }.context("AddBuffer failed")?;
    Ok(sample)
}

fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .context("ConvertToContiguousBuffer failed")?;
    let mut ptr = std::ptr::null_mut();
    let mut length = 0u32;
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut length)) }.context("Lock failed")?;
    let bytes = if ptr.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(ptr, length as usize) }.to_vec()
    };
    unsafe { buffer.Unlock() }.context("Unlock failed")?;
    Ok(bytes)
}

/// Splits an Annex B byte stream into NAL units, start codes removed.
///
/// Each unit runs to the next `00 00 01`. Trailing zero bytes are trimmed,
/// which removes the extra leading zero of a 4-byte start code and any
/// `trailing_zero_8bits`, since a NAL unit never ends in a zero byte.
fn split_annex_b(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            if let Some(start) = start {
                units.push(trim_trailing_zeros(&stream[start..i]));
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(start) = start {
        units.push(trim_trailing_zeros(&stream[start..]));
    }
    units.retain(|unit| !unit.is_empty());
    units
}

fn trim_trailing_zeros(unit: &[u8]) -> &[u8] {
    let end = unit
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |last| last + 1);
    &unit[..end]
}

/// What a NAL unit is, as far as the wire format cares.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Nal {
    Vps,
    Sps,
    Pps,
    /// A slice of an IDR picture, or for HEVC of any random access point.
    Keyframe,
    Slice,
    Sei,
    /// Access unit delimiters, filler and the rest, none of which a decoder
    /// needs.
    Other,
}

fn classify(codec: Codec, unit: &[u8]) -> Nal {
    let Some(&header) = unit.first() else {
        return Nal::Other;
    };
    match codec {
        Codec::H264 => match header & 0x1F {
            5 => Nal::Keyframe,
            1..=4 => Nal::Slice,
            6 => Nal::Sei,
            7 => Nal::Sps,
            8 => Nal::Pps,
            _ => Nal::Other,
        },
        Codec::Hevc => match (header >> 1) & 0x3F {
            16..=23 => Nal::Keyframe,
            0..=31 => Nal::Slice,
            32 => Nal::Vps,
            33 => Nal::Sps,
            34 => Nal::Pps,
            39 | 40 => Nal::Sei,
            _ => Nal::Other,
        },
    }
}

/// The latest parameter sets seen.
#[derive(Default)]
struct ParamSets {
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    fn absorb(&mut self, kind: Nal, unit: &[u8]) {
        let slot = match kind {
            Nal::Vps => &mut self.vps,
            Nal::Sps => &mut self.sps,
            Nal::Pps => &mut self.pps,
            _ => return,
        };
        *slot = Some(unit.to_vec());
    }

    fn complete(&self, codec: Codec) -> bool {
        self.sps.is_some() && self.pps.is_some() && (codec == Codec::H264 || self.vps.is_some())
    }

    /// SPS then PPS for H.264, with the VPS first for HEVC: the order the
    /// wire carries them in. Empty until every one has been seen.
    fn list(&self, codec: Codec) -> Vec<Vec<u8>> {
        let wanted = match codec {
            Codec::H264 => vec![&self.sps, &self.pps],
            Codec::Hevc => vec![&self.vps, &self.sps, &self.pps],
        };
        wanted
            .into_iter()
            .cloned()
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    }
}

/// One output sample, with its parameter sets set aside.
struct AccessUnit {
    /// Slice and SEI NAL units, each behind a 4-byte big-endian length.
    data: Vec<u8>,
    /// Whether any slice is present. Without one the sample is no frame.
    has_slices: bool,
    keyframe: bool,
}

/// Sorts an Annex B access unit: parameter sets go into `params`, slices and
/// SEI into `data` with their lengths in front, and the rest is dropped.
fn parse_access_unit(codec: Codec, stream: &[u8], params: &mut ParamSets) -> AccessUnit {
    let mut unit = AccessUnit {
        data: Vec::with_capacity(stream.len() + 16),
        has_slices: false,
        keyframe: false,
    };
    for nal in split_annex_b(stream) {
        match classify(codec, nal) {
            kind @ (Nal::Vps | Nal::Sps | Nal::Pps) => params.absorb(kind, nal),
            kind @ (Nal::Keyframe | Nal::Slice | Nal::Sei) => {
                unit.data
                    .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                unit.data.extend_from_slice(nal);
                unit.has_slices |= kind != Nal::Sei;
                unit.keyframe |= kind == Nal::Keyframe;
            }
            Nal::Other => {}
        }
    }
    unit
}

/// Input timestamps in submission order, handed back to outputs.
///
/// An encoder may rewrite sample times (Wine's adds a constant offset) and
/// may drop frames under load, so an output's time is used only to find its
/// input. The offset between the first output's time and the first input's
/// is taken off every output time, and the result looked up in the queue.
/// Entries passed over on the way to the match were dropped by the encoder,
/// except for the `reorder_slack` nearest ones, which B-frames may still
/// deliver. An output whose time matches nothing takes the oldest entry.
struct Timestamps {
    /// (time submitted in 100 ns units, `pts_micros`)
    queue: VecDeque<(i64, u64)>,
    offset: Option<i64>,
    reorder_slack: usize,
    /// Inputs the encoder skipped, going by the times of its outputs.
    skipped: u64,
}

impl Timestamps {
    fn new(reorder_slack: usize) -> Self {
        Timestamps {
            queue: VecDeque::new(),
            offset: None,
            reorder_slack,
            skipped: 0,
        }
    }

    fn push(&mut self, time: i64, pts_micros: u64) {
        self.queue.push_back((time, pts_micros));
    }

    /// The `pts_micros` for an output whose sample time is `time`.
    fn pop(&mut self, time: Option<i64>) -> Option<u64> {
        let &(first_time, _) = self.queue.front()?;
        if let Some(time) = time {
            let target = time - *self.offset.get_or_insert(time - first_time);
            let nearest = self
                .queue
                .iter()
                .enumerate()
                .map(|(index, &(queued, _))| (index, (queued - target).abs()))
                .filter(|&(_, distance)| distance <= TIME_TOLERANCE)
                .min_by_key(|&(_, distance)| distance);
            if let Some((index, _)) = nearest {
                let (_, pts_micros) = self.queue.remove(index)?;
                let passed = index.saturating_sub(self.reorder_slack);
                self.queue.drain(..passed);
                self.skipped += passed as u64;
                return Some(pts_micros);
            }
        }
        self.queue.pop_front().map(|(_, pts_micros)| pts_micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stereowire_proto::video::annex_b;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1f, 0xac];
    const PPS: &[u8] = &[0x68, 0xee, 0x3c, 0x80];
    const AUD: &[u8] = &[0x09, 0xf0];
    const SEI: &[u8] = &[0x06, 0x05, 0x01, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x33];
    const SLICE: &[u8] = &[0x41, 0x9a, 0x21, 0x6c];

    /// Joins NAL units into Annex B with 4-byte start codes, the form Media
    /// Foundation encoders emit.
    fn annex(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|unit| [&[0u8, 0, 0, 1][..], unit].concat())
            .collect()
    }

    #[test]
    fn split_annex_b_handles_both_start_code_lengths() {
        let mut stream = annex(&[AUD, SPS]);
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(IDR);
        assert_eq!(split_annex_b(&stream), vec![AUD, SPS, IDR]);
    }

    #[test]
    fn split_annex_b_drops_trailing_zeros_and_leading_junk() {
        let mut stream = vec![0u8, 0];
        stream.extend(annex(&[SLICE]));
        // trailing_zero_8bits after the last unit
        stream.extend_from_slice(&[0, 0]);
        assert_eq!(split_annex_b(&stream), vec![SLICE]);
        assert!(split_annex_b(&[0, 0, 0]).is_empty());
    }

    #[test]
    fn parameter_sets_are_set_aside_and_listed_in_wire_order() {
        let mut params = ParamSets::default();
        let unit = parse_access_unit(Codec::H264, &annex(&[AUD, SPS, PPS, SEI, IDR]), &mut params);
        assert_eq!(params.list(Codec::H264), vec![SPS.to_vec(), PPS.to_vec()]);
        // HEVC needs a VPS too, which an H.264 stream never has
        assert!(params.list(Codec::Hevc).is_empty());
        assert!(unit.has_slices);
    }

    #[test]
    fn parameter_sets_list_nothing_until_complete() {
        let mut params = ParamSets::default();
        parse_access_unit(Codec::H264, &annex(&[SPS, SLICE]), &mut params);
        assert!(!params.complete(Codec::H264));
        assert!(params.list(Codec::H264).is_empty());
        parse_access_unit(Codec::H264, &annex(&[PPS, SLICE]), &mut params);
        assert!(params.complete(Codec::H264));
        assert!(!params.complete(Codec::Hevc));
        assert_eq!(params.list(Codec::H264).len(), 2);
    }

    #[test]
    fn length_prefixed_data_round_trips_through_annex_b() {
        let mut params = ParamSets::default();
        let unit = parse_access_unit(Codec::H264, &annex(&[AUD, SPS, PPS, SEI, IDR]), &mut params);
        // The access unit delimiter is gone, and the parameter sets come
        // back in front, the way the receiver rebuilds a stream
        assert_eq!(
            annex_b(&params.list(Codec::H264), &unit.data),
            annex(&[SPS, PPS, SEI, IDR])
        );
        assert_eq!(annex_b(&[], &unit.data), annex(&[SEI, IDR]));
    }

    #[test]
    fn keyframes_are_recognised_by_nal_type() {
        let mut params = ParamSets::default();
        assert!(parse_access_unit(Codec::H264, &annex(&[SPS, PPS, IDR]), &mut params).keyframe);
        assert!(!parse_access_unit(Codec::H264, &annex(&[AUD, SLICE]), &mut params).keyframe);
        // SEI alone is no frame at all
        assert!(!parse_access_unit(Codec::H264, &annex(&[SEI]), &mut params).has_slices);

        // HEVC keeps the type in bits 1-6 of a two-byte header: 19 is
        // IDR_W_RADL, 1 is TRAIL_R, 32-34 are VPS, SPS and PPS
        let vps: &[u8] = &[0x40, 0x01, 0x0c];
        let sps: &[u8] = &[0x42, 0x01, 0x01];
        let pps: &[u8] = &[0x44, 0x01, 0xc0];
        let idr: &[u8] = &[0x26, 0x01, 0xaf];
        let trail: &[u8] = &[0x02, 0x01, 0xd0];
        let unit = parse_access_unit(Codec::Hevc, &annex(&[vps, sps, pps, idr]), &mut params);
        assert!(unit.keyframe);
        assert_eq!(
            params.list(Codec::Hevc),
            vec![vps.to_vec(), sps.to_vec(), pps.to_vec()]
        );
        assert!(!parse_access_unit(Codec::Hevc, &annex(&[trail]), &mut params).keyframe);
    }

    /// Frame times at 60 fps in 100 ns units, and their `pts_micros`.
    fn queue(frames: usize, slack: usize) -> Timestamps {
        let mut timestamps = Timestamps::new(slack);
        for frame in 0..frames as i64 {
            timestamps.push(frame * 166_666, frame as u64 * 16_666);
        }
        timestamps
    }

    /// Wine's encoder adds this to every output time.
    const OFFSET: i64 = 36_000_000_000_000;

    #[test]
    fn timestamps_survive_an_offset() {
        let mut timestamps = queue(3, 0);
        assert_eq!(timestamps.pop(Some(OFFSET)), Some(0));
        assert_eq!(timestamps.pop(Some(OFFSET + 166_666)), Some(16_666));
        assert_eq!(timestamps.pop(Some(OFFSET + 333_332)), Some(33_332));
        assert_eq!(timestamps.skipped, 0);
    }

    #[test]
    fn a_frame_the_encoder_dropped_is_skipped() {
        let mut timestamps = queue(4, 0);
        assert_eq!(timestamps.pop(Some(OFFSET)), Some(0));
        // Frame 1 never comes out
        assert_eq!(timestamps.pop(Some(OFFSET + 2 * 166_666)), Some(2 * 16_666));
        assert_eq!(timestamps.pop(Some(OFFSET + 3 * 166_666)), Some(3 * 16_666));
        assert_eq!(timestamps.skipped, 1);
        assert_eq!(timestamps.pop(None), None);
    }

    #[test]
    fn reordered_outputs_keep_their_own_timestamps_when_b_frames_are_possible() {
        let mut timestamps = queue(4, REORDER_LIMIT);
        let order = [0i64, 2, 1, 3];
        let got: Vec<_> = order
            .iter()
            .map(|&frame| timestamps.pop(Some(OFFSET + frame * 166_666)))
            .collect();
        assert_eq!(got, vec![Some(0), Some(33_332), Some(16_666), Some(49_998)]);
        assert_eq!(timestamps.skipped, 0);
    }

    #[test]
    fn outputs_without_a_usable_time_take_the_oldest_entry() {
        let mut timestamps = queue(3, 0);
        assert_eq!(timestamps.pop(None), Some(0));
        // The first time seen sets the offset, so it matches whatever is
        // oldest then, and a time matching nothing falls back to the order
        assert_eq!(timestamps.pop(Some(5)), Some(16_666));
        assert_eq!(timestamps.pop(Some(123_456_789)), Some(33_332));
        assert_eq!(timestamps.pop(Some(5)), None);
    }
}
