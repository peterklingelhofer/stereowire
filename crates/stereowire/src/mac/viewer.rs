//! The receiver's window, decoder, and menu bar, bundled behind one type so
//! the rest of the app needs no AppKit imports of its own. The Windows phase
//! mirrors this surface with its own window and decoder.

use anyhow::{Context, Result};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSEventMask};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use stereowire_proto::packet::Codec;

use super::display::Display;
use super::menu::Menu;

pub struct Viewer {
    mtm: MainThreadMarker,
    display: Display,
    menu: Menu,
}

impl Viewer {
    /// Opens the window and installs the menu. Must be called on the main thread.
    pub fn open(title: &str, width: i32, height: i32, show_latency: bool) -> Result<Self> {
        let mtm = MainThreadMarker::new().context("the receiver must run on the main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        // A plain binary is not launched the way a bundled app is, so AppKit
        // needs to be told to finish starting up before it will deliver
        // events or draw.
        app.finishLaunching();
        let display = Display::open(mtm, title, width, height)?;
        let menu = Menu::install(mtm, show_latency);
        Ok(Viewer { mtm, display, menu })
    }

    /// Drains pending UI events so the window stays responsive.
    pub fn pump(&mut self) {
        let app = NSApplication::sharedApplication(self.mtm);
        // `distantPast` makes this non-blocking: take what is queued and return.
        while let Some(event) = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&NSDate::distantPast()),
                NSDefaultRunLoopMode,
                true,
            )
        } {
            app.sendEvent(&event);
        }
    }

    pub fn set_params(&mut self, codec: Codec, params: &[Vec<u8>]) -> Result<()> {
        self.display.set_params(codec, params)
    }

    /// Decodes and shows one frame. Returns false if no format is known yet.
    pub fn present(&mut self, data: &[u8], pts_micros: u64) -> Result<bool> {
        self.display.present(data, pts_micros)
    }

    pub fn set_title(&self, title: &str) {
        self.display.set_title(title);
    }

    /// False once the window is closed or Quit was chosen.
    pub fn is_open(&self) -> bool {
        self.display.is_open() && !self.menu.should_quit()
    }

    /// Whether the latency readout is enabled. Also syncs the menu's check mark.
    pub fn show_latency(&self) -> bool {
        self.menu.sync_check_mark();
        self.menu.show_latency()
    }
}
