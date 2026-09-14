//! The receiver's menu bar.
//!
//! Without a main menu a plain binary gets no standard shortcuts at all, so
//! this also supplies Quit. Quit routes through a flag rather than AppKit's
//! `terminate:` so the run loop can shut down cleanly and finish any recording.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuItem,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSObject, NSObjectProtocol, NSString};

pub struct Ivars {
    show_latency: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

define_class!(
    // SAFETY:
    // - NSObject imposes no subclassing requirements.
    // - MenuTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "StereoWireMenuTarget"]
    #[ivars = Ivars]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(toggleLatency:))]
        fn toggle_latency(&self, _sender: Option<&AnyObject>) {
            let show = &self.ivars().show_latency;
            show.store(!show.load(Ordering::Relaxed), Ordering::Relaxed);
        }

        #[unsafe(method(quitStereoWire:))]
        fn quit_stereo_wire(&self, _sender: Option<&AnyObject>) {
            self.ivars().quit.store(true, Ordering::Relaxed);
        }
    }
);

impl MenuTarget {
    fn new(
        mtm: MainThreadMarker,
        show_latency: Arc<AtomicBool>,
        quit: Arc<AtomicBool>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { show_latency, quit });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Menu {
    pub show_latency: Arc<AtomicBool>,
    pub quit: Arc<AtomicBool>,
    latency_item: Retained<NSMenuItem>,
    /// The menu items hold this only weakly, so it has to be kept alive here.
    _target: Retained<MenuTarget>,
}

impl Menu {
    /// Installs the menu bar. `show_latency` seeds the toggle's initial state.
    pub fn install(mtm: MainThreadMarker, show_latency: bool) -> Self {
        let app = NSApplication::sharedApplication(mtm);
        let show_latency = Arc::new(AtomicBool::new(show_latency));
        let quit = Arc::new(AtomicBool::new(false));
        let target = MenuTarget::new(mtm, show_latency.clone(), quit.clone());

        let menubar = NSMenu::new(mtm);

        // The first menu is the application menu, whatever its title.
        let app_item = NSMenuItem::new(mtm);
        let app_menu = NSMenu::new(mtm);
        let quit_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Quit StereoWire"),
                Some(sel!(quitStereoWire:)),
                ns_string!("q"),
            )
        };
        unsafe { quit_item.setTarget(Some(&target)) };
        app_menu.addItem(&quit_item);
        app_item.setSubmenu(Some(&app_menu));
        menubar.addItem(&app_item);

        let view_item = NSMenuItem::new(mtm);
        let view_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("View"));
        let latency_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                ns_string!("Show Latency in Title"),
                Some(sel!(toggleLatency:)),
                ns_string!("l"),
            )
        };
        unsafe { latency_item.setTarget(Some(&target)) };
        view_menu.addItem(&latency_item);
        view_item.setSubmenu(Some(&view_menu));
        menubar.addItem(&view_item);

        app.setMainMenu(Some(&menubar));

        let menu = Menu {
            show_latency,
            quit,
            latency_item,
            _target: target,
        };
        menu.sync_check_mark();
        menu
    }

    pub fn show_latency(&self) -> bool {
        self.show_latency.load(Ordering::Relaxed)
    }

    pub fn should_quit(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    /// Mirrors the toggle's state onto its check mark.
    pub fn sync_check_mark(&self) {
        let state = if self.show_latency() {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        };
        self.latency_item.setState(state);
    }
}

/// Convenience for building the window title.
pub fn title(base: &str, detail: Option<&str>) -> Retained<NSString> {
    match detail {
        Some(detail) => NSString::from_str(&format!("{base}  ·  {detail}")),
        None => NSString::from_str(base),
    }
}
