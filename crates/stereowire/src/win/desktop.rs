//! Screen capture through DXGI Desktop Duplication, on a thread of its own.
//!
//! Desktop Duplication hands over the desktop as a BGRA texture on the GPU
//! with no pointer drawn in, and the pointer's position and shape
//! separately. Each frame is copied to a staging texture, read back, given
//! the pointer, halved when wider than `max_width`, converted to NV12 (see
//! `pixels`) and handed to the sink.
//!
//! None of this runs under Wine, so its first run is on a real PC. The
//! failures Windows documents for it each get a message of their own:
//! sessions and virtual machines that refuse duplication, the secure desktop
//! of a UAC prompt or the lock screen, a display mode change, and HDR.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, E_NOTIMPL, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use super::capture::{now_micros, qpc_micros, CaptureOptions, CaptureSink};
use super::pixels::{bgra_to_nv12, downscale_half, draw_cursor, CursorKind, CursorShape};
use crate::pattern::Nv12Frame;

/// While the screen is still, the last frame goes out again this often, so a
/// keyframe request or a receiver joining late is served within a second.
const STILL_RESEND: Duration = Duration::from_secs(1);

/// How often a desktop Windows took away is asked for again.
const REOPEN_INTERVAL: Duration = Duration::from_millis(250);

const HDR_MESSAGE: &str = "the display is in HDR, which the sender cannot capture yet. Turn off \
     \"Use HDR\" in Settings > System > Display and start stereowire again";

/// The capture thread. Dropping this stops and joins it.
pub(super) struct DesktopCapture {
    /// Pixel dimensions of the frames it delivers.
    pub size: (i32, i32),
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Opens the primary display on a new thread and starts delivering its
/// frames to `sink`. Returns once the display is open, or with the reason it
/// could not be.
pub(super) fn start(
    options: &CaptureOptions,
    sink: Arc<dyn CaptureSink>,
) -> Result<DesktopCapture> {
    let fps = options.fps.max(1);
    let max_width = options.max_width;
    let show_cursor = options.show_cursor;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let (opened, open_result) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("stereowire-capture".into())
        .spawn(move || {
            // The encoder runs on this thread too, and Media Foundation is
            // built for the multithreaded apartment
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let setup = Duplicator::open().and_then(|duplicator| {
                let plan = Plan::new(duplicator.desktop, max_width)?;
                Ok((duplicator, plan))
            });
            match setup {
                Ok((duplicator, plan)) => {
                    let _ = opened.send(Ok(plan.output));
                    run(duplicator, &plan, fps, show_cursor, &*sink, &thread_stop);
                }
                Err(error) => {
                    let _ = opened.send(Err(error));
                }
            }
        })
        .context("could not start the capture thread")?;

    let outcome = open_result.recv().unwrap_or_else(|_| {
        Err(anyhow!(
            "the capture thread ended before the display opened"
        ))
    });
    match outcome {
        Ok((width, height)) => Ok(DesktopCapture {
            size: (width as i32, height as i32),
            stop,
            thread: Some(thread),
        }),
        Err(error) => {
            let _ = thread.join();
            Err(error)
        }
    }
}

/// The sizes a frame passes through: the desktop cropped to even
/// dimensions, then halved when wider than `max_width`.
struct Plan {
    crop: (usize, usize),
    halve: bool,
    output: (usize, usize),
}

impl Plan {
    fn new(desktop: (usize, usize), max_width: i32) -> Result<Self> {
        let crop = (desktop.0 & !1, desktop.1 & !1);
        let halve = crop.0 > max_width.max(0) as usize;
        let output = if halve {
            ((crop.0 / 2) & !1, (crop.1 / 2) & !1)
        } else {
            crop
        };
        if output.0 == 0 || output.1 == 0 {
            bail!("the display reports a size of {}x{}", desktop.0, desktop.1);
        }
        if halve {
            println!(
                "capture: halving the {}x{} desktop to {}x{} to fit --max-width {max_width}",
                desktop.0, desktop.1, output.0, output.1
            );
        }
        Ok(Plan {
            crop,
            halve,
            output,
        })
    }
}

/// Why the capture loop has to stop or reopen the duplication.
enum Interruption {
    /// Windows took the duplication away and it has to be opened again.
    AccessLost,
    Fatal(String),
}

fn interruption(call: &str, error: windows::core::Error) -> Interruption {
    if error.code() == DXGI_ERROR_ACCESS_LOST {
        Interruption::AccessLost
    } else {
        Interruption::Fatal(format!("{call} failed ({error})"))
    }
}

/// What one wait for the next desktop update came to.
enum Step {
    /// Nothing changed within the timeout.
    Timeout,
    /// Something changed that the picture does not show.
    Unchanged,
    /// The desktop was read into the caller's buffer. `present_time` is the
    /// performance counter at the update, or zero when only the pointer
    /// changed, and `acquired` is when the frame came in.
    Frame {
        present_time: i64,
        acquired: Instant,
    },
}

/// What one acquired frame changed.
struct Changed {
    desktop: bool,
    pointer: bool,
}

/// The outcome of asking for the desktop again after Windows took it away.
enum Reopen {
    Open,
    /// Not available yet, as during a UAC prompt or on the lock screen.
    Unavailable(windows::core::Error),
    Failed(String),
}

/// The pointer as the frames so far have described it.
#[derive(Default)]
struct Pointer {
    visible: bool,
    x: i32,
    y: i32,
    shape: Option<CursorShape>,
}

/// The primary display's duplication, and what reading it back needs.
struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    /// The texture the CPU reads, and the format it was made for.
    staging: Option<(ID3D11Texture2D, DXGI_FORMAT)>,
    /// The desktop's size when capture started, which a session keeps.
    desktop: (usize, usize),
}

