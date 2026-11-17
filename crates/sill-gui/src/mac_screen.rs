//! The Mac's screen for sill-mac (docs/sill-mac.md): its displays and windows from Core
//! Graphics, and pictures by macOS's own `screencapture`, started by Sill so that macOS's Screen
//! Recording consent is Sill's. Whether Sill has that consent is asked without a prompt
//! (`allowed`); the prompt comes only when the person grants Mac viewing (`ask`).
use std::ffi::{CStr, c_void};
use std::path::PathBuf;

use objc2::msg_send;
use objc2::runtime::AnyObject;
use serde::Serialize;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> *const c_void;
    fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
    fn CGDisplayModeRelease(mode: *const c_void);
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *const c_void;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

/// One of the Mac's displays. Positions and sizes are in points, the desktop's coordinates
/// (the main display's top left is 0,0; others may be negative); `scale` is pixels per point.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Display {
    pub id: u32,
    pub main: bool,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub scale: f64,
}

/// A window on the Mac's screen, as the window server lists it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Window {
    pub id: u32,
    pub app: String,
    /// Empty when macOS does not show titles to Sill (no Screen Recording consent).
    pub title: String,
    pub pid: i64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// macOS lets Sill record the screen. Asks without a prompt.
pub fn allowed() -> bool {
    // SAFETY: no arguments, no side effects
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Ask macOS for the Screen Recording consent: its prompt, or its settings, for the person.
pub fn ask() -> bool {
    // SAFETY: no arguments; macOS shows its own dialog
    unsafe { CGRequestScreenCaptureAccess() }
}

/// What to tell the agent and the person when the consent is missing.
pub const NOT_ALLOWED: &str = "macOS does not allow Sill to record the screen: the person turns Sill on in System Settings → Privacy & Security → Screen & System Audio Recording (and starts Sill again)";

pub fn displays() -> Vec<Display> {
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    // SAFETY: a buffer of 32 ids and its size
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    // SAFETY: no arguments
    let main = unsafe { CGMainDisplayID() };
    ids[..count as usize]
        .iter()
        .map(|&id| {
            // SAFETY: an active display's id; its mode is released after reading. The mode's pixels,
            // not CGDisplayPixelsWide's: that is the size in points on a Retina display.
            let (bounds, pw, ph) = unsafe {
                let mode = CGDisplayCopyDisplayMode(id);
                let pixels = if mode.is_null() {
                    (0, 0)
                } else {
                    let p = (CGDisplayModeGetPixelWidth(mode), CGDisplayModeGetPixelHeight(mode));
                    CGDisplayModeRelease(mode);
                    p
                };
                (CGDisplayBounds(id), pixels.0, pixels.1)
            };
            Display {
                id,
                main: id == main,
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
                pixel_width: pw,
                pixel_height: ph,
                scale: if bounds.size.width > 0.0 { pw as f64 / bounds.size.width } else { 1.0 },
            }
        })
        .collect()
}

/// The windows on screen that belong to apps (layer 0: no menu bar, Dock or desktop), front first.
pub fn windows() -> Vec<Window> {
    const ON_SCREEN_ONLY: u32 = 1;
    const EXCLUDE_DESKTOP: u32 = 16;
    // SAFETY: a CFArray of CFDictionary, toll-free bridged to NSArray of NSDictionary; released
    // below; every value is checked for its kind before use
    unsafe {
        let list = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if list.is_null() {
            return Vec::new();
        }
        let array = list as *mut AnyObject;
        let count: usize = msg_send![array, count];
        let mut out = Vec::new();
        for i in 0..count {
            let entry: *mut AnyObject = msg_send![array, objectAtIndex: i];
            let number = |key: &CStr| -> Option<f64> {
                let value = value(entry, key)?;
                Some(msg_send![value, doubleValue])
            };
            if number(c"kCGWindowLayer") != Some(0.0) {
                continue;
            }
            let Some(id) = number(c"kCGWindowNumber") else { continue };
            let bounds = value(entry, c"kCGWindowBounds");
            let field = |key: &CStr| -> f64 { bounds.and_then(|b| value(b, key)).map(|v| msg_send![v, doubleValue]).unwrap_or(0.0) };
            out.push(Window {
                id: id as u32,
                app: string(value(entry, c"kCGWindowOwnerName")),
                title: string(value(entry, c"kCGWindowName")),
                pid: number(c"kCGWindowOwnerPID").unwrap_or(0.0) as i64,
                x: field(c"X"),
                y: field(c"Y"),
                width: field(c"Width"),
                height: field(c"Height"),
            });
        }
        CFRelease(list);
        out
    }
}

/// `dictionary[key]`, when there.
unsafe fn value(dictionary: *mut AnyObject, key: &CStr) -> Option<*mut AnyObject> {
    // SAFETY: the caller's dictionary; NSString from a C string
    unsafe {
        let key: *mut AnyObject = msg_send![objc2::class!(NSString), stringWithUTF8String: key.as_ptr()];
        let value: *mut AnyObject = msg_send![dictionary, objectForKey: key];
        (!value.is_null()).then_some(value)
    }
}

/// An NSString's text, or empty.
fn string(value: Option<*mut AnyObject>) -> String {
    let Some(value) = value else { return String::new() };
    // SAFETY: a string from the window server's dictionary; UTF8String lives with it
    unsafe {
        let text: *const std::ffi::c_char = msg_send![value, UTF8String];
        if text.is_null() { String::new() } else { CStr::from_ptr(text).to_string_lossy().into_owned() }
    }
}

/// A picture of the display `id` or the window `id`: PNG bytes, and its size in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Display(u32),
    Window(u32),
}

