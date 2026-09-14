//! Rebuilding CoreMedia sample buffers from the wire.
//!
//! Both the on-screen display and the headless decoder used by the self-test
//! need the exact same reconstruction, so it lives here once.

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{bail, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
};

/// Presentation timestamps travel in microseconds.
pub const TIMESCALE: i32 = 1_000_000;
/// VideoToolbox emits length-prefixed NAL units with a 4-byte length.
const NAL_LENGTH_BYTES: i32 = 4;

pub fn invalid_time() -> CMTime {
    CMTime {
        value: 0,
        timescale: 0,
        flags: CMTimeFlags::empty(),
        epoch: 0,
    }
}

pub fn micros(value: u64) -> CMTime {
    CMTime {
        value: value as i64,
        timescale: TIMESCALE,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

/// Builds a decoder format description from HEVC parameter sets.
pub fn format_from_params(params: &[Vec<u8>]) -> Result<CFRetained<CMFormatDescription>> {
    if params.is_empty() || params.iter().any(Vec::is_empty) {
        bail!("parameter sets are missing or empty");
    }
    let pointers: Vec<NonNull<u8>> = params
        .iter()
        .map(|p| NonNull::new(p.as_ptr() as *mut u8).expect("checked non-empty"))
        .collect();
    let sizes: Vec<usize> = params.iter().map(Vec::len).collect();

    let mut format: *const CMFormatDescription = std::ptr::null();
    let status = unsafe {
        objc2_core_media::CMVideoFormatDescriptionCreateFromHEVCParameterSets(
            None,
            pointers.len(),
            NonNull::from(&pointers[0]),
            NonNull::from(&sizes[0]),
            NAL_LENGTH_BYTES,
            None,
            NonNull::from(&mut format),
        )
    };
    if status != 0 || format.is_null() {
        bail!("could not build a format description from parameter sets (status {status})");
    }
    Ok(unsafe {
        CFRetained::from_raw(NonNull::new(format as *mut CMFormatDescription).expect("checked"))
    })
}

/// Wraps encoded frame bytes in a sample buffer the decoder can consume.
pub fn sample_from_frame(
    data: &[u8],
    format: &CMFormatDescription,
    pts_micros: u64,
) -> Result<CFRetained<CMSampleBuffer>> {
    if data.is_empty() {
        bail!("frame is empty");
    }
    let block = block_buffer(data)?;
    let timing = CMSampleTimingInfo {
        duration: invalid_time(),
        presentationTimeStamp: micros(pts_micros),
        decodeTimeStamp: invalid_time(),
    };
    let size = data.len();

    let mut sample: *mut CMSampleBuffer = std::ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &timing,
            1,
            &size,
            NonNull::from(&mut sample),
        )
    };
    if status != 0 || sample.is_null() {
        bail!("CMSampleBufferCreateReady failed with status {status}");
    }
    Ok(unsafe { CFRetained::from_raw(NonNull::new(sample).expect("checked")) })
}

/// Copies `data` into a freshly allocated block buffer.
///
/// The copy is deliberate: the sample buffer outlives this call, so it must not
/// borrow the caller's reassembly scratch space.
fn block_buffer(data: &[u8]) -> Result<CFRetained<CMBlockBuffer>> {
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            data.len(),
            None,
            std::ptr::null(),
            0,
            data.len(),
            objc2_core_media::kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    if status != 0 || block.is_null() {
        bail!("CMBlockBufferCreateWithMemoryBlock failed with status {status}");
    }
    let block = unsafe { CFRetained::from_raw(NonNull::new(block).expect("checked")) };

    let source = NonNull::new(data.as_ptr() as *mut c_void).expect("data is non-empty");
    let status = unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, data.len()) };
    if status != 0 {
        bail!("CMBlockBufferReplaceDataBytes failed with status {status}");
    }
    Ok(block)
}
