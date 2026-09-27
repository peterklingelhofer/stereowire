//! Headless H.264/HEVC decoding via Media Foundation.
//!
//! Mirrors `mac::decoder::Decoder`: the self-test uses this to check what
//! actually came out the far end, and the live viewer uses it to get pixels
//! Direct3D can upload. Media Foundation has no equivalent of VideoToolbox's
//! format-from-parameter-sets call, so every access unit is rebuilt as
//! Annex B with the stored parameter sets prepended (see `decode`), the same
//! bitstream form `annex_b` already builds for Windows Media Foundation.
//!
//! `DecodedFrame.luma` is full range (0-255) on both platforms, matching
//! what `mac::decoder::Decoder` hands back: VideoToolbox's own
//! decompression session expands it internally. Media Foundation hands
//! back the bitstream's literal samples instead, so whether those are
//! already full range depends on the decoder: see `decide_full_range` for
//! how that gets worked out, and `expand_limited_range` for the step taken
//! only when the decoder delivered limited range, which today means
//! winegstreamer's decoder under Wine.

use std::collections::VecDeque;

use anyhow::{anyhow, bail, Context, Result};
use stereowire_proto::nal::{annex_b, sps_geometry};
use stereowire_proto::packet::Codec;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use super::mf::{self, ProcessOutcome};

/// A decoded frame, cropped to the display area and packed for the renderer.
pub struct DecodedFrame {
    /// `width * height` bytes, pitch removed.
    pub luma: Vec<u8>,
    /// Interleaved U,V at half resolution, NV12 order, regardless of what
    /// layout the decoder actually handed back.
    pub chroma: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub pts_micros: u64,
}

/// How chroma is laid out in a decoder's output buffer. The spec asks for
/// NV12, but some MFTs (winegstreamer's under Wine, notably) only offer a
/// planar 4:2:0 type, so this records which one so the output reader can
/// re-interleave it into NV12 order regardless.
#[derive(Clone, Copy, PartialEq)]
enum ChromaLayout {
    Nv12,
    /// Planar 4:2:0. `v_before_u` is true for YV12, false for I420/IYUV.
    Planar420 {
        v_before_u: bool,
    },
}

impl ChromaLayout {
    /// The label the startup log line reports.
    fn label(self) -> &'static str {
        match self {
            ChromaLayout::Nv12 => "NV12",
            ChromaLayout::Planar420 { v_before_u: true } => "YV12",
            ChromaLayout::Planar420 { v_before_u: false } => "I420",
        }
    }
}

/// Where a `Negotiation`'s range decision came from, for the startup log
/// line. See `decide_full_range`.
#[derive(Clone, Copy, PartialEq)]
enum RangeSource {
    /// The MFT's own `MF_MT_VIDEO_NOMINAL_RANGE` on the negotiated output
    /// type.
    OutputType,
    /// The bitstream's own `video_full_range_flag`, the output type having
    /// said nothing.
    ParameterSets,
    /// Neither said anything: limited range, the conservative fallback.
    Default,
}

impl RangeSource {
    fn label(self) -> &'static str {
        match self {
            RangeSource::OutputType => "output type",
            RangeSource::ParameterSets => "parameter sets",
            RangeSource::Default => "default",
        }
    }
}

/// Coded vs. display geometry, buffer layout, and delivered range,
/// renegotiated whenever the MFT reports `MF_E_TRANSFORM_STREAM_CHANGE`.
struct Negotiation {
    coded_height: u32,
    display_x: u32,
    display_y: u32,
    display_width: u32,
    display_height: u32,
    fallback_stride: i32,
    provides_samples: bool,
    output_size: u32,
    chroma_layout: ChromaLayout,
    /// Whether the decoder's output is full range (0-255) rather than
    /// limited (16-235). See `decide_full_range`.
    full_range: bool,
    /// Where `full_range` came from, for the startup log line.
    range_source: RangeSource,
}

