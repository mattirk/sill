//! The Mac's window: the system's frame — its rounded corners, rim and shadow, its resizing —
//! with the title bar transparent under silli's top bar and the system's three buttons hidden
//! (the person, 2026-11-12: silli has no red/yellow/green buttons).
use iced::window::raw_window_handle::RawWindowHandle;
use objc2::msg_send;
use objc2::runtime::AnyObject;

/// The frame's settings as the window is made: silli's content fills the whole window.
pub fn platform_specific() -> iced::window::settings::PlatformSpecific {
    iced::window::settings::PlatformSpecific { title_hidden: true, titlebar_transparent: true, fullsize_content_view: true }
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
        let name: *mut AnyObject = msg_send![objc2::class!(NSString), stringWithUTF8String: c"silli".as_ptr()];
        if !name.is_null() {
            let _: bool = msg_send![ns_window, setFrameAutosaveName: name];
        }
    }
    dock_icon();
}

/// The Dock's icon: the herring on its silli, the shelf logo (the person, 2026-11-12), as
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

/// Whether silli is the active application: the one the keyboard goes to.
pub fn is_active() -> bool {
    // SAFETY: on the main thread (the window's update); `isActive` returns a BOOL.
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        !app.is_null() && msg_send![app, isActive]
    }
}

/// Hide the application, as Cmd+H does: the application that was active before becomes active
/// again, which taking the window off the screen alone does not do (silli stayed in the menu bar
/// with no window; the person, 2026-11-19). Any way of showing the window unhides it
/// ([`unhide`]), and so does a click on its Dock icon.
pub fn hide() {
    // SAFETY: on the main thread (the window's update); `hide:` takes a sender, which may be nil.
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        if !app.is_null() {
            let () = msg_send![app, hide: std::ptr::null_mut::<AnyObject>()];
        }
    }
}

/// Undo [`hide`], before the window is shown and made active.
pub fn unhide() {
    // SAFETY: as `hide`; `unhideWithoutActivation` takes nothing.
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        if !app.is_null() {
            let () = msg_send![app, unhideWithoutActivation];
        }
    }
}

/// Before this window starts another silli window (Open on a box): let it take the front. Since
/// macOS 14 activation is cooperative — an app comes forward only when the active one yields — so
/// this window, active from the press, yields to every process of its own bundle: the new window,
/// or one already open on that box that is asked to show itself. Without a bundle (a development
/// build) or before macOS 14 there is nothing to yield; the new window then activates itself.
pub fn yield_activation() {
    // SAFETY: on the main thread (the window's update); NSApplication, NSBundle and NSString are
    // used with their declared argument and return types, and the selector is checked first.
    unsafe {
        let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
        let bundle: *mut AnyObject = msg_send![objc2::class!(NSBundle), mainBundle];
        if app.is_null() || bundle.is_null() {
            return;
        }
        let id: *mut AnyObject = msg_send![bundle, bundleIdentifier];
        let yields: bool = msg_send![app, respondsToSelector: objc2::sel!(yieldActivationToApplicationWithBundleIdentifier:)];
        if !id.is_null() && yields {
            let () = msg_send![app, yieldActivationToApplicationWithBundleIdentifier: id];
        }
    }
}

/// Bring the silli window of process `pid` to the front once it runs, from this (active) one: on
/// a thread of its own, looking for it for up to five seconds. A process that never becomes an
/// application (a second start that hands over to a window already open, and leaves) is not
/// found, and nothing happens.
pub fn activate_when_running(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else { return };
    let _ = std::thread::Builder::new().name("silli-gui-raise".into()).spawn(move || {
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            // SAFETY: NSRunningApplication may be used from any thread; the messages are its own,
            // with their declared argument and return types, inside an autorelease pool.
            let done = objc2::rc::autoreleasepool(|_| unsafe {
                let app: *mut AnyObject = msg_send![objc2::class!(NSRunningApplication), runningApplicationWithProcessIdentifier: pid];
                if app.is_null() {
                    return false;
                }
                let launched: bool = msg_send![app, isFinishedLaunching];
                if !launched {
                    return false;
                }
                // NSApplicationActivateAllWindows
                let _: bool = msg_send![app, activateWithOptions: 1usize];
                msg_send![app, isActive]
            });
            if done {
                return;
            }
        }
    });
}
