//! Screen and system-audio capture via ScreenCaptureKit.
//!
//! One `SCStream` carries both the display frames and the system audio mix, so
//! whatever Logic is playing through the output device arrives here without any
//! virtual audio device (BlackHole and friends) in the path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Context, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::CVImageBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};
use stereowire_proto::packet::{SampleRate, AUDIO_CHANNELS};

/// A captured video frame, in whatever type the platform's capture API
/// delivers it as.
pub type CapturedFrame = CVImageBuffer;

/// Pixel dimensions the platform actually encoded the frame at.
pub fn frame_size(frame: &CapturedFrame) -> (i32, i32) {
    let size = objc2_core_video::CVImageBufferGetEncodedSize(frame);
    (size.width as i32, size.height as i32)
}

/// Receives capture output. Called on ScreenCaptureKit's delivery queue, so
/// implementations must not block.
pub trait CaptureSink: Send + Sync {
    fn on_video(&self, frame: &CapturedFrame, pts_micros: u64);
    /// Interleaved stereo f32, at whatever rate the stream is actually running.
    ///
    /// `pts_micros` is on the same capture clock as [`CaptureSink::on_video`],
    /// which is what makes the two streams comparable.
    fn on_audio(&self, interleaved: &[f32], rate: SampleRate, pts_micros: u64);
}

pub struct Ivars {
    sink: Arc<dyn CaptureSink>,
    /// Ensures a persistent audio problem is reported only once.
    audio_warned: AtomicBool,
}

define_class!(
    // SAFETY:
    // - NSObject imposes no subclassing requirements.
    // - StreamOutput does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "StereoWireStreamOutput"]
    #[ivars = Ivars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            match kind {
                SCStreamOutputType::Screen => self.handle_video(sample),
                SCStreamOutputType::Audio => self.handle_audio(sample),
                _ => {}
            }
        }
    }
);

impl StreamOutput {
    fn new(sink: Arc<dyn CaptureSink>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            sink,
            audio_warned: AtomicBool::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn handle_video(&self, sample: &CMSampleBuffer) {
        // Frames with no image buffer are status-only updates (for example, a
        // "screen idle, nothing changed" notice) and carry no picture.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let pts = unsafe { sample.presentation_time_stamp() };
        self.ivars().sink.on_video(&image, cmtime_micros(pts));
    }

    fn handle_audio(&self, sample: &CMSampleBuffer) {
        // Read the rate the stream is really running at: ScreenCaptureKit does
        // not honour every request, and guessing wrong pitch-shifts everything.
        let Some(rate) = (unsafe { stream_sample_rate(sample) }) else {
            if !self.ivars().audio_warned.swap(true, Ordering::Relaxed) {
                eprintln!("audio capture is running at an unsupported sample rate");
            }
            return;
        };
        let pts = cmtime_micros(unsafe { sample.presentation_time_stamp() });
        match unsafe { interleaved_audio(sample) } {
            Ok(samples) => self.ivars().sink.on_audio(&samples, rate, pts),
            Err(reason) => {
                // Report once rather than flooding: the cause does not change.
                if !self.ivars().audio_warned.swap(true, Ordering::Relaxed) {
                    eprintln!("audio capture produced no samples: {reason}");
                }
            }
        }
    }
}

fn cmtime_micros(time: CMTime) -> u64 {
    if !time.flags.contains(CMTimeFlags::Valid) || time.timescale <= 0 {
        return 0;
    }
    (time.value.max(0) as i128 * 1_000_000 / time.timescale as i128) as u64
}

/// The sample rate the capture is actually delivering.
unsafe fn stream_sample_rate(sample: &CMSampleBuffer) -> Option<SampleRate> {
    let format = unsafe { sample.format_description() }?;
    // An audio sample buffer's format description is an audio one.
    let asbd = unsafe {
        objc2_core_media::CMAudioFormatDescriptionGetStreamBasicDescription(std::mem::transmute::<
            &objc2_core_media::CMFormatDescription,
            &objc2_core_media::CMAudioFormatDescription,
        >(&*format))
    };
    if asbd.is_null() {
        return None;
    }
    SampleRate::from_hz(unsafe { (*asbd).mSampleRate } as u32)
}

/// Pulls PCM out of an audio sample buffer, interleaving if needed.
///
/// ScreenCaptureKit hands over *non-interleaved* float audio, one buffer per
/// channel, which is not what an audio output or the wire format wants.
unsafe fn interleaved_audio(sample: &CMSampleBuffer) -> std::result::Result<Vec<f32>, String> {
    let frames = unsafe { sample.num_samples() } as usize;
    if frames == 0 {
        return Err("sample buffer reported zero frames".into());
    }

    // Ask CoreMedia how much room the buffer list needs rather than guessing:
    // the channel count is not known ahead of time, and a short buffer fails
    // with kCMSampleBufferError_ArrayTooSmall.
    let mut needed = 0usize;
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            std::ptr::null_mut(),
            0,
            None,
            None,
            0,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(format!(
            "could not size the audio buffer list (status {status})"
        ));
    }
    if needed == 0 {
        return Err("audio buffer list reported zero size".into());
    }