pub struct Decoder {
    mft: IMFTransform,
    /// The transform's friendly name, when enumeration found it, for the
    /// startup line.
    name: Option<String>,
    params: Vec<Vec<u8>>,
    codec: Codec,
    /// The display aperture read from the SPS itself, if it parsed cleanly;
    /// see `sps_geometry` for why this overrides whatever (or nothing) the
    /// MFT's own negotiation reports.
    known_display: Option<(u32, u32, u32, u32)>,
    /// The SPS's own `video_full_range_flag`, when it parsed. See
    /// `decide_full_range` for how this and the MFT's own
    /// `MF_MT_VIDEO_NOMINAL_RANGE` are combined.
    known_full_range: Option<bool>,
    /// Whether `MF_LOW_LATENCY` or `CODECAPI_AVLowLatencyMode` (either
    /// route) was accepted, for the startup line reprinted on a range
    /// change.
    low_latency_on: bool,
    neg: Negotiation,
    queue: VecDeque<DecodedFrame>,
    /// Set once a frame that does not fit its buffer has been reported, so
    /// a misnegotiated stream is reported once rather than every frame.
    misfit_reported: bool,
}

impl Decoder {
    pub fn new(codec: Codec, params: &[Vec<u8>]) -> Result<Self> {
        mf::ensure_media_foundation()?;

        let subtype = match codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
        };
        let (mft, name) = activate_decoder(codec, &subtype)?;

        // Both matter on real Windows, where the Microsoft decoder otherwise
        // holds frames back. Under Wine one or both are commonly
        // unavailable, reported through the startup line below rather than
        // as separate complaints
        let attrs = unsafe { mft.GetAttributes() }.ok();
        let mf_low_latency_ok = attrs
            .as_ref()
            .map(|attrs| unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) }.is_ok())
            .unwrap_or(false);
        // Microsoft's H.264 decoder documentation lists this as a transform
        // attribute alongside MF_LOW_LATENCY. The older ICodecAPI route is
        // tried too since that costs nothing and is what some MFTs still
        // need
        let codec_api_ok = attrs
            .as_ref()
            .map(|attrs| unsafe { attrs.SetUINT32(&CODECAPI_AVLowLatencyMode, 1) }.is_ok())
            .unwrap_or(false)
            || mft
                .cast::<ICodecAPI>()
                .map(|api| {
                    unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &mf::variant_bool(true)) }
                        .is_ok()
                })
                .unwrap_or(false);
        let low_latency_on = mf_low_latency_ok || codec_api_ok;

        let geometry = sps_geometry(codec, params);
        let frame_size = match &geometry {
            Some(g) => g.coded,
            None => {
                println!(
                    "decoder: could not read a coded size from the parameter sets, guessing \
                     {}x{} until the first frame corrects it",
                    FALLBACK_FRAME_SIZE.0, FALLBACK_FRAME_SIZE.1
                );
                FALLBACK_FRAME_SIZE
            }
        };
        let known_display = geometry.as_ref().map(|g| g.display);
        let known_full_range = geometry.as_ref().and_then(|g| g.full_range);
        let neg = set_input_type(&mft, &subtype, frame_size)
            .and_then(|()| negotiate_output(&mft, known_display, known_full_range))
            .inspect_err(|_| mf::shut_down(&mft))?;
        unsafe {
            // Best effort: an MFT that ignores these still decodes correctly.
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }

        let decoder = Decoder {
            mft,
            name,
            params: params.to_vec(),
            codec,
            known_display,
            known_full_range,
            low_latency_on,
            neg,
            queue: VecDeque::new(),
            misfit_reported: false,
        };
        decoder.log_negotiation();
        Ok(decoder)
    }

    /// Decodes one frame. Finished frames are queued for `drain`.
    pub fn decode(&mut self, data: &[u8], pts_micros: u64) -> Result<()> {
        let bitstream = annex_b(&self.params, data);
        let sample = mf::sample_with_bytes(&bitstream)?;
        unsafe { sample.SetSampleTime(mf::micros_to_100ns(pts_micros)) }?;
        unsafe { self.mft.ProcessInput(0, &sample, 0) }.context("ProcessInput failed")?;
        self.drain_ready()
    }

    /// Tells the decoder no more input is coming, so it releases whatever
    /// frames it was still holding onto internally for reordering or
    /// pipelining. Unlike VideoToolbox's synchronous decode, an MFT is free
    /// to buffer several frames before producing any output at all, so a
    /// caller that wants everything back (the self-test does) must call
    /// this once the stream is over.
    pub fn finish(&mut self) -> Result<()> {
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .context("ProcessMessage(COMMAND_DRAIN) failed")?;
        self.drain_ready()
    }

    pub fn drain(&mut self) -> impl Iterator<Item = DecodedFrame> + '_ {
        self.queue.drain(..)
    }

    /// Pumps `ProcessOutput` until the MFT has nothing more to give right
    /// now, queuing whatever it produces and renegotiating through any
    /// stream change along the way.
    fn drain_ready(&mut self) -> Result<()> {
        loop {
            match self.process_output_once()? {
                ProcessOutcome::Sample(sample) => {
                    let read = read_output_sample(&sample, &self.neg, &mut self.misfit_reported);
                    if let Some(frame) = read? {
                        self.queue.push_back(frame);
                    }
                }
                ProcessOutcome::NeedMoreInput => break,
                ProcessOutcome::StreamChange => {
                    let previous = (self.neg.full_range, self.neg.range_source);
                    self.neg =
                        negotiate_output(&self.mft, self.known_display, self.known_full_range)?;
                    // Reprint only if the range decision changed
                    if (self.neg.full_range, self.neg.range_source) != previous {
                        self.log_negotiation();
                    }
                }
            }
        }
        Ok(())
    }

    fn process_output_once(&self) -> Result<ProcessOutcome> {
        let own_sample = if self.neg.provides_samples {
            None
        } else {
            Some(mf::empty_sample(self.neg.output_size)?)
        };
        mf::process_output(&self.mft, own_sample)
    }

    /// The startup line describing the negotiated output. Shared between
    /// `Decoder::new` and the reprint after a stream change that changes the
    /// range decision.
    fn log_negotiation(&self) {
        let neg = &self.neg;
        let name = self.name.as_deref().map(|name| format!("{name} "));
        println!(
            "decoder: {}{} via Media Foundation, {} output, {} range from {}, low latency {}",
            name.unwrap_or_default(),
            self.codec.name(),
            neg.chroma_layout.label(),
            if neg.full_range { "full" } else { "limited" },
            neg.range_source.label(),
            if self.low_latency_on { "on" } else { "off" },
        );
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        mf::shut_down(&self.mft);
    }
}