impl Duplicator {
    fn open() -> Result<Self> {
        // Physical pixels for the desktop and the pointer alike, whatever
        // the display's scaling. Failure means the process already chose
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let (adapter, output) = primary_output()?;
        let (device, context) = create_device(&adapter)?;
        let output: IDXGIOutput1 = output.cast().context(
            "this Windows has no Desktop Duplication (IDXGIOutput1 is missing). The sender needs \
             Windows 8 or later",
        )?;
        let duplication = unsafe { output.DuplicateOutput(&device) }.map_err(refused)?;
        let desc = unsafe { duplication.GetDesc() };
        if desc.ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            bail!(HDR_MESSAGE);
        }
        if desc.Rotation == DXGI_MODE_ROTATION_ROTATE90
            || desc.Rotation == DXGI_MODE_ROTATION_ROTATE180
            || desc.Rotation == DXGI_MODE_ROTATION_ROTATE270
        {
            println!(
                "note: the display is rotated, which the sender does not undo yet, so the picture \
                 arrives rotated too"
            );
        }
        Ok(Duplicator {
            device,
            context,
            output,
            duplication: Some(duplication),
            staging: None,
            desktop: (desc.ModeDesc.Width as usize, desc.ModeDesc.Height as usize),
        })
    }

    /// Waits up to `timeout_ms` for the desktop or the pointer to change,
    /// and reads the desktop into `bgra`, cropped to `crop`, when something
    /// the picture shows did.
    fn next(
        &mut self,
        timeout_ms: u32,
        pointer: &mut Pointer,
        show_cursor: bool,
        crop: (usize, usize),
        bgra: &mut Vec<u8>,
    ) -> Result<Step, Interruption> {
        let duplication = self.duplication.clone().ok_or(Interruption::AccessLost)?;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        if let Err(error) =
            unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) }
        {
            if error.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(Step::Timeout);
            }
            return Err(interruption("AcquireNextFrame", error));
        }
        let acquired = Instant::now();
        // The frame is held until ReleaseFrame, which has to happen whatever
        // becomes of it
        let taken = self.take(&duplication, &info, resource, pointer);
        let released = unsafe { duplication.ReleaseFrame() };
        let changed = taken?;
        released.map_err(|error| interruption("ReleaseFrame", error))?;

        let shows = changed.desktop || (show_cursor && changed.pointer);
        if !shows {
            return Ok(Step::Unchanged);
        }
        if !self.read(crop, bgra)? {
            return Ok(Step::Unchanged);
        }
        Ok(Step::Frame {
            present_time: info.LastPresentTime,
            acquired,
        })
    }

    /// Stages the desktop image if it changed and records pointer updates.
    /// Runs while the frame is held.
    fn take(
        &mut self,
        duplication: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
        pointer: &mut Pointer,
    ) -> Result<Changed, Interruption> {
        // A zero present time means only the pointer changed. The surface
        // still holds the desktop, so it is staged anyway when nothing has
        // been yet
        let desktop = info.LastPresentTime != 0 || self.staging.is_none();
        if desktop {
            let texture: ID3D11Texture2D = resource
                .ok_or_else(|| Interruption::Fatal("a desktop update came with no image".into()))?
                .cast()
                .map_err(|error| {
                    Interruption::Fatal(format!("the desktop image is not a texture ({error})"))
                })?;
            self.stage(&texture)?;
        }

        // The position is only meaningful when the pointer was updated
        let mut moved = false;
        if info.LastMouseUpdateTime != 0 {
            let position = info.PointerPosition.Position;
            let visible = info.PointerPosition.Visible.as_bool();
            moved = visible != pointer.visible
                || (visible && (position.x, position.y) != (pointer.x, pointer.y));
            pointer.visible = visible;
            pointer.x = position.x;
            pointer.y = position.y;
        }
        let reshaped = info.PointerShapeBufferSize > 0;
        if reshaped {
            match pointer_shape(duplication, info.PointerShapeBufferSize) {
                Ok(shape) => pointer.shape = shape,
                Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                    return Err(Interruption::AccessLost)
                }
                // The previous shape is a better guess than none
                Err(_) => {}
            }
        }
        Ok(Changed {
            desktop,
            pointer: moved || reshaped,
        })
    }

    /// Copies the desktop texture into the staging texture the CPU reads.
    fn stage(&mut self, texture: &ID3D11Texture2D) -> Result<(), Interruption> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            return Err(Interruption::Fatal(HDR_MESSAGE.into()));
        }
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        {
            return Err(Interruption::Fatal(format!(
                "the desktop arrived as DXGI format {}, which the sender cannot read",
                desc.Format.0
            )));
        }
        let size = (desc.Width as usize, desc.Height as usize);
        if size != self.desktop {
            return Err(Interruption::Fatal(resized(self.desktop, size)));
        }
        if !self
            .staging
            .as_ref()
            .is_some_and(|(_, format)| *format == desc.Format)
        {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            }
            .map_err(|error| interruption("CreateTexture2D", error))?;
            let staging = staging
                .ok_or_else(|| Interruption::Fatal("CreateTexture2D produced no texture".into()))?;
            self.staging = Some((staging, desc.Format));
        }
        if let Some((staging, _)) = &self.staging {
            unsafe { self.context.CopyResource(staging, texture) };
        }
        Ok(())
    }

    /// Reads the staged desktop into `bgra`, cropped to `crop`. False when
    /// nothing has been staged yet.
    fn read(&self, crop: (usize, usize), bgra: &mut Vec<u8>) -> Result<bool, Interruption> {
        let Some((staging, _)) = &self.staging else {
            return Ok(false);
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|error| interruption("Map", error))?;
        let (width, height) = crop;
        let row_bytes = width * 4;
        let pitch = mapped.RowPitch as usize;
        let readable = !mapped.pData.is_null() && pitch >= row_bytes;
        if readable {
            bgra.resize(row_bytes * height, 0);
            for (y, row) in bgra.chunks_exact_mut(row_bytes).enumerate() {
                // SAFETY: the mapping holds the texture's rows, `pitch` bytes
                // apart, and the crop never exceeds the texture
                let source = unsafe {
                    std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(y * pitch),
                        row_bytes,
                    )
                };
                row.copy_from_slice(source);
            }
        }
        unsafe { self.context.Unmap(staging, 0) };
        if !readable {
            return Err(Interruption::Fatal(
                "the staging texture could not be read".into(),
            ));
        }
        Ok(true)
    }

    /// Opens the duplication again after Windows took it away.
    fn reopen(&mut self) -> Reopen {
        let duplication = match unsafe { self.output.DuplicateOutput(&self.device) } {
            Ok(duplication) => duplication,
            Err(error) => {
                let passing = [
                    E_ACCESSDENIED,
                    DXGI_ERROR_ACCESS_LOST,
                    DXGI_ERROR_UNSUPPORTED,
                    DXGI_ERROR_SESSION_DISCONNECTED,
                    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
                ];
                return if passing.contains(&error.code()) {
                    Reopen::Unavailable(error)
                } else {
                    Reopen::Failed(format!("could not reopen Desktop Duplication ({error})"))
                };
            }
        };
        let desc = unsafe { duplication.GetDesc() };
        if desc.ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            return Reopen::Failed(HDR_MESSAGE.into());
        }
        let size = (desc.ModeDesc.Width as usize, desc.ModeDesc.Height as usize);
        if size != self.desktop {
            return Reopen::Failed(resized(self.desktop, size));
        }
        self.duplication = Some(duplication);
        Reopen::Open
    }
}

