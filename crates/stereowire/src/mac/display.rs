//! Receiver-side video: rebuilds sample buffers from the wire and shows them.
//!
//! `AVSampleBufferDisplayLayer` handles hardware decode and presentation, so
//! the only work here is reconstructing the `CMSampleBuffer`s that VideoToolbox
//! took apart on the sending end.

use anyhow::Result;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask,
};
use objc2_av_foundation::{AVQueuedSampleBufferRendering, AVSampleBufferDisplayLayer};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_media::{kCMSampleAttachmentKey_DisplayImmediately, CMFormatDescription};
use objc2_foundation::{NSMutableDictionary, NSString};

use super::{cf, sample};

pub struct Display {
    layer: Retained<AVSampleBufferDisplayLayer>,
    window: Retained<NSWindow>,
    /// Cached decoder format, rebuilt whenever the parameter sets change.
    format: Option<CFRetained<CMFormatDescription>>,
    params: Vec<Vec<u8>>,
}

impl Display {
    pub fn open(mtm: MainThreadMarker, title: &str, width: i32, height: i32) -> Result<Self> {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

        // Open at half the incoming pixel size so a Retina capture lands in a
        // sensibly-sized window on a Retina display.
        let content = CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(f64::from(width) / 2.0, f64::from(height) / 2.0),
        );
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                content,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str(title));
        window.center();

        let layer = unsafe { AVSampleBufferDisplayLayer::new() };
        unsafe {
            layer.setVideoGravity(
                objc2_av_foundation::AVLayerVideoGravityResizeAspect.expect("gravity constant"),
            )
        };

        // A layer-hosting view: AppKit resizes our layer with the window.
        let view = window.contentView().expect("window has a content view");
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);

        window.makeKeyAndOrderFront(None);
        app.activate();

        Ok(Display {
            layer,
            window,
            format: None,
            params: Vec::new(),
        })
    }

    /// Installs new HEVC parameter sets, replacing the decoder format.
    pub fn set_params(&mut self, params: &[Vec<u8>]) -> Result<()> {
        if params.is_empty() || params == self.params {
            return Ok(());
        }
        self.format = Some(sample::format_from_params(params)?);
        self.params = params.to_vec();
        // The decoder must start over on the next keyframe with the new format.
        unsafe { self.layer.sampleBufferRenderer().flush() };
        Ok(())
    }

    /// Decodes and shows one frame. Returns false if no format is known yet.
    pub fn present(&mut self, data: &[u8], pts_micros: u64) -> Result<bool> {
        let Some(format) = self.format.clone() else {
            return Ok(false);
        };
        let sample = sample::sample_from_frame(data, &format, pts_micros)?;

        // We drive presentation ourselves rather than via a timebase, so tell
        // the layer to show each frame the moment it decodes.
        unsafe {
            if let Some(attachments) = sample.sample_attachments_array(true) {
                let array = cf::as_ns_array(&attachments);
                if !array.is_empty() {
                    let entry = array.objectAtIndex(0);
                    let mutable: &NSMutableDictionary<NSString, objc2::runtime::AnyObject> =
                        &*(&*entry as *const _
                            as *const NSMutableDictionary<NSString, objc2::runtime::AnyObject>);
                    mutable.setObject_forKey(
                        &cf::bool_value(true),
                        objc2::runtime::ProtocolObject::from_ref(cf::key(
                            kCMSampleAttachmentKey_DisplayImmediately,
                        )),
                    );
                }
            }
            self.layer
                .sampleBufferRenderer()
                .enqueueSampleBuffer(&sample);
        }
        Ok(true)
    }

    /// Updating the title is a main-thread AppKit call, so it is done on a
    /// timer rather than per frame.
    pub fn set_title(&self, title: &NSString) {
        self.window.setTitle(title);
    }

    pub fn is_open(&self) -> bool {
        self.window.isVisible()
    }
}