/// Enumerates a synchronous decoder MFT for `subtype`, falling back to the
/// well-known CLSID (as some Wine configurations need) before giving up.
/// Returns the transform with its friendly name, when enumeration found it.
fn activate_decoder(codec: Codec, subtype: &GUID) -> Result<(IMFTransform, Option<String>)> {
    let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    if let Some(activate) = mf::enumerate(MFT_CATEGORY_VIDEO_DECODER, flags, Some(subtype), None)
        .into_iter()
        .next()
    {
        let name = mf::friendly_name(&activate);
        let mft = unsafe { activate.ActivateObject::<IMFTransform>() }
            .context("ActivateObject failed")?;
        return Ok((mft, name));
    }
    let clsid = match codec {
        Codec::H264 => &CLSID_MSH264DecoderMFT,
        Codec::Hevc => &CLSID_MSH265DecoderMFT,
    };
    let fallback: windows::core::Result<IMFTransform> =
        unsafe { CoCreateInstance(clsid, None, CLSCTX_INPROC_SERVER) };
    fallback.map(|mft| (mft, None)).map_err(|e| match codec {
        Codec::Hevc => anyhow!(
            "this PC has no HEVC decoder. Ask the sender to add --codec h264, or install HEVC \
             Video Extensions from the Microsoft Store"
        ),
        Codec::H264 => anyhow!("this PC has no H.264 decoder ({e})"),
    })
}

fn set_input_type(mft: &IMFTransform, subtype: &GUID, frame_size: (u32, u32)) -> Result<()> {
    let input_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType failed")?;
    unsafe {
        input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        input_type.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_MixedInterlaceOrProgressive.0 as u32,
        )?;
        // On real Windows a wrong guess here is only a missed optimisation:
        // the decoder corrects it via MF_E_TRANSFORM_STREAM_CHANGE once it
        // has parsed the first access unit. Some Wine decoder backends do
        // not renegotiate that way, so `frame_size` is read from the SPS
        // itself (see `sps_geometry`) whenever that parses cleanly, and
        // only falls back to a generic guess when it does not.
        input_type.SetUINT64(&MF_MT_FRAME_SIZE, mf::pack(frame_size.0, frame_size.1))?;
        mft.SetInputType(0, &input_type, 0)?;
    }
    Ok(())
}

