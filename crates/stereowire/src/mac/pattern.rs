//! Synthetic video frames for the self-test.
//!
//! Using a generated pattern instead of a real screen capture means the video
//! path can be tested without Screen Recording permission, and gives an exact
//! reference image to compare the decoded output against.

use std::ptr::NonNull;

use anyhow::{bail, Result};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferLockFlags};

pub struct TestPattern {
    pub width: usize,
    pub height: usize,
}

impl TestPattern {
    /// The reference luma plane for frame `index`.
    ///
    /// A static gradient exercises spatial detail while a moving box forces
    /// real inter-frame prediction, so the test covers both I- and P-frames.
    pub fn luma(&self, index: u32) -> Vec<u8> {
        let mut plane = vec![0u8; self.width * self.height];
        let box_size = self.width / 8;
        let travel = self.width.saturating_sub(box_size).max(1);
        let box_x = (index as usize * 17) % travel;
        let box_y = (index as usize * 11) % self.height.saturating_sub(box_size).max(1);

        for y in 0..self.height {
            for x in 0..self.width {
                let gradient =
                    ((x * 255 / self.width.max(1)) ^ (y * 255 / self.height.max(1))) as u8;
                let inside =
                    x >= box_x && x < box_x + box_size && y >= box_y && y < box_y + box_size;
                plane[y * self.width + x] = if inside { 235 } else { gradient };
            }
        }
        plane
    }

    /// Builds a pixel buffer holding frame `index`, ready for the encoder.
    pub fn frame(&self, index: u32) -> Result<(CFRetained<CVPixelBuffer>, Vec<u8>)> {
        let luma = self.luma(index);
        let buffer = self.pixel_buffer()?;

        let flags = CVPixelBufferLockFlags::empty();
        if unsafe { objc2_core_video::CVPixelBufferLockBaseAddress(&buffer, flags) } != 0 {
            bail!("could not lock the pixel buffer for writing");
        }

        let write = || {
            let base = objc2_core_video::CVPixelBufferGetBaseAddressOfPlane(&buffer, 0);
            let stride = objc2_core_video::CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0);
            if base.is_null() || stride < self.width {
                return false;
            }
            for y in 0..self.height {
                let row = unsafe { (base as *mut u8).add(y * stride) };
                unsafe {
                    std::ptr::copy_nonoverlapping(luma[y * self.width..].as_ptr(), row, self.width)
                };
            }

            // Neutral chroma: the test only measures luma fidelity.
            let base = objc2_core_video::CVPixelBufferGetBaseAddressOfPlane(&buffer, 1);
            let stride = objc2_core_video::CVPixelBufferGetBytesPerRowOfPlane(&buffer, 1);
            if base.is_null() {
                return false;
            }
            for y in 0..self.height / 2 {
                let row = unsafe { (base as *mut u8).add(y * stride) };
                unsafe { std::ptr::write_bytes(row, 128, self.width) };
            }
            true
        };
        let ok = write();
        unsafe { objc2_core_video::CVPixelBufferUnlockBaseAddress(&buffer, flags) };
        if !ok {
            bail!("could not address the pixel buffer planes");
        }
        Ok((buffer, luma))
    }

    fn pixel_buffer(&self) -> Result<CFRetained<CVPixelBuffer>> {
        let mut buffer: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            objc2_core_video::CVPixelBufferCreate(
                None,
                self.width,
                self.height,
                objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                None,
                NonNull::from(&mut buffer),
            )
        };
        if status != 0 || buffer.is_null() {
            bail!("CVPixelBufferCreate failed with status {status}");
        }
        Ok(unsafe { CFRetained::from_raw(NonNull::new(buffer).expect("checked")) })
    }
}

/// Peak signal-to-noise ratio in dB between a reference and a decoded plane.
///
/// Returns `None` if the planes are not comparable. `f64::INFINITY` means the
/// two planes are identical.
pub fn psnr(reference: &[u8], decoded: &[u8]) -> Option<f64> {
    if reference.is_empty() || reference.len() != decoded.len() {
        return None;
    }
    let sum: f64 = reference
        .iter()
        .zip(decoded)
        .map(|(&a, &b)| {
            let diff = f64::from(a) - f64::from(b);
            diff * diff
        })
        .sum();
    let mse = sum / reference.len() as f64;
    if mse == 0.0 {
        return Some(f64::INFINITY);
    }
    Some(10.0 * (255.0f64 * 255.0 / mse).log10())
}
