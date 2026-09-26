//! The receiver's window, decoder, and renderer, bundled behind one type with
//! the same surface as `mac::viewer::Viewer` so `receive.rs` needs no
//! platform-specific code of its own.

use anyhow::Result;
use stereowire_proto::packet::Codec;

use super::decoder::Decoder;
use super::render::Renderer;
use super::window::Window;

pub struct Viewer {
    window: Window,
    renderer: Renderer,
    decoder: Option<Decoder>,
    codec: Option<Codec>,
    params: Vec<Vec<u8>>,
}

impl Viewer {
    pub fn open(title: &str, width: i32, height: i32, show_latency: bool) -> Result<Self> {
        let window = Window::open(title, width, height, show_latency)?;
        let renderer = Renderer::new(window.hwnd())?;
        Ok(Viewer {
            window,
            renderer,
            decoder: None,
            codec: None,
            params: Vec::new(),
        })
    }

    /// Drains pending window messages so the window stays responsive.
    pub fn pump(&mut self) {
        self.window.pump();
    }

    /// Rebuilds the decoder whenever the codec or parameter sets actually
    /// change. Every frame carries a copy of the parameter sets, so this
    /// must not rebuild on every call, only when something is new.
    pub fn set_params(&mut self, codec: Codec, params: &[Vec<u8>]) -> Result<()> {
        if params.is_empty() || (Some(codec) == self.codec && params == self.params) {
            return Ok(());
        }
        self.decoder = Some(Decoder::new(codec, params)?);
        self.codec = Some(codec);
        self.params = params.to_vec();
        Ok(())
    }

    /// Decodes and shows one frame. Returns `false` while no decoder exists
    /// yet, or while this call's input produced no frame to show.
    ///
    /// The a/v offset the receiver prints is measured against the frame
    /// handed to this call, so it reads a few frames early by whatever
    /// latency the decoder itself is holding onto internally.
    pub fn present(&mut self, data: &[u8], pts_micros: u64) -> Result<bool> {
        if self.decoder.is_none() {
            return Ok(false);
        }

        if let Some((width, height)) = self.window.take_resize() {
            self.renderer.resize(width, height)?;
        }

        let decoded = self
            .decoder
            .as_mut()
            .map(|decoder| decoder.decode(data, pts_micros));
        if let Some(Err(error)) = decoded {
            // A frame the decoder rejects costs that frame, never the session.
            // Clearing the stored parameter sets makes the next frame rebuild
            // the decoder, and the depacketizer only releases frames from a
            // keyframe onwards after a gap, so the new one starts clean
            eprintln!("video: decode failed, restarting the decoder: {error}");
            self.decoder = None;
            self.params.clear();
            return Ok(false);
        }
        // A gap that just got filled can release more than one frame at
        // once; only the newest is worth showing on a live view, so the
        // rest are dropped here rather than queuing up a backlog.
        let newest = self
            .decoder
            .as_mut()
            .and_then(|decoder| decoder.drain().last());
        match newest {
            Some(frame) => {
                self.renderer.draw(&frame)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    /// False once the window is closed.
    pub fn is_open(&self) -> bool {
        self.window.is_open()
    }

    pub fn show_latency(&self) -> bool {
        self.window.show_latency()
    }
}