/// A guess at the coded picture size to declare before any bitstream has
/// been seen. Falls back to a generic size that real Windows would still
/// correct via a stream change; see `set_input_type`.
const FALLBACK_FRAME_SIZE: (u32, u32) = (1920, 1080);

/// Enumerates the MFT's offered output types, preferring NV12, and returns
/// the negotiated geometry, buffer layout, and delivered range.
/// `known_display`, when set, overrides whatever aperture (or lack of one)
/// the MFT itself reports. See `sps_geometry` for why that is more
/// trustworthy than asking the MFT. `known_full_range` is the same kind of
/// fallback for the range decision, used only when the negotiated output
/// type itself says nothing. See `decide_full_range`.
fn negotiate_output(
    mft: &IMFTransform,
    known_display: Option<(u32, u32, u32, u32)>,
    known_full_range: Option<bool>,
) -> Result<Negotiation> {
    let mut nv12_type: Option<IMFMediaType> = None;
    let mut planar_type: Option<(IMFMediaType, ChromaLayout)> = None;
    for i in 0.. {
        let candidate = match unsafe { mft.GetOutputAvailableType(0, i) } {
            Ok(t) => t,
            Err(_) => break,
        };
        let subtype = unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
        if subtype == MFVideoFormat_NV12 && nv12_type.is_none() {
            nv12_type = Some(candidate);
        } else if (subtype == MFVideoFormat_I420 || subtype == MFVideoFormat_IYUV)
            && planar_type.is_none()
        {
            planar_type = Some((candidate, ChromaLayout::Planar420 { v_before_u: false }));
        } else if subtype == MFVideoFormat_YV12 && planar_type.is_none() {
            planar_type = Some((candidate, ChromaLayout::Planar420 { v_before_u: true }));
        }
    }

    let (chosen_type, chroma_layout) = if let Some(t) = nv12_type {
        (t, ChromaLayout::Nv12)
    } else if let Some((t, layout)) = planar_type {
        (t, layout)
    } else {
        bail!("no NV12 or planar 4:2:0 output type offered");
    };
    unsafe { mft.SetOutputType(0, &chosen_type, 0) }.context("SetOutputType failed")?;

    let stream_info =
        unsafe { mft.GetOutputStreamInfo(0) }.context("GetOutputStreamInfo failed")?;
    let provides_samples = (stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

    let current = unsafe { mft.GetOutputCurrentType(0) }.unwrap_or(chosen_type);
    let (coded_width, coded_height) = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map(|packed| ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32))
        .unwrap_or((0, 0));
    let fallback_stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|s| s as i32)
        .unwrap_or(coded_width as i32);
    let (display_x, display_y, display_width, display_height) = known_display
        .or_else(|| read_display_aperture(&current))
        .unwrap_or((0, 0, coded_width, coded_height));
    let (full_range, range_source) = decide_full_range(&current, known_full_range);

    Ok(Negotiation {
        coded_height,
        display_x,
        display_y,
        display_width,
        display_height,
        fallback_stride,
        provides_samples,
        output_size: stream_info.cbSize,
        chroma_layout,
        full_range,
        range_source,
    })
}

fn read_display_aperture(media_type: &IMFMediaType) -> Option<(u32, u32, u32, u32)> {
    let mut buf = [0u8; std::mem::size_of::<MFVideoArea>()];
    unsafe { media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut buf, None) }.ok()?;
    let area: MFVideoArea = unsafe { std::ptr::read(buf.as_ptr() as *const MFVideoArea) };
    Some((
        area.OffsetX.value as u32,
        area.OffsetY.value as u32,
        area.Area.cx as u32,
        area.Area.cy as u32,
    ))
}

/// Reads `MF_MT_VIDEO_NOMINAL_RANGE` off a media type. `Some(true)` is
/// `MFNominalRange_Normal` (full range, value 1) and `Some(false)` is
/// `MFNominalRange_Wide` (limited range, value 2, and what winegstreamer's
/// decoder sets). Anything else, including the attribute being absent, is
/// `None`.
fn read_nominal_range(media_type: &IMFMediaType) -> Option<bool> {
    match unsafe { media_type.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) } {
        Ok(v) if v == MFNominalRange_Normal.0 as u32 => Some(true),
        Ok(v) if v == MFNominalRange_Wide.0 as u32 => Some(false),
        _ => None,
    }
}

