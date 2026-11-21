//! Windows' window: the system's own frame kept — its shadow and round corners, its snapping to
//! the screen's edges and out of them again, its resizing — with the title bar taken away, so
//! silli's top bar is the top of the window (the person, 2026-11-20: no title bar, the buttons in
//! the `+` menu as on the Mac).
//!
//! A window made without the system's frame loses what the frame does: dragged out of a snapped
//! place it kept the snapped size. So the frame stays and only its title bar goes: the window is
//! told that its whole area is the program's (`WM_NCCALCSIZE`). Moving and resizing are still
//! asked of Windows by silli's top bar and edges, and Windows then does them as for any window.
use std::ffi::c_void;

use iced::window::raw_window_handle::RawWindowHandle;

type Hwnd = *mut c_void;

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[link(name = "comctl32", kind = "raw-dylib")]
unsafe extern "system" {
    fn SetWindowSubclass(window: Hwnd, procedure: extern "system" fn(Hwnd, u32, usize, isize, usize, usize) -> isize, id: usize, data: usize) -> i32;
    fn DefSubclassProc(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
}

#[link(name = "user32", kind = "raw-dylib")]
unsafe extern "system" {
    fn SetWindowPos(window: Hwnd, after: Hwnd, x: i32, y: i32, width: i32, height: i32, flags: u32) -> i32;
    fn IsZoomed(window: Hwnd) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
}

const WM_NCCALCSIZE: u32 = 0x0083;
/// `SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED`.
const FRAME_CHANGED: u32 = 0x0002 | 0x0001 | 0x0004 | 0x0010 | 0x0020;
const SM_CXSIZEFRAME: i32 = 32;
const SM_CYSIZEFRAME: i32 = 33;
const SM_CXPADDEDBORDER: i32 = 92;

/// The window's own procedure, first: the title bar's room is not taken out of the window.
extern "system" fn frame(window: Hwnd, message: u32, wparam: usize, lparam: isize, _id: usize, _data: usize) -> isize {
    if message == WM_NCCALCSIZE && wparam != 0 {
        // SAFETY: with a true wparam, lparam points at the window's proposed rectangle (the first
        // of NCCALCSIZE_PARAMS); left as it is, the whole window is the program's
        unsafe {
            // a maximized window reaches past the screen by its frame's width on every side:
            // brought back in, or the top bar would be cut off
            if IsZoomed(window) != 0 {
                let rect = &mut *(lparam as *mut Rect);
                let pad = GetSystemMetrics(SM_CXPADDEDBORDER);
                let (x, y) = (GetSystemMetrics(SM_CXSIZEFRAME) + pad, GetSystemMetrics(SM_CYSIZEFRAME) + pad);
                rect.left += x;
                rect.right -= x;
                rect.top += y;
                rect.bottom -= y;
            }
        }
        return 0;
    }
    // SAFETY: every other message goes on to the window's own procedure
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

/// Once the window exists: take its title bar away, and have Windows lay its frame out again.
pub fn dress(window: &dyn iced::window::Window) {
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    let window = handle.hwnd.get() as Hwnd;
    // SAFETY: this live window's handle, on the thread that owns it (iced's window tasks run
    // there); the procedure lives as long as the program
    unsafe {
        SetWindowSubclass(window, frame, 1, 0);
        SetWindowPos(window, std::ptr::null_mut(), 0, 0, 0, 0, FRAME_CHANGED);
    }
}
