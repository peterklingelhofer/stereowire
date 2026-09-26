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
use std::mem::ManuallyDrop;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use stereowire_proto::packet::Codec;
use stereowire_proto::video::annex_b;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL};

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

/// Starts Media Foundation once per process; every `Decoder::new` calls this.
static MF_STARTUP: OnceLock<std::result::Result<(), String>> = OnceLock::new();

fn ensure_media_foundation() -> Result<()> {
    MF_STARTUP
        .get_or_init(|| unsafe {
            // Apartment threaded, the mode cpal picks for WASAPI, because audio
            // playback may have initialised COM on this thread first. A
            // RPC_E_CHANGED_MODE reply only says another mode is already
            // active, and a synchronous MFT works under either
            let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if init.is_err() && init != RPC_E_CHANGED_MODE {
                return Err(windows::core::Error::from_hresult(init).to_string());
            }
            MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|message| anyhow!("could not start Media Foundation: {message}"))
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

enum Drain {
    Frame(IMFSample),
    NeedMoreInput,
    StreamChanged,
}

pub struct Decoder {
    mft: IMFTransform,
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
}

impl Decoder {
    pub fn new(codec: Codec, params: &[Vec<u8>]) -> Result<Self> {
        ensure_media_foundation()?;

        let subtype = match codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
        };
        let mft = activate_decoder(codec, &subtype)?;

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
                    unsafe { api.SetValue(&CODECAPI_AVLowLatencyMode, &low_latency_variant()) }
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
        set_input_type(&mft, &subtype, frame_size)?;
        let neg = negotiate_output(&mft, known_display, known_full_range)?;
        unsafe {
            // Best effort: an MFT that ignores these still decodes correctly.
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        log_negotiation(codec, &neg, low_latency_on);

        Ok(Decoder {
            mft,
            params: params.to_vec(),
            codec,
            known_display,
            known_full_range,
            low_latency_on,
            neg,
            queue: VecDeque::new(),
        })
    }

    /// Decodes one frame. Finished frames are queued for `drain`.
    pub fn decode(&mut self, data: &[u8], pts_micros: u64) -> Result<()> {
        let bitstream = annex_b(&self.params, data);
        let sample = make_input_sample(&bitstream, pts_micros)?;
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
                Drain::Frame(sample) => {
                    if let Some(frame) = read_output_sample(&sample, &self.neg)? {
                        self.queue.push_back(frame);
                    }
                }
                Drain::NeedMoreInput => break,
                Drain::StreamChanged => {
                    let previous = (self.neg.full_range, self.neg.range_source);
                    self.neg =
                        negotiate_output(&self.mft, self.known_display, self.known_full_range)?;
                    // Reprint only if the range decision changed
                    if (self.neg.full_range, self.neg.range_source) != previous {
                        log_negotiation(self.codec, &self.neg, self.low_latency_on);
                    }
                }
            }
        }
        Ok(())
    }

    fn process_output_once(&self) -> Result<Drain> {
        let own_sample = if self.neg.provides_samples {
            None
        } else {
            Some(make_output_sample(self.neg.output_size)?)
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
        // Reclaim ownership so whatever ended up in the buffer drops
        // (releases) correctly regardless of which branch below is taken.
        let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
        let _events = unsafe { ManuallyDrop::take(&mut buffer.pEvents) };

        match outcome {
            Ok(()) => Ok(Drain::Frame(
                sample.context("ProcessOutput succeeded without producing a sample")?,
            )),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Drain::NeedMoreInput),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Drain::StreamChanged),
            Err(e) => bail!("ProcessOutput failed: {e}"),
        }
    }
}

fn low_latency_variant() -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        // VARIANT.Anonymous.Anonymous is a ManuallyDrop reached through a
        // union field; an explicit `*` is required so the compiler doesn't
        // try to drop-and-replace it on assignment.
        (*v.Anonymous.Anonymous).vt = VT_BOOL;
        (*v.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_TRUE;
    }
    v
}