/// Whether the decoder's output is full range, and where that came from:
/// the output type's own say when it has one, else the bitstream's
/// `video_full_range_flag`, else limited range as the fallback when neither
/// says anything.
fn decide_full_range(
    output_type: &IMFMediaType,
    known_full_range: Option<bool>,
) -> (bool, RangeSource) {
    if let Some(full) = read_nominal_range(output_type) {
        (full, RangeSource::OutputType)
    } else if let Some(full) = known_full_range {
        (full, RangeSource::ParameterSets)
    } else {
        (false, RangeSource::Default)
    }
}

/// Reads one decoded output sample, cropping to the display aperture and
/// returning tightly packed luma and NV12-order chroma. `None` means the
/// buffer could not be addressed, or cannot hold the negotiated geometry,
/// which callers treat as no frame this round. The first frame that does not
/// fit is reported, and `misfit_reported` records that it was.
fn read_output_sample(
    sample: &IMFSample,
    neg: &Negotiation,
    misfit_reported: &mut bool,
) -> Result<Option<DecodedFrame>> {
    let sample_time = unsafe { sample.GetSampleTime() }.unwrap_or(0);
    let buffer = unsafe { sample.GetBufferByIndex(0) }.context("GetBufferByIndex failed")?;

    // Both locks also report how many bytes the buffer holds from `ptr`. A 2D
    // buffer takes Lock2DSize, since its contiguous length leaves out the
    // padding at the end of each row
    let buffer2d: Option<IMF2DBuffer2> = buffer.cast().ok();
    let (ptr, pitch, available): (*const u8, i32, usize) = if let Some(buffer2d) = &buffer2d {
        let (mut scanline, mut start) = (std::ptr::null_mut(), std::ptr::null_mut());
        let (mut lock_pitch, mut length) = (0i32, 0u32);
        unsafe {
            buffer2d.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline,
                &mut lock_pitch,
                &mut start,
                &mut length,
            )
        }
        .context("Lock2DSize failed")?;
        // The buffer starts before the top row only when its rows run bottom up
        let skipped = (scanline as usize).saturating_sub(start as usize);
        let held = (length as usize).saturating_sub(skipped);
        (scanline, lock_pitch, held)
    } else {
        let mut linear = std::ptr::null_mut();
        let mut length = 0u32;
        unsafe { buffer.Lock(&mut linear, Some(&mut length), None) }.context("Lock failed")?;
        (linear, neg.fallback_stride, length as usize)
    };

    let display = (
        neg.display_x as usize,
        neg.display_y as usize,
        neg.display_width as usize,
        neg.display_height as usize,
    );
    let usable = !ptr.is_null() && pitch > 0 && neg.display_width > 0 && neg.display_height > 0;
    let coded_height = neg.coded_height as usize;
    let fitting = usable && fits(pitch as usize, coded_height, available, display);
    if usable && !fitting && !std::mem::replace(misfit_reported, true) {
        println!(
            "decoder: skipping frames, since a {}x{} picture at ({}, {}) in {coded_height} rows \
             at pitch {pitch} does not fit the {available}-byte buffer",
            neg.display_width, neg.display_height, neg.display_x, neg.display_y
        );
    }
    if !fitting {
        if let Some(buffer2d) = &buffer2d {
            unsafe { buffer2d.Unlock2D().ok() };
        } else {
            unsafe { buffer.Unlock().ok() };
        }
        return Ok(None);
    }
    let pitch = pitch as usize;

    // SAFETY: the buffer stays locked until the Unlock below, its lock
    // reported `available` bytes from `ptr`, and `fits` checked those cover
    // coded_height rows of luma at this pitch and the chroma read after them
    let full_luma = unsafe { std::slice::from_raw_parts(ptr, pitch * neg.coded_height as usize) };
    let mut luma = crop_plane(
        full_luma,
        pitch,
        neg.display_x as usize,
        neg.display_y as usize,
        neg.display_width as usize,
        neg.display_height as usize,
    );
    // The MFT hands back the bitstream's literal samples. VideoToolbox
    // writes full-range codewords (see the module doc comment), so on real
    // Windows, where Microsoft's decoder passes them through unchanged,
    // luma is already full range here. Wine's winegstreamer decoder
    // converts to limited range (16-235) instead and says so on the
    // negotiated output type (see `decide_full_range`), so only there does
    // this step run, keeping `DecodedFrame.luma` meaning the same thing
    // (full range) on both platforms, for the self-test's comparison
    // against a full-range reference and for the renderer alike
    if !neg.full_range {
        for byte in &mut luma {
            *byte = expand_limited_range(*byte);
        }
    }

    let chroma = match neg.chroma_layout {
        ChromaLayout::Nv12 => {
            let uv_offset = pitch * neg.coded_height as usize;
            let uv_rows = neg.coded_height as usize / 2;
            // SAFETY: still locked, and `fits` checked the buffer also covers
            // coded_height / 2 rows of interleaved chroma at this pitch after
            // the luma
            let full_uv =
                unsafe { std::slice::from_raw_parts(ptr.add(uv_offset), pitch * uv_rows) };
            crop_plane(
                full_uv,
                pitch,
                neg.display_x as usize,
                (neg.display_y / 2) as usize,
                neg.display_width as usize,
                (neg.display_height / 2) as usize,
            )
        }
        ChromaLayout::Planar420 { v_before_u } => {
            // Planar 4:2:0: the Y plane, then two half-resolution,
            // half-pitch chroma planes back to back. YV12 orders them V
            // then U; I420/IYUV order them U then V.
            let chroma_pitch = pitch / 2;
            let chroma_rows = neg.coded_height as usize / 2;
            let first_offset = pitch * neg.coded_height as usize;
            let second_offset = first_offset + chroma_pitch * chroma_rows;
            // SAFETY: still locked, and `fits` checked the buffer also covers
            // coded_height / 2 rows at the full pitch after the luma, room for
            // both of these half-pitch planes
            let full_first = unsafe {
                std::slice::from_raw_parts(ptr.add(first_offset), chroma_pitch * chroma_rows)
            };
            let full_second = unsafe {
                std::slice::from_raw_parts(ptr.add(second_offset), chroma_pitch * chroma_rows)
            };
            let (full_u, full_v) = if v_before_u {
                (full_second, full_first)
            } else {
                (full_first, full_second)
            };
            let crop_chroma = |plane: &[u8]| {
                crop_plane(
                    plane,
                    chroma_pitch,
                    (neg.display_x / 2) as usize,
                    (neg.display_y / 2) as usize,
                    (neg.display_width / 2) as usize,
                    (neg.display_height / 2) as usize,
                )
            };
            interleave_chroma(&crop_chroma(full_u), &crop_chroma(full_v))
        }
    };

    if let Some(buffer2d) = &buffer2d {
        unsafe { buffer2d.Unlock2D() }.context("Unlock2D failed")?;
    } else {
        unsafe { buffer.Unlock() }.context("Unlock failed")?;
    }

    Ok(Some(DecodedFrame {
        luma,
        chroma,
        width: neg.display_width as usize,
        height: neg.display_height as usize,
        pts_micros: mf::hundred_ns_to_micros(sample_time),
    }))
}