fn resized(was: (usize, usize), now: (usize, usize)) -> String {
    format!(
        "the display changed from {}x{} to {}x{}, which a running session cannot follow. Start \
         stereowire again to share the new size",
        was.0, was.1, now.0, now.1
    )
}

/// The frame most recently handed to the sink, kept to send again while the
/// screen is still.
struct Outbox<'a> {
    sink: &'a dyn CaptureSink,
    frame: Nv12Frame,
    filled: bool,
    last_sent: Option<Instant>,
}

impl Outbox<'_> {
    fn send(&mut self, pts_micros: u64, at: Instant) {
        self.sink.on_video(&self.frame, pts_micros);
        self.filled = true;
        self.last_sent = Some(at);
    }

    fn resend_if_due(&mut self) {
        if self.filled
            && self
                .last_sent
                .is_none_or(|sent| sent.elapsed() >= STILL_RESEND)
        {
            self.send(now_micros(), Instant::now());
        }
    }
}

fn run(
    mut duplicator: Duplicator,
    plan: &Plan,
    fps: i32,
    show_cursor: bool,
    sink: &dyn CaptureSink,
    stop: &AtomicBool,
) {
    let interval = Duration::from_secs(1) / fps as u32;
    let timeout_ms = (1000 / fps as u32).clamp(1, 100);
    let mut pointer = Pointer::default();
    let mut bgra = Vec::new();
    let mut half = Vec::new();
    let mut outbox = Outbox {
        sink,
        frame: Nv12Frame {
            width: plan.output.0,
            height: plan.output.1,
            data: Vec::new(),
        },
        filled: false,
        last_sent: None,
    };

    while !stop.load(Ordering::Relaxed) {
        // At most `fps` frames a second. Waiting out the interval before the
        // next acquire means the frame taken then holds every change up to
        // that moment, so the last change of a burst always goes out
        if let Some(rest) = outbox
            .last_sent
            .and_then(|sent| interval.checked_sub(sent.elapsed()))
        {
            std::thread::sleep(rest);
        }
        match duplicator.next(timeout_ms, &mut pointer, show_cursor, plan.crop, &mut bgra) {
            Ok(Step::Frame {
                present_time,
                acquired,
            }) => {
                let (width, height) = plan.crop;
                if show_cursor && pointer.visible {
                    if let Some(shape) = &pointer.shape {
                        // Desktop Duplication reports where the shape's
                        // top-left corner goes. Its documentation says the
                        // hot spot plays no part in placing it
                        draw_cursor(&mut bgra, width, height, shape, pointer.x, pointer.y);
                    }
                }
                let (image, width, height) = if plan.halve {
                    let (half_width, half_height) = downscale_half(&bgra, width, height, &mut half);
                    (&half, half_width, half_height)
                } else {
                    (&bgra, width, height)
                };
                bgra_to_nv12(image, width, height, &mut outbox.frame.data);
                let pts_micros = if present_time != 0 {
                    qpc_micros(present_time)
                } else {
                    now_micros()
                };
                outbox.send(pts_micros, acquired);
            }
            Ok(Step::Unchanged) => {}
            Ok(Step::Timeout) => outbox.resend_if_due(),
            Err(Interruption::AccessLost) => {
                if !recover(&mut duplicator, &mut outbox, stop) {
                    return;
                }
            }
            Err(Interruption::Fatal(message)) => {
                eprintln!("capture: stopped, {message}");
                return;
            }
        }
    }
}