/// The startup line describing the negotiated output. Shared between
/// `Decoder::new` and the reprint after a stream change that changes the
/// range decision.
fn log_negotiation(codec: Codec, neg: &Negotiation, low_latency_on: bool) {
    println!(
        "decoder: {} via Media Foundation, {} output, {} range from {}, low latency {}",
        codec.name(),
        neg.chroma_layout.label(),
        if neg.full_range { "full" } else { "limited" },
        neg.range_source.label(),
        if low_latency_on { "on" } else { "off" },
    );
}

/// Enumerates a synchronous decoder MFT for `subtype`, falling back to the
/// well-known CLSID (as some Wine configurations need) before giving up.
fn activate_decoder(codec: Codec, subtype: &GUID) -> Result<IMFTransform> {
    if let Some(mft) = enumerate_and_activate(subtype)? {
        return Ok(mft);
    }
    let clsid = match codec {
        Codec::H264 => &CLSID_MSH264DecoderMFT,
        Codec::Hevc => &CLSID_MSH265DecoderMFT,
    };
    let fallback: windows::core::Result<IMFTransform> =
        unsafe { CoCreateInstance(clsid, None, CLSCTX_INPROC_SERVER) };
    fallback.map_err(|e| match codec {
        Codec::Hevc => anyhow!(
            "this PC has no HEVC decoder. Ask the sender to add --codec h264, or install HEVC \
             Video Extensions from the Microsoft Store"
        ),
        Codec::H264 => anyhow!("this PC has no H.264 decoder ({e})"),
    })
}

fn enumerate_and_activate(subtype: &GUID) -> Result<Option<IMFTransform>> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    let result = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input_info),
            None,
            &mut activates,
            &mut count,
        )
    };
    if result.is_err() || count == 0 {
        if !activates.is_null() {
            unsafe { CoTaskMemFree(Some(activates as *const core::ffi::c_void)) };
        }
        return Ok(None);
    }
    // MFTEnumEx transfers ownership of each activate to us; reading it out of
    // the array does not add a reference, so no extra Release is needed.
    let first = unsafe { std::ptr::read(activates) };
    for i in 1..count as usize {
        let _ = unsafe { std::ptr::read(activates.add(i)) };
    }
    unsafe { CoTaskMemFree(Some(activates as *const core::ffi::c_void)) };

    let Some(activate) = first else {
        return Ok(None);
    };
    let mft =
        unsafe { activate.ActivateObject::<IMFTransform>() }.context("ActivateObject failed")?;
    Ok(Some(mft))
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
        input_type.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(frame_size.0) << 32) | u64::from(frame_size.1),
        )?;
        mft.SetInputType(0, &input_type, 0)?;
    }
    Ok(())
}

/// A guess at the coded picture size to declare before any bitstream has
/// been seen. Falls back to a generic size that real Windows would still
/// correct via a stream change; see `set_input_type`.
const FALLBACK_FRAME_SIZE: (u32, u32) = (1920, 1080);

/// What an SPS says about a picture: the coded (macroblock-aligned) size to
/// declare on `SetInputType`, the display aperture within it as
/// (x, y, width, height), and the VUI's `video_full_range_flag`, if the SPS
/// carries a VUI at all.
struct SpsGeometry {
    coded: (u32, u32),
    display: (u32, u32, u32, u32),
    /// `None` when the VUI is absent, or parsing it gives up early. See
    /// `read_vui_full_range` and `hevc_vui_full_range`.
    full_range: Option<bool>,
}

/// Parses the picture geometry, and a `video_full_range_flag` hint,
/// straight out of the SPS, the way VideoToolbox derives geometry
/// internally when building a format description from parameter sets.
/// `params` is in the order the sender writes them: [SPS, PPS] for H.264,
/// [VPS, SPS, PPS] for HEVC.
///
/// This is needed for more than an optimisation: on real Windows a wrong
/// initial guess is corrected once the decoder signals
/// `MF_E_TRANSFORM_STREAM_CHANGE`, but at least one Wine decoder backend
/// only populates `MF_MT_MINIMUM_DISPLAY_APERTURE` on the type that follows
/// that signal, and only signals it at all when its own idea of the coded
/// size differs from what was declared. Handing it the right coded size up
/// front, as this does, means that signal never fires and the aperture is
/// never populated, so the display size the sender actually cropped to
/// would otherwise be unrecoverable under that backend.
fn sps_geometry(codec: Codec, params: &[Vec<u8>]) -> Option<SpsGeometry> {
    match codec {
        Codec::H264 => h264_geometry(params.first()?),
        Codec::Hevc => hevc_geometry(params.get(1)?),
    }
}

