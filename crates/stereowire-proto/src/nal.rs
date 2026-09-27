//! Reshaping H.264 and HEVC bitstreams between the forms encoders and
//! decoders take, VideoToolbox's 4-byte length prefixes and Annex B start
//! codes, and reading picture geometry and range from parameter sets.
//!
//! It is pure Rust, so it runs and is tested on every platform, including
//! WebAssembly later.

use crate::packet::Codec;

/// Converts VideoToolbox's length-prefixed NAL units to Annex B.
///
/// VideoToolbox emits NAL units with a 4-byte big-endian length prefix. This
/// returns the parameter sets, then every NAL unit of `data`, each preceded by
/// the start code `00 00 00 01`, which is the form Windows Media Foundation
/// and browsers' WebCodecs expect. Stops at the first malformed length (one
/// that runs past the end) and returns what parsed so far.
pub fn annex_b(params: &[Vec<u8>], data: &[u8]) -> Vec<u8> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::with_capacity(data.len() + data.len() / 8 + 32);
    for set in params {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(set);
    }
    let mut rest = data;
    while rest.len() >= 4 {
        let (len_bytes, tail) = rest.split_at(4);
        let len = u32::from_be_bytes(len_bytes.try_into().expect("checked 4 bytes")) as usize;
        if len > tail.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&tail[..len]);
        rest = &tail[len..];
    }
    out
}