/// Opens the duplication again after Windows took it away, sending the last
/// frame again meanwhile. Returns false when capture has to stop.
fn recover(duplicator: &mut Duplicator, outbox: &mut Outbox, stop: &AtomicBool) -> bool {
    duplicator.duplication = None;
    let mut waiting = false;
    while !stop.load(Ordering::Relaxed) {
        match duplicator.reopen() {
            Reopen::Open => {
                if waiting {
                    println!("capture: the desktop is back");
                }
                return true;
            }
            Reopen::Unavailable(error) => {
                if !waiting {
                    println!(
                        "capture: the desktop is unavailable ({error}), as during a UAC prompt, \
                         on the lock screen or through a display change. Waiting for it"
                    );
                    waiting = true;
                }
                outbox.resend_if_due();
                std::thread::sleep(REOPEN_INTERVAL);
            }
            Reopen::Failed(message) => {
                eprintln!("capture: stopped, {message}");
                return false;
            }
        }
    }
    false
}

/// The adapter and output that show the primary desktop, whose top-left
/// corner sits at (0, 0), else the first output attached to the desktop.
fn primary_output() -> Result<(IDXGIAdapter1, IDXGIOutput)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("could not start DXGI")?;
    let mut fallback = None;
    for adapter_index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
            break;
        };
        for output_index in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            let Ok(desc) = (unsafe { output.GetDesc() }) else {
                continue;
            };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let corner = desc.DesktopCoordinates;
            if corner.left == 0 && corner.top == 0 {
                return Ok((adapter, output));
            }
            fallback.get_or_insert((adapter.clone(), output));
        }
    }
    fallback.context("found no display attached to the desktop")
}

