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
    dock_icon();
}

/// The Dock's icon: the herring on its sill, the shelf logo (the person, 2026-11-12), as
/// everywhere. Set while running until an app bundle carries it.
fn dock_icon() {
    const SIZE: u32 = 512;
    let Some(rgba) = crate::tray::rgba(crate::tray::TRAY_ICON, SIZE) else { return };
    let Some(image) = image::RgbaImage::from_raw(SIZE, SIZE, rgba) else { return };
    let mut png = std::io::Cursor::new(Vec::new());
    if image.write_to(&mut png, image::ImageFormat::Png).is_err() {
        return;
    }
    let png = png.into_inner();
    // SAFETY: on the main thread (the window's task); NSData copies the bytes, NSImage and
    // NSApplication are used with their declared argument and return types.
    unsafe {
        let data: *mut AnyObject = msg_send![
            objc2::class!(NSData),
            dataWithBytes: png.as_ptr().cast::<std::ffi::c_void>(),
            length: png.len()
        ];
        if data.is_null() {
            return;
        }
        let alloc: *mut AnyObject = msg_send![objc2::class!(NSImage), alloc];
        let image: *mut AnyObject = msg_send![alloc, initWithData: data];
        if image.is_null() {
            return;
        }
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let () = msg_send![app, setApplicationIconImage: image];
        let () = msg_send![image, release];
    }
}