    // Backed by u64 so the AudioBuffer pointers inside land on a valid alignment.
    let mut storage = vec![0u64; needed.div_ceil(8)];
    let list = storage.as_mut_ptr() as *mut AudioBufferList;

    // The returned block buffer owns the sample memory and must outlive our
    // reads, so it is held until the end of this function.
    let mut block: *mut objc2_core_media::CMBlockBuffer = std::ptr::null_mut();
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            list,
            needed,
            None,
            None,
            0,
            &mut block,
        )
    };
    if status != 0 {
        return Err(format!(
            "could not read the audio buffer list (status {status})"
        ));
    }
    let _block = (!block.is_null()).then(|| unsafe {
        objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new(block).expect("checked"))
    });

    let count = unsafe { (*list).mNumberBuffers } as usize;
    if count == 0 {
        return Err("buffer list came back empty".into());
    }
    let buffers = unsafe {
        std::slice::from_raw_parts(
            std::ptr::addr_of!((*list).mBuffers) as *const objc2_core_audio_types::AudioBuffer,
            count,
        )
    };

    let channel = |index: usize| -> Option<&[f32]> {
        let b = buffers.get(index)?;
        (!b.mData.is_null()).then(|| unsafe {
            std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
        })
    };

    let mut out = Vec::with_capacity(frames * AUDIO_CHANNELS);
    if count == 1 && buffers[0].mNumberChannels >= 2 {
        // Already interleaved; take the first two channels.
        let src = channel(0).ok_or("interleaved buffer had no data pointer")?;
        let stride = buffers[0].mNumberChannels as usize;
        for frame in src.chunks_exact(stride).take(frames) {
            out.push(frame[0]);
            out.push(frame[1]);
        }
    } else {
        // Planar: one buffer per channel. Mono is duplicated to both sides.
        let left = channel(0).ok_or("first channel had no data pointer")?;
        let right = channel(1).unwrap_or(left);
        let frames = frames.min(left.len()).min(right.len());
        for i in 0..frames {
            out.push(left[i]);
            out.push(right[i]);
        }
    }
    if out.is_empty() {
        return Err("no samples were extracted".into());
    }
    Ok(out)
}

/// A running capture. Dropping it stops the stream.
pub struct Capture {
    stream: Retained<SCStream>,
    /// Pixel dimensions ScreenCaptureKit was asked to deliver.
    pub size: (i32, i32),
    _output: Retained<StreamOutput>,
}

pub struct CaptureOptions {
    pub fps: i32,
    /// Captured at the source's own rate where possible, so a 44.1 kHz project
    /// is never resampled on its way onto the wire.
    pub sample_rate: SampleRate,
    /// Downscales the capture if the display is wider than this, to keep the
    /// encoder and the link within budget.
    pub max_width: i32,
    pub show_cursor: bool,
    /// Windows only. ScreenCaptureKit captures the system mix, so a request
    /// is noted and ignored.
    pub audio_device: Option<String>,
}

impl Capture {
    pub fn start(options: &CaptureOptions, sink: Arc<dyn CaptureSink>) -> Result<Self> {
        if options.audio_device.is_some() {
            println!(
                "note: audio device selection applies to Windows only, ScreenCaptureKit \
                 captures the system mix"
            );
        }
        let content = shareable_content()?;
        let displays = unsafe { content.displays() };
        let display = displays.firstObject().context(
            "no capturable display found (grant Screen Recording in System Settings > Privacy & Security)",
        )?;

        // SCDisplay reports points; ask for the backing pixels so text stays sharp.
        let scale = backing_scale();
        let mut width = (unsafe { display.width() } as f64 * scale).round() as i32;
        let mut height = (unsafe { display.height() } as f64 * scale).round() as i32;
        if width > options.max_width {
            height = (height as f64 * options.max_width as f64 / width as f64).round() as i32;
            width = options.max_width;
        }
        // Encoders want even dimensions.
        width &= !1;
        height &= !1;

        let config = configure(options, width, height)?;
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            )
        };

        let output = StreamOutput::new(sink);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };

        let protocol = ProtocolObject::from_ref(&*output);
        for kind in [SCStreamOutputType::Screen, SCStreamOutputType::Audio] {
            // A dedicated serial queue per track keeps audio delivery from
            // queueing behind a large video frame.
            let queue = dispatch2::DispatchQueue::new(
                if kind == SCStreamOutputType::Screen {
                    "stereowire.video"
                } else {
                    "stereowire.audio"
                },
                None,
            );
            unsafe {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(protocol, kind, Some(&queue))
                    .map_err(|e| anyhow::anyhow!("addStreamOutput failed: {e:?}"))?;
            }
        }

        start_capture(&stream)?;
        Ok(Capture {
            stream,
            size: (width, height),
            _output: output,
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = done.clone();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let (lock, cv) = &*signal;
            *lock.lock().expect("not poisoned") = true;
            cv.notify_all();
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let (lock, cv) = &*done;
        let mut finished = lock.lock().expect("not poisoned");
        while !*finished {
            let (guard, timeout) = cv
                .wait_timeout(finished, std::time::Duration::from_secs(2))
                .expect("not poisoned");
            finished = guard;
            if timeout.timed_out() {
                break;
            }
        }
    }
}