pub fn capture(target: &Target) -> Result<(Vec<u8>, u32, u32), String> {
    if !allowed() {
        return Err(NOT_ALLOWED.into());
    }
    let file = scratch()?;
    let mut command = std::process::Command::new("/usr/sbin/screencapture");
    // -x: no sound; -t png; -o: a window without its shadow
    command.args(["-x", "-t", "png"]);
    match target {
        Target::Display(id) => {
            // screencapture numbers the displays 1, 2… in the order the window server lists them
            let index = displays().iter().position(|d| d.id == *id).ok_or_else(|| format!("the Mac has no display {id}: `sill-mac displays` lists them"))?;
            command.arg("-D").arg((index + 1).to_string());
        }
        Target::Window(id) => {
            if !windows().iter().any(|w| w.id == *id) {
                return Err(format!("the Mac has no window {id} on screen: `sill-mac windows` lists them"));
            }
            command.arg("-o").arg("-l").arg(id.to_string());
        }
    }
    let status = command.arg(&file).status().map_err(|e| format!("screencapture: {e}"))?;
    let png = std::fs::read(&file).unwrap_or_default();
    let _ = std::fs::remove_file(&file);
    if !status.success() || png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(format!("macOS could not take the picture ({status})"));
    }
    // the PNG's own size: its header's width and height
    let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Ok((png, width, height))
}

/// A private file for one picture.
fn scratch() -> Result<PathBuf, String> {
    let dir = sill_proto::runtime_dir().map_err(|e| e.to_string())?.join("mac-pictures");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(dir.join(format!("{}-{n}.png", std::process::id())))
}

#[cfg(test)]
mod tests {
    /// Reads this Mac's real screen: `cargo test -p sill-gui mac_screen -- --ignored`. Needs the
    /// Screen Recording consent for whatever started the test (the terminal).
    #[test]
    #[ignore]
    fn this_macs_displays_windows_and_a_picture() {
        let displays = super::displays();
        assert!(displays.iter().any(|d| d.main), "{displays:?}");
        let main = displays.iter().find(|d| d.main).unwrap();
        assert!(main.pixel_width as f64 >= main.width && main.scale >= 1.0, "{main:?}");
        let windows = super::windows();
        assert!(!windows.is_empty());
        eprintln!("{} displays, {} windows; main {main:?}", displays.len(), windows.len());
        let (png, width, height) = super::capture(&super::Target::Display(main.id)).unwrap();
        assert_eq!((width as usize, height as usize), (main.pixel_width, main.pixel_height));
        assert!(png.len() > 1000);
        let window = &windows[0];
        let (_, w, h) = super::capture(&super::Target::Window(window.id)).unwrap();
        eprintln!("window {} ({}): {w}x{h} pixels for {}x{} points", window.id, window.app, window.width, window.height);
        assert!(super::capture(&super::Target::Window(1)).is_err(), "an id not on screen is refused");
    }
}