/// Splits an Annex B byte stream into NAL units, start codes removed.
///
/// Each unit runs to the next `00 00 01`. Trailing zero bytes are trimmed,
/// which removes the extra leading zero of a 4-byte start code and any
/// `trailing_zero_8bits`, since a NAL unit never ends in a zero byte.
pub fn split_annex_b(stream: &[u8]) -> Vec<&[u8]> {
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
pub enum NalKind {
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

/// Classifies a NAL unit, given without its start code or length prefix.
pub fn classify(codec: Codec, unit: &[u8]) -> NalKind {
    let Some(&header) = unit.first() else {
        return NalKind::Other;
    };
    match codec {
        Codec::H264 => match header & 0x1F {
            5 => NalKind::Keyframe,
            1..=4 => NalKind::Slice,
            6 => NalKind::Sei,
            7 => NalKind::Sps,
            8 => NalKind::Pps,
            _ => NalKind::Other,
        },
        Codec::Hevc => match (header >> 1) & 0x3F {
            16..=23 => NalKind::Keyframe,
            0..=31 => NalKind::Slice,
            32 => NalKind::Vps,
            33 => NalKind::Sps,
            34 => NalKind::Pps,
            39 | 40 => NalKind::Sei,
            _ => NalKind::Other,
        },
    }
}

/// The latest parameter sets seen.
#[derive(Default)]
pub struct ParamSets {
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// Keeps `unit` when `kind` is a parameter set, replacing the earlier
    /// one of that kind.
    pub fn absorb(&mut self, kind: NalKind, unit: &[u8]) {
        let slot = match kind {
            NalKind::Vps => &mut self.vps,
            NalKind::Sps => &mut self.sps,
            NalKind::Pps => &mut self.pps,
            _ => return,
        };
        *slot = Some(unit.to_vec());
    }

    /// Whether every parameter set `codec` needs has been seen.
    pub fn complete(&self, codec: Codec) -> bool {
        self.sps.is_some() && self.pps.is_some() && (codec == Codec::H264 || self.vps.is_some())
    }

    /// SPS then PPS for H.264, with the VPS first for HEVC: the order the
    /// wire carries them in. Empty until every one has been seen.
    pub fn list(&self, codec: Codec) -> Vec<Vec<u8>> {
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
pub struct AccessUnit {
    /// Slice and SEI NAL units, each behind a 4-byte big-endian length.
    pub data: Vec<u8>,
    /// Whether any slice is present. Without one the sample is no frame.
    pub has_slices: bool,
    pub keyframe: bool,
}

/// Sorts an Annex B access unit: parameter sets go into `params`, slices and
/// SEI into `data` with their lengths in front, and the rest is dropped.
pub fn parse_access_unit(codec: Codec, stream: &[u8], params: &mut ParamSets) -> AccessUnit {
    let mut unit = AccessUnit {
        data: Vec::with_capacity(stream.len() + 16),
        has_slices: false,
        keyframe: false,
    };
    for nal in split_annex_b(stream) {
        match classify(codec, nal) {
            kind @ (NalKind::Vps | NalKind::Sps | NalKind::Pps) => params.absorb(kind, nal),
            kind @ (NalKind::Keyframe | NalKind::Slice | NalKind::Sei) => {
                unit.data
                    .extend_from_slice(&(nal.len() as u32).to_be_bytes());
                unit.data.extend_from_slice(nal);
                unit.has_slices |= kind != NalKind::Sei;
                unit.keyframe |= kind == NalKind::Keyframe;
            }
            NalKind::Other => {}
        }
    }
    unit
}

/// What an SPS says about a picture: the coded (macroblock-aligned) size to
/// declare on `SetInputType`, the display aperture within it as
/// (x, y, width, height), and the VUI's `video_full_range_flag`, if the SPS
/// carries a VUI at all.
pub struct SpsGeometry {
    pub coded: (u32, u32),
    pub display: (u32, u32, u32, u32),
    /// `None` when the VUI is absent, or parsing it gives up early. See
    /// `read_vui_full_range` and `hevc_vui_full_range`.
    pub full_range: Option<bool>,
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
pub fn sps_geometry(codec: Codec, params: &[Vec<u8>]) -> Option<SpsGeometry> {
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
            if zeros >= 32 {
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
    // Every size and offset here comes off the network, so the arithmetic
    // saturates
    let coded_width = width_in_mbs.saturating_mul(16);
    let coded_height = height_in_map_units.saturating_mul(height_scale * 16);

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
    let display_x = left.saturating_mul(crop_unit_x);
    let display_y = top.saturating_mul(crop_unit_y);
    let display_width =
        coded_width.saturating_sub(left.saturating_add(right).saturating_mul(crop_unit_x));
    let display_height =
        coded_height.saturating_sub(top.saturating_add(bottom).saturating_mul(crop_unit_y));
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
    let display_x = left.saturating_mul(crop_unit_x);
    let display_y = top.saturating_mul(crop_unit_y);
    let display_width =
        coded_width.saturating_sub(left.saturating_add(right).saturating_mul(crop_unit_x));
    let display_height =
        coded_height.saturating_sub(top.saturating_add(bottom).saturating_mul(crop_unit_y));
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
        let lt_poc_lsb_bits = log2_max_poc_lsb_minus4.saturating_add(4);
        for _ in 0..num_long_term_ref_pics_sps {
            r.skip(lt_poc_lsb_bits)?; // lt_ref_pic_poc_lsb_sps[i]
            r.skip(1)?; // used_by_curr_pic_lt_sps_flag[i]
        }
    }
    r.skip(1)?; // sps_temporal_mvp_enabled_flag
    r.skip(1)?; // strong_intra_smoothing_enabled_flag

    read_vui_full_range(r)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn annex_b_prefixes_params_and_each_nal_unit_with_a_start_code() {
        let params = vec![vec![1u8, 2, 3], vec![4u8, 5]];
        let nal_units = [vec![9u8, 9, 9], vec![7u8, 7]];
        let mut data = Vec::new();
        for nal in &nal_units {
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(nal);
        }

        let out = annex_b(&params, &data);

        let mut expected = Vec::new();
        for set in params.iter().chain(nal_units.iter()) {
            expected.extend_from_slice(&[0, 0, 0, 1]);
            expected.extend_from_slice(set);
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn annex_b_stops_at_a_length_that_runs_past_the_end() {
        let mut data = 100u32.to_be_bytes().to_vec();
        data.extend_from_slice(&[1, 2, 3]);
        // The claimed length is longer than the three bytes that follow it.
        assert!(annex_b(&[], &data).is_empty());
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

    #[test]
    fn ue_refuses_a_prefix_too_long_for_a_u32() {
        // 32 zeros, the marker bit, then a full suffix: decoding it would
        // shift a u32 by 32
        assert_eq!(BitReader::new(&[0, 0, 0, 0, 0x80, 0, 0, 0, 0]).ue(), None);
        // 31 zeros is the longest prefix a u32 holds
        assert_eq!(
            BitReader::new(&[0, 0, 0, 1, 0xff, 0xff, 0xff, 0xfe]).ue(),
            Some(u32::MAX - 1)
        );
    }

    /// `value` as an Exp-Golomb code, written as 0s and 1s for `nal_unit`.
    fn exp_golomb(value: u32) -> String {
        let code = u64::from(value) + 1;
        format!("{}{code:b}", "0".repeat(code.ilog2() as usize))
    }

    /// Packs fields written as 0s and 1s into a NAL unit, adding the
    /// emulation prevention bytes an encoder would, so a test can build a
    /// parameter set field by field.
    fn nal_unit(fields: &[&str]) -> Vec<u8> {
        let bits = fields.concat();
        let mut rbsp = vec![0u8; bits.len().div_ceil(8)];
        for (index, bit) in bits.bytes().enumerate() {
            if bit == b'1' {
                rbsp[index / 8] |= 0x80 >> (index % 8);
            }
        }
        let mut unit = Vec::new();
        for byte in rbsp {
            if unit.ends_with(&[0, 0]) && byte <= 3 {
                unit.push(3);
            }
            unit.push(byte);
        }
        unit
    }

    #[test]
    fn oversized_sps_fields_saturate() {
        let huge = exp_golomb(u32::MAX - 1);
        // The cropping or conformance window flag, set, then four huge offsets
        let window = ["1", &huge, &huge, &huge, &huge].concat();

        let h264 = nal_unit(&[
            "01100111",         // NAL header, type 7
            "01000010",         // profile_idc 66, which has no chroma format fields
            "0000000000011111", // constraint flags and level_idc
            "11111",            // SPS id, frame_num and POC sizes, POC type 0, reference frames
            "0",                // gaps_in_frame_num_value_allowed_flag
            &huge,              // pic_width_in_mbs_minus1
            &huge,              // pic_height_in_map_units_minus1
            "11",               // frame_mbs_only_flag, direct_8x8_inference_flag
            &window,            // frame cropping
            "0",                // vui_parameters_present_flag
        ]);
        let geometry = h264_geometry(&h264).expect("the geometry still parses");
        assert_eq!(geometry.coded, (u32::MAX, u32::MAX));
        assert_eq!(geometry.display, (u32::MAX, u32::MAX, 0, 0));

        let hevc = nal_unit(&[
            "0100001000000001", // NAL header, type 33
            "00000001",         // VPS id, one sub-layer, temporal ID nesting
            &"0".repeat(96),    // profile_tier_level
            "1010",             // SPS id, chroma_format_idc 1
            &exp_golomb(640),   // pic_width_in_luma_samples
            &exp_golomb(368),   // pic_height_in_luma_samples
            &window,            // conformance window
            "11",               // bit depths
            &huge,              // log2_max_pic_order_cnt_lsb_minus4
            "1111",             // ordering info for the one sub-layer
            "111111",           // coding and transform block sizes
            "0000",             // scaling list, AMP, SAO and PCM all off
            "1",                // no short-term reference picture sets
            "1010",             // one long-term picture, its POC LSBs huge + 4 bits long
        ]);
        let geometry = hevc_geometry(&hevc).expect("the geometry still parses");
        assert_eq!(geometry.coded, (640, 368));
        assert_eq!(geometry.display, (u32::MAX, u32::MAX, 0, 0));
        // The long-term picture's POC LSBs run past the end, so the range is unknown
        assert_eq!(geometry.full_range, None);
    }
}