/// Removes H.264/HEVC emulation-prevention bytes (the `03` in any `00 00 03`
/// run) so the result is the raw RBSP a bit reader can walk directly.
fn strip_emulation_prevention(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0u32;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// A big-endian, MSB-first bit reader over an RBSP, with the Exp-Golomb
/// codes H.264 and HEVC parameter sets are built from.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.pos / 8;
        let shift = 7 - (self.pos % 8);
        let b = *self.data.get(byte)?;
        self.pos += 1;
        Some((u32::from(b) >> shift) & 1)
    }

    fn skip(&mut self, n: u32) -> Option<()> {
        for _ in 0..n {
            self.bit()?;
        }
        Some(())
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// `ue(v)`: Exp-Golomb unsigned. Also the bit pattern `se(v)` uses, so
    /// this doubles as a "consume one Exp-Golomb code" skip for fields whose
    /// signed value this parser never needs.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            // A real SPS never runs this long; treat it as malformed.
            if zeros > 32 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let suffix = self.bits(zeros)?;
        Some((1u32 << zeros) - 1 + suffix)
    }
}

/// H.264 profiles whose SPS carries `chroma_format_idc` and the scaling-list
/// flags, per the spec's `if (profile_idc == ...)` list in `seq_parameter_set_data`.
const H264_PROFILES_WITH_CHROMA_INFO: &[u32] =
    &[100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Parses an H.264 SPS (the raw NAL, starting with its 1-byte NAL header)
/// far enough to recover the coded picture size, the frame-cropping
/// rectangle within it, and (see `read_vui_full_range`) the VUI's
/// `video_full_range_flag`. Gives up on the geometry (returning `None`)
/// only for a custom scaling matrix, which this never needs to interpret
/// and VideoToolbox does not emit anyway. A VUI that is absent, or that a
/// short read gives up on partway through, only loses `full_range`,
/// leaving the geometry already parsed by that point intact.
fn h264_geometry(sps: &[u8]) -> Option<SpsGeometry> {
    let rbsp = strip_emulation_prevention(sps);
    let mut r = BitReader::new(&rbsp);
    r.bits(8)?; // NAL header
    let profile_idc = r.bits(8)?;
    r.bits(8)?; // constraint flags + reserved
    r.bits(8)?; // level_idc
    r.ue()?; // seq_parameter_set_id
             // When absent, 4:2:0 is what the spec requires callers to infer, and the
             // only format VideoToolbox ever emits.
    let mut chroma_format_idc = 1;
    if H264_PROFILES_WITH_CHROMA_INFO.contains(&profile_idc) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.skip(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bits(1)? != 0 {
            return None; // seq_scaling_matrix_present_flag
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = r.ue()?;
    if pic_order_cnt_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        r.skip(1)?; // delta_pic_order_always_zero_flag
        r.ue()?; // offset_for_non_ref_pic, se(v) but bit-identical to ue(v)
        r.ue()?; // offset_for_top_to_bottom_field
        let cycle = r.ue()?;
        for _ in 0..cycle {
            r.ue()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_in_mbs = r.ue()? + 1;
    let height_in_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bits(1)?;
    let height_scale = if frame_mbs_only != 0 { 1 } else { 2 };
    let coded_width = width_in_mbs * 16;
    let coded_height = height_in_map_units * height_scale * 16;

    if frame_mbs_only == 0 {
        r.skip(1)?; // mb_adaptive_frame_field_flag
    }
    r.skip(1)?; // direct_8x8_inference_flag
    let (mut left, mut right, mut top, mut bottom) = (0u32, 0u32, 0u32, 0u32);
    if r.bits(1)? != 0 {
        // frame_cropping_flag
        left = r.ue()?;
        right = r.ue()?;
        top = r.ue()?;
        bottom = r.ue()?;
    }
    // Table 6-1's crop units: monochrome or separately-coded planes crop in
    // full samples; every chroma format VideoToolbox actually emits (4:2:0)
    // crops in chroma-sample pairs.
    let (crop_unit_x, crop_unit_y) = if chroma_format_idc == 0 {
        (1, height_scale)
    } else {
        (2, 2 * height_scale)
    };
    let display_x = left * crop_unit_x;
    let display_y = top * crop_unit_y;
    let display_width = coded_width.saturating_sub((left + right) * crop_unit_x);
    let display_height = coded_height.saturating_sub((top + bottom) * crop_unit_y);
    // A VUI that turns out absent, or a short read partway through it, only
    // loses this: it never runs `?` back into the geometry above
    let full_range = read_vui_full_range(&mut r);

    Some(SpsGeometry {
        coded: (coded_width, coded_height),
        display: (display_x, display_y, display_width, display_height),
        full_range,
    })
}

/// The VUI prefix H.264 and HEVC agree on exactly: reads
/// `vui_parameters_present_flag` and, when set, walks past the aspect ratio
/// and overscan info to `video_full_range_flag`. `None` when the VUI or its
/// video signal type turns out absent, or a short read gives up first,
/// either of which the caller treats as unknown range.
fn read_vui_full_range(r: &mut BitReader) -> Option<bool> {
    if r.bits(1)? == 0 {
        return None; // vui_parameters_present_flag
    }
    if r.bits(1)? != 0 {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            // Extended_SAR
            r.bits(16)?; // sar_width
            r.bits(16)?; // sar_height
        }
    }
    if r.bits(1)? != 0 {
        r.skip(1)?; // overscan_appropriate_flag
    }
    if r.bits(1)? == 0 {
        return None; // video_signal_type_present_flag
    }
    r.skip(3)?; // video_format
    Some(r.bits(1)? != 0) // video_full_range_flag
}

/// Parses an HEVC SPS far enough to recover
/// `pic_width_in_luma_samples`/`pic_height_in_luma_samples`, the
/// conformance window within them, and (see `hevc_vui_full_range`) the
/// VUI's `video_full_range_flag`. `sps_max_sub_layers_minus1` can be
/// nonzero: VideoToolbox's HEVC encoder declares more than one temporal
/// sub-layer even when it only encodes a single one, so
/// `skip_profile_tier_level` walks however many the SPS declares instead of
/// assuming there is exactly one. Gives up on the geometry (returning
/// `None`) only for a custom scaling list, which this never needs to
/// interpret and VideoToolbox does not emit anyway. A VUI that is absent,
/// or that a short read gives up on partway through, only loses
/// `full_range`, leaving the geometry already parsed by that point intact.
fn hevc_geometry(sps: &[u8]) -> Option<SpsGeometry> {
    let rbsp = strip_emulation_prevention(sps);
    let mut r = BitReader::new(&rbsp);
    r.bits(16)?; // NAL header (2 bytes for HEVC)
    r.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)?;
    r.skip(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;
    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()?;
    let separate_colour_plane = if chroma_format_idc == 3 {
        r.bits(1)?
    } else {
        0
    };
    let coded_width = r.ue()?;
    let coded_height = r.ue()?;

    let (mut left, mut right, mut top, mut bottom) = (0u32, 0u32, 0u32, 0u32);
    if r.bits(1)? != 0 {
        // conformance_window_flag
        left = r.ue()?;
        right = r.ue()?;
        top = r.ue()?;
        bottom = r.ue()?;
    }
    // Table 6-1 again: HEVC's ConformanceWindow crops in chroma-sample units
    // for 4:2:0/4:2:2, in luma samples for 4:4:4 or separately-coded planes.
    let (crop_unit_x, crop_unit_y) = match chroma_format_idc {
        _ if separate_colour_plane != 0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let display_x = left * crop_unit_x;
    let display_y = top * crop_unit_y;
    let display_width = coded_width.saturating_sub((left + right) * crop_unit_x);
    let display_height = coded_height.saturating_sub((top + bottom) * crop_unit_y);
    // Everything from here on only feeds the range hint: a give-up partway
    // through (a scaling list, a short read) loses that and nothing else
    let full_range = hevc_vui_full_range(&mut r, max_sub_layers_minus1);

    Some(SpsGeometry {
        coded: (coded_width, coded_height),
        display: (display_x, display_y, display_width, display_height),
        full_range,
    })
}

/// Skips `profile_tier_level(1, max_sub_layers_minus1)`, H.265 7.3.3: a
/// fixed 88-bit profile block, then an 8-bit level. Beyond that, for
/// however many extra temporal sub-layers the SPS declares, this reads
/// that sub-layer's own presence flags, then the reserved padding up to 8
/// sub-layers, then each present sub-layer's own profile or level block.
/// VideoToolbox's HEVC encoder declares a second sub-layer it never uses,
/// so this walks however many the SPS actually declares instead of
/// assuming there is only one.
fn skip_profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> Option<()> {
    r.skip(88)?; // profile space/tier/idc, compatibility flags, constraint flags
    r.skip(8)?; // general_level_idc
    let sub_layers = max_sub_layers_minus1 as usize;
    // (sub_layer_profile_present_flag[i], sub_layer_level_present_flag[i])
    let mut presence = [(false, false); 8];
    for entry in presence.iter_mut().take(sub_layers) {
        *entry = (r.bits(1)? != 0, r.bits(1)? != 0);
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?; // reserved_zero_2bits[i]
        }
    }
    for &(profile_present, level_present) in presence.iter().take(sub_layers) {
        if profile_present {
            r.skip(88)?;
        }
        if level_present {
            r.skip(8)?;
        }
    }
    Some(())
}

/// One `st_ref_pic_set(stRpsIdx)`, H.265 7.3.7, returning
/// `NumDeltaPocs[stRpsIdx]` for later sets to predict from. `previous` holds
/// `NumDeltaPocs[0..stRpsIdx]`. In an SPS (unlike a slice header) an
/// inter-predicted set always predicts from `stRpsIdx - 1`, so
/// `delta_idx_minus1` is never present.
fn read_short_term_ref_pic_set(r: &mut BitReader, idx: u32, previous: &[u32]) -> Option<u32> {
    let inter_predicted = idx != 0 && r.bits(1)? != 0;
    if inter_predicted {
        r.skip(1)?; // delta_rps_sign
        r.ue()?; // abs_delta_rps_minus1
        let num_delta_pocs_ref = *previous.last()?;
        let mut num_delta_pocs = 0u32;
        for _ in 0..=num_delta_pocs_ref {
            let used = r.bits(1)?; // used_by_curr_pic_flag
            let use_delta = if used == 0 { r.bits(1)? } else { 1 }; // use_delta_flag
            if used != 0 || use_delta != 0 {
                num_delta_pocs += 1;
            }
        }
        Some(num_delta_pocs)
    } else {
        let num_negative_pics = r.ue()?;
        let num_positive_pics = r.ue()?;
        for _ in 0..num_negative_pics {
            r.ue()?; // delta_poc_s0_minus1
            r.skip(1)?; // used_by_curr_pic_s0_flag
        }
        for _ in 0..num_positive_pics {
            r.ue()?; // delta_poc_s1_minus1
            r.skip(1)?; // used_by_curr_pic_s1_flag
        }
        Some(num_negative_pics + num_positive_pics)
    }
}

/// Everything HEVC's SPS carries between the conformance window and its
/// `vui_parameters_present_flag` (H.265 7.3.2.2), then the same VUI prefix
/// H.264 has. `None` means an absent VUI or video signal type, a scaling
/// list VideoToolbox never emits anyway (so this does not decode one), or a
/// short read: the caller treats all of them as unknown range.
fn hevc_vui_full_range(r: &mut BitReader, max_sub_layers_minus1: u32) -> Option<bool> {
    r.ue()?; // bit_depth_luma_minus8
    r.ue()?; // bit_depth_chroma_minus8
    let log2_max_poc_lsb_minus4 = r.ue()?;
    let ordering_info_present = r.bits(1)?; // sps_sub_layer_ordering_info_present_flag
                                            // (present ? 0 : max_sub_layers_minus1) through max_sub_layers_minus1:
                                            // one iteration per declared sub-layer when present, otherwise exactly
                                            // one regardless of how many there are
    let iterations = if ordering_info_present != 0 {
        max_sub_layers_minus1 + 1
    } else {
        1
    };
    for _ in 0..iterations {
        r.ue()?; // sps_max_dec_pic_buffering_minus1[i]
        r.ue()?; // sps_max_num_reorder_pics[i]
        r.ue()?; // sps_max_latency_increase_plus1[i]
    }
    r.ue()?; // log2_min_luma_coding_block_size_minus3
    r.ue()?; // log2_diff_max_min_luma_coding_block_size
    r.ue()?; // log2_min_luma_transform_block_size_minus2
    r.ue()?; // log2_diff_max_min_luma_transform_block_size
    r.ue()?; // max_transform_hierarchy_depth_inter
    r.ue()?; // max_transform_hierarchy_depth_intra
    if r.bits(1)? != 0 {
        // scaling_list_enabled_flag
        if r.bits(1)? != 0 {
            return None; // sps_scaling_list_data_present_flag: not emitted by VideoToolbox
        }
    }
    r.skip(1)?; // amp_enabled_flag
    r.skip(1)?; // sample_adaptive_offset_enabled_flag
    if r.bits(1)? != 0 {
        // pcm_enabled_flag
        r.skip(4)?; // pcm_sample_bit_depth_luma_minus1
        r.skip(4)?; // pcm_sample_bit_depth_chroma_minus1
        r.ue()?; // log2_min_pcm_luma_coding_block_size_minus3
        r.ue()?; // log2_diff_max_min_pcm_luma_coding_block_size
        r.skip(1)?; // pcm_loop_filter_disabled_flag
    }

    let num_short_term_ref_pic_sets = r.ue()?;
    let mut num_delta_pocs = Vec::new();
    for i in 0..num_short_term_ref_pic_sets {
        let n = read_short_term_ref_pic_set(r, i, &num_delta_pocs)?;
        num_delta_pocs.push(n);
    }

    if r.bits(1)? != 0 {
        // long_term_ref_pics_present_flag
        let num_long_term_ref_pics_sps = r.ue()?;
        let lt_poc_lsb_bits = log2_max_poc_lsb_minus4 + 4;
        for _ in 0..num_long_term_ref_pics_sps {
            r.skip(lt_poc_lsb_bits)?; // lt_ref_pic_poc_lsb_sps[i]
            r.skip(1)?; // used_by_curr_pic_lt_sps_flag[i]
        }
    }
    r.skip(1)?; // sps_temporal_mvp_enabled_flag
    r.skip(1)?; // strong_intra_smoothing_enabled_flag

    read_vui_full_range(r)
}

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

fn make_input_sample(bitstream: &[u8], pts_micros: u64) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(bitstream.len() as u32) }
        .context("MFCreateMemoryBuffer failed")?;
    unsafe {
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(bitstream.as_ptr(), ptr, bitstream.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bitstream.len() as u32)?;
    }
    let sample = unsafe { MFCreateSample() }.context("MFCreateSample failed")?;
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(micros_to_100ns(pts_micros))?;
    }
    Ok(sample)
}

