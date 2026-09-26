//! Win32 window: class registration, message pump, and the state a viewer
//! reads back (open, the latency toggle, client size, a pending resize).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: PCWSTR = windows::core::w!("stereowire_receiver");

/// State reached through `GWLP_USERDATA`. The window procedure and the
/// `Window` methods run on the same thread (nothing calls `pump` from
/// anywhere else), but they touch this through separate `&State` borrows
/// derived from a raw pointer, so each field is its own atomic rather than
/// requiring one exclusive borrow of the whole struct.
struct State {
    open: AtomicBool,
    show_latency: AtomicBool,
    resized: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
}

pub struct Window {
    hwnd: HWND,
    state: *mut State,
}

impl Window {
    /// Opens the window. `width`/`height` are the incoming pixel size; the
    /// client area starts at half that, scaled for the system DPI and
    /// clamped to the primary work area, the same as the Mac receiver
    /// opening at half a Retina capture's size.
    pub fn open(title: &str, width: i32, height: i32, show_latency: bool) -> Result<Self> {
        unsafe {
            // Failure just means the window is DPI-virtualized; not fatal.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        register_class()?;

        let scale = f64::from(unsafe { GetDpiForSystem() }) / 96.0;
        let mut client_width = ((f64::from(width) / 2.0) * scale).round() as i32;
        let mut client_height = ((f64::from(height) / 2.0) * scale).round() as i32;

        let mut work_area = RECT::default();
        let got_work_area = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work_area as *mut RECT as *mut c_void),
                Default::default(),
            )
        }
        .is_ok();
        if got_work_area {
            client_width = client_width.min(work_area.right - work_area.left).max(1);
            client_height = client_height.min(work_area.bottom - work_area.top).max(1);
        }

        let mut rect = RECT {
            left: 0,
            top: 0,
            right: client_width,
            bottom: client_height,
        };
        unsafe { AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false) }
            .context("AdjustWindowRect failed")?;

        let state = Box::into_raw(Box::new(State {
            open: AtomicBool::new(true),
            show_latency: AtomicBool::new(show_latency),
            resized: AtomicBool::new(false),
            width: AtomicU32::new(client_width.max(0) as u32),
            height: AtomicU32::new(client_height.max(0) as u32),
        }));

        let wide_title = to_wide(title);
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                CLASS_NAME,
                PCWSTR(wide_title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance()?.into()),
                None,
            )
        };
        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(e) => {
                drop(unsafe { Box::from_raw(state) });
                return Err(e).context("CreateWindowExW failed");
            }
        };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        Ok(Window { hwnd, state })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Drains pending messages so the window stays responsive.
    pub fn pump(&mut self) {
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub fn set_title(&self, title: &str) {
        let wide = to_wide(title);
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(wide.as_ptr()));
        }
    }

    /// False once the window has been closed.
    pub fn is_open(&self) -> bool {
        self.state().open.load(Ordering::Relaxed)
    }

    pub fn show_latency(&self) -> bool {
        self.state().show_latency.load(Ordering::Relaxed)
    }

    /// The new client size, if it has changed since the last call.
    pub fn take_resize(&self) -> Option<(u32, u32)> {
        let state = self.state();
        if state.resized.swap(false, Ordering::Relaxed) {
            Some((
                state.width.load(Ordering::Relaxed),
                state.height.load(Ordering::Relaxed),
            ))
        } else {
            None
        }
    }

    fn state(&self) -> &State {
        // SAFETY: the pointer was allocated in `open` and only ever freed in
        // `Drop`, which cannot run while `self` is still borrowed.
        unsafe { &*self.state }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            drop(Box::from_raw(self.state));
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn instance() -> Result<HMODULE> {
    unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")
}

static CLASS_REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();

fn register_class() -> Result<()> {
    CLASS_REGISTERED
        .get_or_init(|| {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance().map(Into::into).unwrap_or_default(),
                hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            if unsafe { RegisterClassExW(&class) } == 0 {
                Err(windows::core::Error::from_thread().to_string())
            } else {
                Ok(())
            }
        })
        .clone()
        .map_err(|message| anyhow::anyhow!("RegisterClassExW failed: {message}"))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    match msg {
        WM_SIZE => {
            if let Some(state) = unsafe { state_ptr.as_ref() } {
                let width = (lparam.0 as u32) & 0xFFFF;
                let height = ((lparam.0 as u32) >> 16) & 0xFFFF;
                state.width.store(width, Ordering::Relaxed);
                state.height.store(height, Ordering::Relaxed);
                state.resized.store(true, Ordering::Relaxed);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(state) = unsafe { state_ptr.as_ref() } {
                state.open.store(false, Ordering::Relaxed);
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(state) = unsafe { state_ptr.as_ref() } {
                state.open.store(false, Ordering::Relaxed);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 == b'L' as usize {
                let ctrl_down = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                if ctrl_down {
                    if let Some(state) = unsafe { state_ptr.as_ref() } {
                        let current = state.show_latency.load(Ordering::Relaxed);
                        state.show_latency.store(!current, Ordering::Relaxed);
                    }
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
