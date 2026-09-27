//! Media Foundation plumbing the encoder and decoder share: starting it,
//! finding transforms, building samples and collecting their output.

use std::mem::ManuallyDrop;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

/// Starts Media Foundation once per process. Every `Decoder::new` and
/// `Encoder::new` calls this.
static MF_STARTUP: OnceLock<std::result::Result<(), String>> = OnceLock::new();

pub fn ensure_media_foundation() -> Result<()> {
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

/// Presentation timestamps travel in microseconds on the wire; Media
/// Foundation counts in 100 ns units.
pub fn micros_to_100ns(micros: u64) -> i64 {
    (micros as i64).saturating_mul(10)
}

/// Media Foundation's 100 ns units back to microseconds, with times before
/// zero read as zero.
pub fn hundred_ns_to_micros(value: i64) -> u64 {
    value.max(0) as u64 / 10
}

pub fn variant_u32(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        // VARIANT.Anonymous.Anonymous is a ManuallyDrop reached through a
        // union field, so assignments go through an explicit `*`
        (*variant.Anonymous.Anonymous).vt = VT_UI4;
        (*variant.Anonymous.Anonymous).Anonymous.ulVal = value;
    }
    variant
}

pub fn variant_bool(value: bool) -> VARIANT {
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
pub fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// Video MFTs in `category` taking the `input` subtype and producing the
/// `output` one, in the order `MFTEnumEx` ranks them. `None` leaves that
/// side open.
pub fn enumerate(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<&GUID>,
    output: Option<&GUID>,
) -> Vec<IMFActivate> {
    let info = |subtype: &GUID| MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let (input, output) = (input.map(info), output.map(info));
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let result = unsafe {
        MFTEnumEx(
            category,
            flags,
            input.as_ref().map(std::ptr::from_ref),
            output.as_ref().map(std::ptr::from_ref),
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

pub fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut length) }
        .ok()?;
    let name = unsafe { value.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(value.0 as *const core::ffi::c_void)) };
    name.filter(|name| !name.trim().is_empty())
}

/// A sample holding a copy of `data`. The caller sets its timing.
pub fn sample_with_bytes(data: &[u8]) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(data.len() as u32) }
        .context("MFCreateMemoryBuffer failed")?;
    unsafe {
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
    }
    let sample = unsafe { MFCreateSample() }.context("MFCreateSample failed")?;
    unsafe { sample.AddBuffer(&buffer) }?;
    Ok(sample)
}

/// A sample with an empty buffer of `size` bytes, for a transform that
/// writes its output into one the caller provides.
pub fn empty_sample(size: u32) -> Result<IMFSample> {
    let buffer =
        unsafe { MFCreateMemoryBuffer(size.max(1)) }.context("MFCreateMemoryBuffer failed")?;
    let sample = unsafe { MFCreateSample() }.context("MFCreateSample failed")?;
    unsafe { sample.AddBuffer(&buffer) }.context("AddBuffer failed")?;
    Ok(sample)
}

/// What one `ProcessOutput` call produced.
pub enum ProcessOutcome {
    Sample(IMFSample),
    NeedMoreInput,
    StreamChange,
}

/// Runs `ProcessOutput` once, writing into `own_sample` for a transform that
/// does not provide its own.
pub fn process_output(mft: &IMFTransform, own_sample: Option<IMFSample>) -> Result<ProcessOutcome> {
    let mut buffer = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(own_sample),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    };
    let mut status = 0u32;
    let outcome = unsafe { mft.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
    // Reclaim ownership so whatever ended up in the buffer is released
    // whichever branch below is taken
    let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
    let _events = unsafe { ManuallyDrop::take(&mut buffer.pEvents) };

    match outcome {
        Ok(()) => Ok(ProcessOutcome::Sample(
            sample.context("ProcessOutput succeeded without producing a sample")?,
        )),
        Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
            Ok(ProcessOutcome::NeedMoreInput)
        }
        Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
            Ok(ProcessOutcome::StreamChange)
        }
        Err(error) => bail!("ProcessOutput failed: {error}"),
    }
}

/// Releases an MFT's worker threads and event queue. Asynchronous encoders
/// need this, and it is harmless on the rest.
pub fn shut_down(mft: &IMFTransform) {
    if let Ok(shutdown) = mft.cast::<IMFShutdown>() {
        let _ = unsafe { shutdown.Shutdown() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micros_to_100ns_scales_by_ten() {
        assert_eq!(micros_to_100ns(0), 0);
        assert_eq!(micros_to_100ns(1), 10);
        // 1/30s in whole microseconds, matching a 30 fps test stream.
        assert_eq!(micros_to_100ns(33_333), 333_330);
    }

    #[test]
    fn hundred_ns_to_micros_divides_by_ten() {
        assert_eq!(hundred_ns_to_micros(0), 0);
        assert_eq!(hundred_ns_to_micros(10), 1);
        assert_eq!(hundred_ns_to_micros(333_330), 33_333);
        // A time before the stream began reads as its start
        assert_eq!(hundred_ns_to_micros(-5), 0);
    }
}