fn make_output_sample(size: u32) -> Result<IMFSample> {
    let buffer =
        unsafe { MFCreateMemoryBuffer(size.max(1)) }.context("MFCreateMemoryBuffer failed")?;
    let sample = unsafe { MFCreateSample() }.context("MFCreateSample failed")?;
    unsafe { sample.AddBuffer(&buffer) }.context("AddBuffer failed")?;
    Ok(sample)
}

/// Reads one decoded output sample, cropping to the display aperture and
/// returning tightly packed luma and NV12-order chroma. `None` means the
/// buffer could not be addressed, which callers treat as no frame this round.
fn read_output_sample(sample: &IMFSample, neg: &Negotiation) -> Result<Option<DecodedFrame>> {
    let sample_time = unsafe { sample.GetSampleTime() }.unwrap_or(0);
    let buffer = unsafe { sample.GetBufferByIndex(0) }.context("GetBufferByIndex failed")?;

    let buffer2d: Option<IMF2DBuffer> = buffer.cast().ok();
    let (ptr, pitch): (*const u8, i32) = if let Some(buffer2d) = &buffer2d {
        let mut scanline = std::ptr::null_mut();
        let mut lock_pitch = 0i32;
        unsafe { buffer2d.Lock2D(&mut scanline, &mut lock_pitch) }.context("Lock2D failed")?;
        (scanline, lock_pitch)
    } else {
        let mut linear = std::ptr::null_mut();
        unsafe { buffer.Lock(&mut linear, None, None) }.context("Lock failed")?;
        (linear, neg.fallback_stride)
    };

    if ptr.is_null() || pitch <= 0 || neg.display_width == 0 || neg.display_height == 0 {
        if let Some(buffer2d) = &buffer2d {
            unsafe { buffer2d.Unlock2D().ok() };
        } else {
            unsafe { buffer.Unlock().ok() };
        }
        return Ok(None);
    }
    let pitch = pitch as usize;

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
        pts_micros: (sample_time.max(0) as u64) / 10,
    }))
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