/// Whether a buffer holding `available` bytes, with rows `pitch` bytes apart,
/// has room for the `coded_height` rows of luma and the half as many rows of
/// chroma after them that `read_output_sample` reads, and whether the display
/// area, as (x, y, width, height), lies inside that luma plane with even
/// sides. The chroma crops halve the area, so these also keep them inside
/// their planes.
fn fits(
    pitch: usize,
    coded_height: usize,
    available: usize,
    (x, y, width, height): (usize, usize, usize, usize),
) -> bool {
    let planes = pitch
        .saturating_mul(coded_height)
        .saturating_add(pitch.saturating_mul(coded_height / 2));
    planes <= available
        && x.saturating_add(width) <= pitch
        && y.saturating_add(height) <= coded_height
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
}

/// Copies a `width x height` region out of a plane, discarding the pitch
/// padding. Pure and safe: everything unsafe about reading the decoder's
/// buffer happens in `read_output_sample`, before this is called.
fn crop_plane(
    src: &[u8],
    pitch: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = (y + row) * pitch + x;
        out.extend_from_slice(&src[start..start + width]);
    }
    out
}

/// Interleaves two half-resolution planar chroma planes into NV12 order
/// (U, V per sample pair).
fn interleave_chroma(u: &[u8], v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(u.len() + v.len());
    for (a, b) in u.iter().zip(v.iter()) {
        out.push(*a);
        out.push(*b);
    }
    out
}

