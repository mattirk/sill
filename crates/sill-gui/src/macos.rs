//! The Mac's window: the system's frame — its rounded corners, rim and shadow, its resizing —
//! with the title bar transparent under sill's top bar and the system's three buttons hidden
//! (the person, 2026-11-12: sill has no red/yellow/green buttons).
use iced::window::raw_window_handle::RawWindowHandle;
use objc2::msg_send;
use objc2::runtime::AnyObject;

/// The frame's settings as the window is made: sill's content fills the whole window.
pub fn platform_specific() -> iced::window::settings::PlatformSpecific {
    iced::window::settings::PlatformSpecific {
        title_hidden: true,
        titlebar_transparent: true,
        fullsize_content_view: true,
    }
}

/// Once the window exists (on the main thread, where iced's window tasks run): hide the system's
/// close, miniaturize and zoom buttons, and have the system keep the window's size and place
/// between runs.
pub fn dress(window: &dyn iced::window::Window) {
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
    let view: *mut AnyObject = handle.ns_view.as_ptr().cast();
    // SAFETY: the handle is this live window's NSView, on the main thread; the messages are
    // NSView's and NSWindow's own, with their declared argument and return types.
    unsafe {
        let ns_window: *mut AnyObject = msg_send![view, window];
        if ns_window.is_null() {
            return;
        }
        for kind in 0..3isize {
            let button: *mut AnyObject = msg_send![ns_window, standardWindowButton: kind];
            if !button.is_null() {
                let () = msg_send![button, setHidden: true];
            }
        }
        let name: *mut AnyObject =
            msg_send![objc2::class!(NSString), stringWithUTF8String: c"sill".as_ptr()];
        if !name.is_null() {
            let _: bool = msg_send![ns_window, setFrameAutosaveName: name];
        }
    }
}