/// Presentation timestamps travel in microseconds on the wire; Media
/// Foundation counts in 100 ns units.
fn micros_to_100ns(micros: u64) -> i64 {
    (micros as i64).saturating_mul(10)
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

    #[test]
    fn micros_to_100ns_scales_by_ten() {
        assert_eq!(micros_to_100ns(0), 0);
        assert_eq!(micros_to_100ns(1), 10);
        // 1/30s in whole microseconds, matching a 30 fps test stream.
        assert_eq!(micros_to_100ns(33_333), 333_330);
    }

    /// Decodes a hex string into bytes, for the SPS/VPS/PPS fixtures below.
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn h264_geometry_reads_size_and_full_range_from_a_real_sps() {
        // The real SPS VideoToolbox produced for the 640x360 self-test
        // pattern. Its PPS is not needed for geometry or range.
        let sps = hex("27640020ac1314100280bfe59b81010103c2010842");
        let geometry = h264_geometry(&sps).expect("a real SPS should parse");
        assert_eq!(geometry.coded, (640, 368));
        assert_eq!(geometry.display, (0, 0, 640, 360));
        assert_eq!(geometry.full_range, Some(true));
    }

    #[test]
    fn hevc_geometry_reads_size_and_full_range_from_a_real_sps() {
        // The real VPS/SPS/PPS VideoToolbox produced for the same pattern.
        // ffprobe reports coded height 368 for this stream: the
        // conformance window only crops 4 rows off 368, not 360, and
        // sps_max_sub_layers_minus1 is 1, not 0, in this real capture.
        let vps = hex("40010c03ffff016000000300b0000003000003007b0000043024");
        let sps = hex("420103016000000300b0000003000003007b0000a005020171f2e2010ee452082e7e13d0bea1bd50feaa08f55413eaaa0af555417eaaaa0cf555541beaaaaa0ef5555541feaaaaaa043d55555529b81010101fc20104");
        let pps = hex("4401c072f05b24");
        let geometry =
            sps_geometry(Codec::Hevc, &[vps, sps, pps]).expect("a real SPS should parse");
        assert_eq!(geometry.coded, (640, 368));
        assert_eq!(geometry.display, (0, 0, 640, 360));
        assert_eq!(geometry.full_range, Some(true));
    }
}
