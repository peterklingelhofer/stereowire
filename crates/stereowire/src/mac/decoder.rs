//! Headless HEVC decoding via VideoToolbox.
//!
//! The live receiver hands frames to `AVSampleBufferDisplayLayer`, which
//! decodes and draws in one step but never gives the pixels back. This decoder
//! exists so the self-test can check what actually came out the far end.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc::{channel, Receiver, Sender};

use anyhow::{bail, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMFormatDescription, CMTime};
use objc2_core_video::{CVImageBuffer, CVPixelBufferLockFlags};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession,
};

use super::sample;

/// A decoded frame, reduced to its luma plane. Luma alone is enough to judge
/// whether the picture survived the round trip.
pub struct DecodedFrame {
    pub luma: Vec<u8>,
    pub width: usize,
    pub height: usize,
    #[allow(dead_code, reason = "carried for callers that reorder by timestamp")]
    pub pts_micros: u64,
}

pub struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    frames: Receiver<DecodedFrame>,
    format: CFRetained<CMFormatDescription>,
    sink: *mut Sender<DecodedFrame>,
}

impl Decoder {
    pub fn new(params: &[Vec<u8>]) -> Result<Self> {
        let format = sample::format_from_params(params)?;
        let (tx, rx) = channel();
        let sink = Box::into_raw(Box::new(tx));

        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_decoded),
            decompressionOutputRefCon: sink as *mut c_void,
        };
        let mut session: *mut VTDecompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                None,
                &record,
                NonNull::from(&mut session),
            )
        };
        if status != 0 || session.is_null() {
            drop(unsafe { Box::from_raw(sink) });
            bail!("VTDecompressionSessionCreate failed with status {status}");
        }
        let session = unsafe { CFRetained::from_raw(NonNull::new(session).expect("checked")) };
        Ok(Decoder {
            session,
            frames: rx,
            format,
            sink,
        })
    }

    /// Decodes one frame, blocking until the callback has run.
    pub fn decode(&self, data: &[u8], pts_micros: u64) -> Result<()> {
        let sample = sample::sample_from_frame(data, &self.format, pts_micros)?;
        let mut info = VTDecodeInfoFlags::empty();
        let status = unsafe {
            self.session.decode_frame(
                &sample,
                // Synchronous keeps the self-test deterministic.
                VTDecodeFrameFlags::empty(),
                std::ptr::null_mut(),
                &mut info,
            )
        };
        if status != 0 {
            bail!("VTDecompressionSessionDecodeFrame failed with status {status}");
        }
        Ok(())
    }

    pub fn drain(&self) -> impl Iterator<Item = DecodedFrame> + '_ {
        self.frames.try_iter()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
        drop(unsafe { Box::from_raw(self.sink) });
    }
}

unsafe extern "C-unwind" fn on_decoded(
    refcon: *mut c_void,
    _source_frame: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    pts: CMTime,
    _duration: CMTime,
) {
    if status != 0 || image.is_null() || refcon.is_null() {
        return;
    }
    let image = unsafe { &*image };
    let sink = unsafe { &*(refcon as *const Sender<DecodedFrame>) };

    let width = objc2_core_video::CVPixelBufferGetWidth(image);
    let height = objc2_core_video::CVPixelBufferGetHeight(image);
    if width == 0 || height == 0 {
        return;
    }

    // Read-only lock: we only copy the luma plane out.
    let flags = CVPixelBufferLockFlags::ReadOnly;
    if unsafe { objc2_core_video::CVPixelBufferLockBaseAddress(image, flags) } != 0 {
        return;
    }
    let base = objc2_core_video::CVPixelBufferGetBaseAddressOfPlane(image, 0);
    let stride = objc2_core_video::CVPixelBufferGetBytesPerRowOfPlane(image, 0);

    let luma = if base.is_null() || stride < width {
        Vec::new()
    } else {
        // Copy row by row: the plane is padded to the stride.
        let mut luma = Vec::with_capacity(width * height);
        for row in 0..height {
            let start = unsafe { (base as *const u8).add(row * stride) };
            luma.extend_from_slice(unsafe { std::slice::from_raw_parts(start, width) });
        }
        luma
    };
    unsafe { objc2_core_video::CVPixelBufferUnlockBaseAddress(image, flags) };

    if luma.is_empty() {
        return;
    }
    let pts_micros = if pts.timescale > 0 {
        (pts.value.max(0) as i128 * sample::TIMESCALE as i128 / pts.timescale as i128) as u64
    } else {
        0
    };
    let _ = sink.send(DecodedFrame {
        luma,
        width,
        height,
        pts_micros,
    });
}