/// A Direct3D 11 device on `adapter`, which Desktop Duplication requires to
/// be the adapter driving the display.
fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| {
        anyhow!(
            "could not open Direct3D 11 on the display adapter ({error}). Screen capture needs \
             it, and some virtual machines and Wine do not provide it"
        )
    })?;
    Ok((
        device.context("D3D11CreateDevice produced no device")?,
        context.context("D3D11CreateDevice produced no context")?,
    ))
}

/// Explains a failed `DuplicateOutput`, for the failures Windows documents.
fn refused(error: windows::core::Error) -> anyhow::Error {
    let code = error.code();
    let reason = if code == DXGI_ERROR_UNSUPPORTED {
        "Windows does not offer Desktop Duplication here. That happens in some remote desktop \
         sessions and virtual machines, and on the Microsoft Basic Display Adapter. Run \
         stereowire in the PC's own session, with its graphics driver installed"
    } else if code == E_ACCESSDENIED {
        "Windows refused access to the desktop, as it does on the lock screen, at a UAC prompt \
         and in some remote sessions. Unlock the PC and run stereowire from the signed-in desktop"
    } else if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
        "Too many programs are capturing the screen already, and Windows allows four. Close one \
         and try again"
    } else if code == DXGI_ERROR_SESSION_DISCONNECTED {
        "This Windows session is disconnected. Reconnect to it and try again"
    } else if code == E_NOTIMPL {
        "This system does not implement Desktop Duplication, as under Wine. The sender needs \
         Windows 8 or later"
    } else {
        return anyhow!("could not start Desktop Duplication ({error})");
    };
    anyhow!("could not start Desktop Duplication ({error}). {reason}")
}

/// Reads the pointer shape that came with the current frame. `None` for a
/// shape type Windows has not documented.
fn pointer_shape(
    duplication: &IDXGIOutputDuplication,
    size: u32,
) -> windows::core::Result<Option<CursorShape>> {
    let mut data = vec![0u8; size as usize];
    let mut required = 0u32;
    let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
    unsafe {
        duplication.GetFramePointerShape(size, data.as_mut_ptr().cast(), &mut required, &mut info)
    }?;
    let kind = if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        CursorKind::Monochrome
    } else if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
        CursorKind::Color
    } else if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
        CursorKind::MaskedColor
    } else {
        return Ok(None);
    };
    Ok(Some(CursorShape {
        kind,
        width: info.Width as usize,
        height: info.Height as usize,
        pitch: info.Pitch as usize,
        data,
    }))
}