/// BT.709/BT.601 limited range (16-235) to full range (0-255), for one luma
/// byte, rounded to the nearest level. A lookup table computed once: this
/// runs over every pixel of every decoded frame.
const LUMA_FULL_RANGE: [u8; 256] = build_luma_full_range();

const fn build_luma_full_range() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut v = 0usize;
    while v < 256 {
        let scaled = ((v as i32 - 16) * 255 + 109) / 219;
        table[v] = if scaled < 0 {
            0
        } else if scaled > 255 {
            255
        } else {
            scaled as u8
        };
        v += 1;
    }
    table
}

fn expand_limited_range(byte: u8) -> u8 {
    LUMA_FULL_RANGE[byte as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_plane_removes_pitch_padding() {
        // A 4x2 image with a 6-byte pitch: 2 bytes of padding per row.
        let src = vec![1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        assert_eq!(
            crop_plane(&src, 6, 0, 0, 4, 2),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    #[test]
    fn crop_plane_honours_an_offset_aperture() {
        let src = vec![
            0, 0, 0, 0, //
            0, 9, 8, 0, //
            0, 7, 6, 0, //
            0, 0, 0, 0, //
        ];
        assert_eq!(crop_plane(&src, 4, 1, 1, 2, 2), vec![9, 8, 7, 6]);
    }

    #[test]
    fn crop_plane_at_the_full_extent_is_a_plain_copy() {
        let src = vec![1u8, 2, 3, 4, 5, 6];
        assert_eq!(crop_plane(&src, 3, 0, 0, 3, 2), src);
    }

    /// 32 coded rows at a 64-byte pitch: 2048 bytes of luma, then 16 rows
    /// of chroma taking 1024 more.
    const PITCH: usize = 64;
    const ROWS: usize = 32;
    const PLANES: usize = 3072;

    #[test]
    fn fits_accepts_a_display_area_inside_the_planes() {
        assert!(fits(PITCH, ROWS, PLANES, (8, 4, 48, 24)));
        assert!(fits(PITCH, ROWS, PLANES, (0, 0, PITCH, ROWS)));
    }

    #[test]
    fn fits_refuses_a_right_edge_past_the_pitch() {
        assert!(!fits(PITCH, ROWS, PLANES, (24, 0, 48, 32)));
    }

    #[test]
    fn fits_refuses_a_bottom_edge_past_the_coded_rows() {
        assert!(!fits(PITCH, ROWS, PLANES, (0, 16, 64, 32)));
    }

    #[test]
    fn fits_refuses_a_buffer_shorter_than_the_planes() {
        assert!(!fits(PITCH, ROWS, PLANES - 1, (0, 0, 64, 32)));
    }

    #[test]
    fn fits_refuses_odd_sides_the_chroma_crop_cannot_halve() {
        assert!(!fits(PITCH, ROWS, PLANES, (0, 0, 63, 32)));
        assert!(!fits(PITCH, ROWS, PLANES, (0, 0, 64, 31)));
    }

    #[test]
    fn expand_limited_range_maps_the_studio_range_endpoints() {
        assert_eq!(expand_limited_range(16), 0);
        assert_eq!(expand_limited_range(235), 255);
        // The middle of the range should land near the middle, not exactly
        // on it: 219 studio levels do not divide evenly into 256.
        assert!((123..=132).contains(&expand_limited_range(128)));
    }

    #[test]
    fn expand_limited_range_clamps_rather_than_wraps_outside_the_studio_range() {
        // Values outside 16-235 legitimately occur (headroom/footroom) and
        // must saturate rather than overflow.
        assert_eq!(expand_limited_range(0), 0);
        assert_eq!(expand_limited_range(255), 255);
    }

    #[test]
    fn expand_limited_range_is_monotonic() {
        for v in 0u8..255 {
            assert!(expand_limited_range(v) <= expand_limited_range(v + 1));
        }
    }

    #[test]
    fn interleave_chroma_alternates_u_and_v() {
        let u = vec![1u8, 2, 3];
        let v = vec![9u8, 8, 7];
        assert_eq!(interleave_chroma(&u, &v), vec![1, 9, 2, 8, 3, 7]);
    }

    #[test]
    fn interleave_chroma_stops_at_the_shorter_plane() {
        let u = vec![1u8, 2, 3];
        let v = vec![9u8];
        assert_eq!(interleave_chroma(&u, &v), vec![1, 9]);
    }
}