fn configure(
    options: &CaptureOptions,
    width: i32,
    height: i32,
) -> Result<Retained<SCStreamConfiguration>> {
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setShowsCursor(options.show_cursor);
        // 4:2:0 keeps the hardware encoder on its fast path. Text stays legible
        // because we send native resolution at a high bitrate.
        config.setPixelFormat(objc2_core_video::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
        // Pin the color space so both ends agree rather than guessing.
        config.setColorSpaceName(objc2_core_graphics::kCGColorSpaceSRGB);
        config.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: options.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        // Shallow queue: we would rather drop a frame than show a stale one.
        config.setQueueDepth(5);

        config.setCapturesAudio(true);
        config.setSampleRate(options.sample_rate.hz() as isize);
        config.setChannelCount(stereowire_proto::packet::AUDIO_CHANNELS as isize);
        // Without this our own playback would be captured and echoed back.
        config.setExcludesCurrentProcessAudio(true);
    }
    Ok(config)
}

/// The main display's backing scale factor (2.0 on Retina).
fn backing_scale() -> f64 {
    objc2_foundation::MainThreadMarker::new()
        .and_then(objc2_app_kit::NSScreen::mainScreen)
        .map(|screen| screen.backingScaleFactor())
        .unwrap_or(2.0)
}

/// An owned `SCShareableContent` in transit from ScreenCaptureKit's completion
/// handler, which runs on one of the framework's own threads.
///
/// objc2 marks `Retained` as `!Send`, since that is the correct default across
/// all Objective-C classes, so ownership moves as a raw pointer instead.
struct OwnedContent(*mut SCShareableContent);

// SAFETY: Objective-C reference counting is atomic, and SCShareableContent is an
// immutable snapshot of the shareable displays, so handing one to another thread
// races with nothing. Exactly one `Retained` is rebuilt from the pointer, by the
// receiving side.
unsafe impl Send for OwnedContent {}

type ContentSlot = Arc<(Mutex<Option<Result<OwnedContent, String>>>, Condvar)>;

/// Wraps ScreenCaptureKit's async content query in a blocking call.
fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let slot: ContentSlot = Arc::new((Mutex::new(None), Condvar::new()));
    let fill = slot.clone();

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if content.is_null() {
                let message = unsafe { error.as_ref() }
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "unknown error".into());
                Err(message)
            } else {
                // Retained here; the waiting thread takes over that ownership.
                let retained = unsafe { Retained::retain(content) }.expect("non-null");
                Ok(OwnedContent(Retained::into_raw(retained)))
            };
            let (lock, cv) = &*fill;
            *lock.lock().expect("not poisoned") = Some(result);
            cv.notify_all();
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    let (lock, cv) = &*slot;
    let mut guard = lock.lock().expect("not poisoned");
    while guard.is_none() {
        let (next, timeout) = cv
            .wait_timeout(guard, std::time::Duration::from_secs(10))
            .expect("not poisoned");
        guard = next;
        if timeout.timed_out() && guard.is_none() {
            bail!("timed out querying shareable content; is Screen Recording permission granted?");
        }
    }
    match guard.take().expect("filled") {
        // SAFETY: the pointer came from `Retained::into_raw` in the handler
        // above, and is consumed exactly once here.
        Ok(content) => Ok(unsafe { Retained::from_raw(content.0) }.expect("non-null")),
        Err(e) => bail!("could not list displays: {e}"),
    }
}

fn start_capture(stream: &SCStream) -> Result<()> {
    let slot: Arc<(Mutex<Option<Option<String>>>, Condvar)> =
        Arc::new((Mutex::new(None), Condvar::new()));
    let fill = slot.clone();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let message = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let (lock, cv) = &*fill;
        *lock.lock().expect("not poisoned") = Some(message);
        cv.notify_all();
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

    let (lock, cv) = &*slot;
    let mut guard = lock.lock().expect("not poisoned");
    while guard.is_none() {
        let (next, timeout) = cv
            .wait_timeout(guard, std::time::Duration::from_secs(10))
            .expect("not poisoned");
        guard = next;
        if timeout.timed_out() && guard.is_none() {
            bail!("timed out starting capture");
        }
    }
    match guard.take().expect("filled") {
        Some(message) => bail!("could not start capture: {message}"),
        None => Ok(()),
    }
}
