//! Hardware HEVC/H.264 encoding via VideoToolbox.
//!
//! Tuned for latency rather than bits: B-frames are disabled so no frame ever
//! waits on a later one, and the low-latency rate controller is requested so
//! the encoder does not buffer to smooth its output. On a link with tens of
//! megabits to spare, that is the right trade.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc::{channel, Receiver, Sender};

use anyhow::{bail, Result};
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMSampleBuffer,
};
use objc2_core_video::CVImageBuffer;
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_H264EntropyMode,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTH264EntropyMode_CABAC, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
};
use stereowire_proto::packet::Codec;

use super::cf;
use super::sample::{invalid_time, micros, micros_from};

/// One compressed frame, ready to put on the wire.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_micros: u64,
    /// VPS/SPS/PPS, or SPS/PPS for H.264, carried out-of-band on every frame.
    /// They total around a hundred bytes, and sending them only on keyframes
    /// means a single lost keyframe fragment leaves the receiver unable to
    /// build a decoder at all until the next one, seconds later.
    pub params: Vec<Vec<u8>>,
}

/// Carried through the C callback's refcon: the codec is needed to read back
/// the right kind of parameter sets, alongside the channel to the encoder.
struct Sink {
    codec: Codec,
    tx: Sender<EncodedFrame>,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    frames: Receiver<EncodedFrame>,
    /// Owns the sink the C callback writes through; must outlive the session.
    sink: *mut Sink,
}

// The session and its callback are driven from the capture queue; the channel
// end inside `Sink` is itself Send.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(codec: Codec, width: i32, height: i32, bitrate_bps: i32, fps: i32) -> Result<Self> {
        let (tx, rx) = channel();
        let sink = Box::into_raw(Box::new(Sink { codec, tx }));

        let spec = cf::dict(&[(
            unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl },
            cf::bool_value(true),
        )]);

        // Low-latency rate control needs a hardware encoder. Apple Silicon and
        // T2 Macs have one; some older Intel Macs do not, and a process
        // translated by Rosetta cannot reach the one that is there. Falling back
        // to the default rate controller costs a little latency and is far
        // better than refusing to start on a Mac that could otherwise send.
        let mut session: *mut VTCompressionSession = std::ptr::null_mut();
        let mut status = create_session(codec, width, height, Some(&spec), sink, &mut session);
        if status != 0 || session.is_null() {
            eprintln!(
                "note: no low-latency {} encoder on this Mac (status {status}), \
                 falling back to the default rate controller",
                codec.name()
            );
            session = std::ptr::null_mut();
            status = create_session(codec, width, height, None, sink, &mut session);
        }
        if status != 0 || session.is_null() {
            drop(unsafe { Box::from_raw(sink) });
            bail!("VTCompressionSessionCreate failed with status {status}");
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new(session).expect("checked")) };

        let encoder = Encoder {
            session,
            frames: rx,
            sink,
        };
        encoder.configure(codec, bitrate_bps, fps)?;
        unsafe { encoder.session.prepare_to_encode_frames() };
        Ok(encoder)
    }

    fn set(&self, key: &CFString, value: &CFType) -> Result<()> {
        let status = unsafe { VTSessionSetProperty(cf::as_cf(&*self.session), key, Some(value)) };
        if status != 0 {
            bail!("VTSessionSetProperty failed with status {status}");
        }
        Ok(())
    }

    fn configure(&self, codec: Codec, bitrate_bps: i32, fps: i32) -> Result<()> {
        let real_time = cf::bool_value(true);
        let no_reorder = cf::bool_value(false);
        let bitrate = cf::i32_value(bitrate_bps);
        let frame_rate = cf::f32_value(fps as f32);
        // A keyframe every two seconds bounds how long a receiver that joins or
        // loses sync has to wait for a clean picture.
        let key_interval = cf::i32_value(fps * 2);

        unsafe {
            self.set(kVTCompressionPropertyKey_RealTime, cf::as_cf(&*real_time))?;
            self.set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                cf::as_cf(&*no_reorder),
            )?;
            self.set(
                kVTCompressionPropertyKey_AverageBitRate,
                cf::as_cf(&*bitrate),
            )?;
            self.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                cf::as_cf(&*frame_rate),
            )?;
            self.set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                cf::as_cf(&*key_interval),
            )?;
            match codec {
                Codec::Hevc => {
                    self.set(
                        kVTCompressionPropertyKey_ProfileLevel,
                        cf::as_cf(kVTProfileLevel_HEVC_Main_AutoLevel),
                    )?;
                }
                Codec::H264 => {
                    self.set(
                        kVTCompressionPropertyKey_ProfileLevel,
                        cf::as_cf(kVTProfileLevel_H264_High_AutoLevel),
                    )?;
                    // Windows Media Foundation and browsers decode CABAC just
                    // as well, and it compresses noticeably better than CAVLC.
                    self.set(
                        kVTCompressionPropertyKey_H264EntropyMode,
                        cf::as_cf(kVTH264EntropyMode_CABAC),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Submits a captured frame. Output arrives later via [`Encoder::drain`].
    pub fn encode(
        &self,
        image: &CVImageBuffer,
        pts_micros: u64,
        force_keyframe: bool,
    ) -> Result<()> {
        let properties = force_keyframe.then(|| {
            cf::dict(&[(
                unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame },
                cf::bool_value(true),
            )])
        });
        let mut info = VTEncodeInfoFlags::empty();
        let status = unsafe {
            self.session.encode_frame(
                image,
                micros(pts_micros),
                // Unknown duration; the encoder infers cadence from timestamps.
                invalid_time(),
                properties.as_deref().map(cf::as_cf_dict),
                std::ptr::null_mut(),
                &mut info,
            )
        };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame failed with status {status}");
        }
        Ok(())
    }

    /// Changes the target bitrate on a running session, which VideoToolbox
    /// applies from the next frame.
    pub fn set_bitrate(&self, bitrate_bps: i32) -> Result<()> {
        let value = cf::i32_value(bitrate_bps);
        unsafe { self.set(kVTCompressionPropertyKey_AverageBitRate, cf::as_cf(&*value)) }
    }

    /// Blocks until every submitted frame has been emitted.
    pub fn finish(&self) -> Result<()> {
        // An invalid deadline means "complete everything outstanding".
        let status = unsafe { self.session.complete_frames(invalid_time()) };
        if status != 0 {
            bail!("VTCompressionSessionCompleteFrames failed with status {status}");
        }
        Ok(())
    }

    /// Non-blocking: returns whatever the encoder has finished since last call.
    pub fn drain(&self) -> impl Iterator<Item = EncodedFrame> + '_ {
        self.frames.try_iter()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Invalidate first so the callback cannot fire after the sender is gone.
        unsafe { self.session.invalidate() };
        drop(unsafe { Box::from_raw(self.sink) });
    }
}

/// VideoToolbox output callback. Runs on an encoder-owned thread.
/// Creates the compression session, with or without an encoder specification.
/// Split out so the caller can retry without low-latency rate control on a Mac
/// that cannot provide it.
fn create_session(
    codec: Codec,
    width: i32,
    height: i32,
    spec: Option<&NSDictionary<NSString, AnyObject>>,
    sink: *mut Sink,
    session: &mut *mut VTCompressionSession,
) -> i32 {
    let codec_type = match codec {
        Codec::Hevc => kCMVideoCodecType_HEVC,
        Codec::H264 => kCMVideoCodecType_H264,
    };
    unsafe {
        VTCompressionSession::create(
            None,
            width,
            height,
            codec_type,
            spec.map(cf::as_cf_dict),
            None,
            None,
            Some(on_encoded),
            sink as *mut c_void,
            NonNull::from(session),
        )
    }
}

unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    _source_frame: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    if status != 0 || sample_buffer.is_null() || refcon.is_null() {
        return;
    }
    let sample = unsafe { &*sample_buffer };
    let sink = unsafe { &*(refcon as *const Sink) };

    let Some(frame) = (unsafe { extract(sample, sink.codec) }) else {
        return;
    };
    // A closed receiver just means we are shutting down.
    let _ = sink.tx.send(frame);
}

unsafe fn extract(sample: &CMSampleBuffer, codec: Codec) -> Option<EncodedFrame> {
    let keyframe = unsafe { is_keyframe(sample) };

    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut data = vec![0u8; len];
    // Copies out of the possibly-scattered block buffer in one call.
    let dest = NonNull::new(data.as_mut_ptr() as *mut c_void)?;
    let status = unsafe { block.copy_data_bytes(0, len, dest) };
    if status != 0 {
        return None;
    }

    let pts_micros = micros_from(unsafe { sample.presentation_time_stamp() });

    // Parameter sets live in the format description rather than the bitstream,
    // so they have to be carried alongside the frames themselves.
    let params = unsafe { sample.format_description() }
        .map(|fd| unsafe { params(&fd, codec) })
        .unwrap_or_default();

    Some(EncodedFrame {
        data,
        keyframe,
        pts_micros,
        params,
    })
}

unsafe fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        // No attachments at all means nothing marked it as a dependent frame.
        return true;
    };
    let array = cf::as_ns_array(&attachments);
    if array.is_empty() {
        return true;
    }
    let entry = array.objectAtIndex(0);
    let not_sync = unsafe { entry.objectForKey(cf::key(kCMSampleAttachmentKey_NotSync)) };
    match not_sync {
        // Present and true means this frame depends on others.
        Some(value) => !value
            .downcast_ref::<NSNumber>()
            .is_some_and(|n| n.as_bool()),
        None => true,
    }
}

/// Reads back VPS/SPS/PPS for HEVC, or SPS/PPS for H.264, from a format
/// description VideoToolbox attached to an encoded sample.
unsafe fn params(format: &objc2_core_media::CMFormatDescription, codec: Codec) -> Vec<Vec<u8>> {
    let mut count = 0usize;
    // The first call is only to learn how many parameter sets there are.
    let status = unsafe {
        match codec {
            Codec::Hevc => objc2_core_media::CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                format,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
                std::ptr::null_mut(),
            ),
            Codec::H264 => objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
                std::ptr::null_mut(),
            ),
        }
    };
    if status != 0 {
        return Vec::new();
    }

    let mut sets = Vec::with_capacity(count);
    for index in 0..count {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size = 0usize;
        let status = unsafe {
            match codec {
                Codec::Hevc => {
                    objc2_core_media::CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                        format,
                        index,
                        &mut ptr,
                        &mut size,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                }
                Codec::H264 => {
                    objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        format,
                        index,
                        &mut ptr,
                        &mut size,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                }
            }
        };
        if status != 0 || ptr.is_null() {
            return Vec::new();
        }
        sets.push(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec());
    }
    sets
}
