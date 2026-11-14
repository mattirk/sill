//! The Mac's mouse and keyboard for sill-mac (docs/sill-mac.md, slice 2): actions aimed in a
//! picture's pixels, posted as Core Graphics events. macOS asks the person for its Accessibility
//! consent when they grant Mac control (`ask`); without it nothing is posted.
//!
//! The person's own input wins: a request is refused while the person uses the mouse or keyboard,
//! and before every action, if anyone but Sill used them since the request began, it stops there
//! and says how far it got. Sill marks the events it posts and a listen-only event tap (`watch`)
//! sees the rest (security review, 2026-11-13, finding 6: the earlier check alone, the system's
//! idle time against Sill's own last event, missed the person while Sill posted often).
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> *const c_void;
    fn CGEventCreateMouseEvent(source: *const c_void, kind: u32, at: Point, button: u32) -> *const c_void;
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> *const c_void;
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheels: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *const c_void;
    fn CGEventSetFlags(event: *const c_void, flags: u64);
    fn CGEventSetIntegerValueField(event: *const c_void, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(event: *const c_void, length: usize, text: *const u16);
    fn CGEventPost(tap: u32, event: *const c_void);
    fn CGEventSourceSecondsSinceLastEventType(state: i32, kind: u32) -> f64;
    fn CGEventGetIntegerValueField(event: *const c_void, field: u32) -> i64;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events: u64,
        callback: extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
        info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGPreflightListenEventAccess() -> bool;
}
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFMachPortCreateRunLoopSource(allocator: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    static kCFRunLoopCommonModes: *const c_void;
}

const HID_TAP: u32 = 0;
const SOURCE_HID_SYSTEM: i32 = 1;
const ANY_INPUT: u32 = !0;
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const CLICK_STATE: u32 = 1;
const SCROLL_PIXELS: u32 = 0;
const SHIFT: u64 = 0x20000;
const CONTROL: u64 = 0x40000;
const OPTION: u64 = 0x80000;
const COMMAND: u64 = 0x100000;
const FUNCTION: u64 = 0x800000;
/// Event fields: the poster's own 64 bits, and the posting process.
const SOURCE_USER_DATA: u32 = 42;
const SOURCE_PID: u32 = 41;
/// What Sill puts in the user data of every event it posts.
const MARK: i64 = i64::from_be_bytes(*b"sill-mac");
const SESSION_TAP: u32 = 1;
const HEAD_INSERT: u32 = 0;
const LISTEN_ONLY: u32 = 1;
/// The person's mouse: buttons, moves, drags, scrolling. Watching it needs no consent.
const MOUSE_EVENTS: u64 =
    (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 22) | (1 << 25) | (1 << 26) | (1 << 27);
/// The person's keyboard: keys and modifiers. Watching it needs macOS's Input Monitoring consent.
const KEY_EVENTS: u64 = (1 << 10) | (1 << 11) | (1 << 12);
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

/// macOS trusts Sill with Accessibility, which posting mouse and keyboard events needs. Asks
/// without a prompt.
pub fn allowed() -> bool {
    // SAFETY: no arguments, no side effects
    unsafe { AXIsProcessTrusted() }
}

/// Ask macOS for the consent: its prompt ("Sill would like to control this computer using
/// accessibility features", with a button to System Settings). Core Graphics's own request
/// showed nothing (the person, 2026-11-12); the Accessibility API's check with its prompt
/// option is what shows it.
pub fn ask() -> bool {
    // SAFETY: an NSDictionary (toll-free a CFDictionary) of the prompt option, alive across the call
    unsafe {
        let key: *mut objc2::runtime::AnyObject = objc2::msg_send![
            objc2::class!(NSString),
            stringWithUTF8String: c"AXTrustedCheckOptionPrompt".as_ptr()
        ];
        let yes: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSNumber), numberWithBool: true];
        let options: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSDictionary), dictionaryWithObject: yes, forKey: key];
        AXIsProcessTrustedWithOptions(options as *const c_void)
    }
}

pub const NOT_ALLOWED: &str = "macOS does not allow Sill to control the mouse and keyboard: the person turns Sill on in System Settings → Privacy & Security → Accessibility";

/// The agent is driving the Mac's input: the menu-bar herring shows it.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// When Sill last posted an event: input after it is the person's.
static LAST_POSTED: Mutex<Option<Instant>> = Mutex::new(None);
/// One session's input at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// How long the person's own input holds the agent's off.
const PERSON_QUIET: f64 = 1.5;

/// The watch's clock: milliseconds since the first use, from 1 (0 is "never").
fn clock_ms() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// When the watch last saw the person's mouse or keyboard, by `clock_ms`; 0 for never.
static PERSON_LAST: AtomicU64 = AtomicU64::new(0);
/// The event taps, once they run: the mouse's and the keyboard's.
static TAPS: [AtomicPtr<c_void>; 2] = [AtomicPtr::new(std::ptr::null_mut()), AtomicPtr::new(std::ptr::null_mut())];
static WATCH_STARTING: AtomicBool = AtomicBool::new(false);

/// Sill posted this event itself: its mark, or this process as the poster.
fn ours(user_data: i64, pid: i64) -> bool {
    user_data == MARK || pid == i64::from(std::process::id())
}

extern "C" fn on_event(_proxy: *mut c_void, kind: u32, event: *mut c_void, _info: *mut c_void) -> *mut c_void {
    match kind {
        // macOS switched the tap off (a slow callback, secure input): on again
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => {
            for tap in &TAPS {
                let tap = tap.load(Ordering::Acquire);
                if !tap.is_null() {
                    // SAFETY: a tap made in `tap`, never released
                    unsafe { CGEventTapEnable(tap, true) };
                }
            }
        }
        _ if !event.is_null() => {
            // SAFETY: the event macOS hands the tap, alive for the call
            let (user_data, pid) = unsafe {
                (CGEventGetIntegerValueField(event, SOURCE_USER_DATA), CGEventGetIntegerValueField(event, SOURCE_PID))
            };
            if !ours(user_data, pid) {
                PERSON_LAST.store(clock_ms(), Ordering::Release);
            }
        }
        _ => {}
    }
    // a listen-only tap: what is returned changes nothing
    event
}

/// Watch the mouse and keyboard for the person's own input: listen-only event taps on threads of
/// their own, each started once. The mouse needs no consent. The keyboard needs Input Monitoring,
/// looked at without a prompt: asking for a consent is the person's to do, never an agent
/// request's. What is not watched is judged by the system's idle time, as before; a later call
/// starts what it can then. Whether the mouse is watched.
pub fn watch() -> bool {
    if WATCH_STARTING.swap(true, Ordering::AcqRel) {
        return !TAPS[0].load(Ordering::Acquire).is_null();
    }
    if TAPS[0].load(Ordering::Acquire).is_null() && !tap(0, MOUSE_EVENTS) {
        eprintln!("sill-gui: the person's mouse is judged by idle time only (no event tap)");
    }
    // SAFETY: a query without side effects or a prompt
    if TAPS[1].load(Ordering::Acquire).is_null() && unsafe { CGPreflightListenEventAccess() } && !tap(1, KEY_EVENTS) {
        eprintln!("sill-gui: the person's keyboard is judged by idle time only (no event tap)");
    }
    WATCH_STARTING.store(false, Ordering::Release);
    !TAPS[0].load(Ordering::Acquire).is_null()
}

/// Start a listen-only tap for `events` as `TAPS[slot]`; whether it runs.
fn tap(slot: usize, events: u64) -> bool {
    let (started, answer) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("sill-gui-person-input".into()).spawn(move || {
        // SAFETY: a tap with a callback that only reads its event, its run loop source added to
        // this thread's own run loop, which then runs for the rest of the process
        unsafe {
            let tap = CGEventTapCreate(SESSION_TAP, HEAD_INSERT, LISTEN_ONLY, events, on_event, std::ptr::null_mut());
            if tap.is_null() {
                let _ = started.send(false);
                return;
            }
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            if source.is_null() {
                CFRelease(tap);
                let _ = started.send(false);
                return;
            }
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            TAPS[slot].store(tap, Ordering::Release);
            let _ = started.send(true);
            CFRunLoopRun();
        }
    });
    spawned.is_ok() && answer.recv_timeout(Duration::from_secs(2)).unwrap_or(false)
}

/// A moment to compare the person's input with: a request's start.
#[derive(Clone, Copy)]
pub struct Since(u64);

pub fn since_now() -> Since {
    Since(clock_ms())
}

/// By the system's idle time: the latest input is recent and clearly later than Sill's own last
/// event. Misses the person while Sill posts often; the watch does not.
fn idle_says_person() -> bool {
    // SAFETY: a query without side effects
    let idle = unsafe { CGEventSourceSecondsSinceLastEventType(SOURCE_HID_SYSTEM, ANY_INPUT) };
    if idle >= PERSON_QUIET {
        return false;
    }
    let ours = LAST_POSTED.lock().unwrap().map(|at| at.elapsed().as_secs_f64());
    match ours {
        Some(ours) => idle + 0.25 < ours,
        None => true,
    }
}

/// The person used the mouse or keyboard in the last moments (not Sill's own events): a new
/// request waits.
pub fn person_active() -> bool {
    let last = PERSON_LAST.load(Ordering::Acquire);
    let recent = last != 0 && clock_ms().saturating_sub(last) < (PERSON_QUIET * 1000.0) as u64;
    recent || idle_says_person()
}

/// The person used the mouse or keyboard since `since` (not Sill's own events): the rest of a
/// request stops.
pub fn person_since(since: Since) -> bool {
    PERSON_LAST.load(Ordering::Acquire) > since.0 || idle_says_person()
}

/// Sleep `ms` (an agent's wait), ending early if the person uses the mouse or keyboard.
pub fn wait(ms: u64, since: Since) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until && !person_since(since) {
        std::thread::sleep(until.saturating_duration_since(Instant::now()).min(Duration::from_millis(50)));
    }
}

/// Holds the input for one request; the herring shows it while held.
pub struct Driving<'a> {
    _one: std::sync::MutexGuard<'a, ()>,
}

pub fn drive() -> Option<Driving<'static>> {
    let one = ONE_AT_A_TIME.try_lock().ok()?;
    ACTIVE.store(true, Ordering::Relaxed);
    Some(Driving { _one: one })
}

impl Drop for Driving<'_> {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Relaxed);
    }
}

fn post(event: *const c_void) {
    if event.is_null() {
        return;
    }
    // SAFETY: an event just made; marked as Sill's for the watch, released after posting
    unsafe {
        CGEventSetIntegerValueField(event, SOURCE_USER_DATA, MARK);
        CGEventPost(HID_TAP, event);
        CFRelease(event);
    }
    *LAST_POSTED.lock().unwrap() = Some(Instant::now());
}

fn mouse(kind: u32, at: Point, button: u32, clicks: i64) {
    // SAFETY: a mouse event with no source (the system's); fields set before posting
    unsafe {
        let event = CGEventCreateMouseEvent(std::ptr::null(), kind, at, button);
        if !event.is_null() && clicks > 0 {
            CGEventSetIntegerValueField(event, CLICK_STATE, clicks);
        }
        post(event);
    }
}

pub fn move_to(at: Point) {
    mouse(MOVED, at, 0, 0);
}

/// A click (or a double click) at `at`, the button pressed and released.
pub fn click(at: Point, right: bool, count: u32) {
    let (down, up, button) = if right { (RIGHT_DOWN, RIGHT_UP, 1) } else { (LEFT_DOWN, LEFT_UP, 0) };
    move_to(at);
    for n in 1..=count.clamp(1, 3) {
        mouse(down, at, button, n as i64);
        std::thread::sleep(Duration::from_millis(15));
        mouse(up, at, button, n as i64);
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// The left button pressed at `from`, moved to `to` in steps, released there.
pub fn drag(from: Point, to: Point) {
    move_to(from);
    mouse(LEFT_DOWN, from, 0, 1);
    const STEPS: u32 = 12;
    for i in 1..=STEPS {
        let t = f64::from(i) / f64::from(STEPS);
        let at = Point { x: from.x + (to.x - from.x) * t, y: from.y + (to.y - from.y) * t };
        mouse(LEFT_DRAGGED, at, 0, 1);
        std::thread::sleep(Duration::from_millis(12));
    }
    mouse(LEFT_UP, to, 0, 1);
}

/// Scroll by `dy`, `dx` pixels at `at`: positive goes down / right through the content.
pub fn scroll(at: Point, dy: f64, dx: f64) {
    move_to(at);
    // SAFETY: a scroll event with no source; Core Graphics's wheel 1 is vertical, positive up
    unsafe {
        let event = CGEventCreateScrollWheelEvent2(
            std::ptr::null(),
            SCROLL_PIXELS,
            2,
            (-dy).round() as i32,
            (-dx).round() as i32,
            0,
        );
        post(event);
    }
}

/// Type `text` where the focus is, as Unicode: no clipboard, no keyboard layout.
pub fn type_text(text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: a private source; key events carrying the text in chunks macOS accepts
    unsafe {
        let source = CGEventSourceCreate(-1);
        for chunk in units.chunks(16) {
            for down in [true, false] {
                let event = CGEventCreateKeyboardEvent(source, 0, down);
                if !event.is_null() {
                    CGEventKeyboardSetUnicodeString(event, chunk.len(), chunk.as_ptr());
                }
                post(event);
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        if !source.is_null() {
            CFRelease(source);
        }
    }
}

/// Press and release a combination like `cmd+shift+p`.
pub fn key(combination: &str) -> Result<(), String> {
    let (code, flags) = parse_key(combination)?;
    // SAFETY: key events with no source; flags set before posting
    unsafe {
        for down in [true, false] {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), code, down);
            if !event.is_null() {
                CGEventSetFlags(event, flags);
            }
            post(event);
        }
    }
    Ok(())
}

/// A combination's key code (a position on the keyboard, as macOS numbers them) and modifiers.
pub fn parse_key(combination: &str) -> Result<(u16, u64), String> {
    let mut flags = 0u64;
    let parts: Vec<String> = combination.split('+').map(|p| p.trim().to_ascii_lowercase()).collect();
    let (name, modifiers) = parts.split_last().ok_or("an empty key")?;
    for m in modifiers {
        flags |= match m.as_str() {
            "cmd" | "command" | "super" => COMMAND,
            "shift" => SHIFT,
            "ctrl" | "control" => CONTROL,
            "alt" | "option" | "opt" => OPTION,
            "fn" => FUNCTION,
            other => return Err(format!("unknown modifier {other}")),
        };
    }
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6,
    ];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    let code = match name.as_str() {
        n if n.len() == 1 && n.as_bytes()[0].is_ascii_lowercase() => LETTERS[(n.as_bytes()[0] - b'a') as usize],
        n if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() => DIGITS[(n.as_bytes()[0] - b'0') as usize],
        "return" | "enter" => 36,
        "tab" => 48,
        "space" => 49,
        "backspace" | "delete" => 51,
        "escape" | "esc" => 53,
        "forwarddelete" => 117,
        "home" => 115,
        "end" => 119,
        "pageup" => 116,
        "pagedown" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "-" | "minus" => 27,
        "=" | "equal" => 24,
        "," | "comma" => 43,
        "." | "period" => 47,
        "/" | "slash" => 44,
        ";" | "semicolon" => 41,
        "[" => 33,
        "]" => 30,
        "`" | "grave" => 50,
        f if f.starts_with('f') && f.len() <= 3 => match f[1..].parse::<u8>() {
            Ok(n @ 1..=12) => [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111][usize::from(n - 1)],
            _ => return Err(format!("unknown key {f}")),
        },
        other => return Err(format!("unknown key {other}")),
    };
    Ok((code, flags))
}

/// Where the agent aimed: a picture's frame, the target it showed and where that lay.
#[derive(Debug, Clone)]
pub struct Frame {
    pub session: String,
    pub generation: u64,
    pub target: crate::mac_screen::Target,
    /// The target's place on the Mac's desktop when the picture was taken, in points.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// The picture's size in pixels.
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub taken: Instant,
}

/// How long a frame can be aimed in: after that, a new picture.
pub const FRAME_LIFETIME: Duration = Duration::from_secs(120);

impl Frame {
    /// A point in the picture's pixels as a point on the Mac's desktop.
    pub fn to_desktop(&self, px: f64, py: f64) -> Result<Point, String> {
        if !(0.0..f64::from(self.pixel_width)).contains(&px) || !(0.0..f64::from(self.pixel_height)).contains(&py) {
            return Err(format!(
                "{px},{py} is outside the picture ({}×{} pixels)",
                self.pixel_width, self.pixel_height
            ));
        }
        Ok(Point {
            x: self.x + px * self.width / f64::from(self.pixel_width),
            y: self.y + py * self.height / f64::from(self.pixel_height),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(x: f64, y: f64, width: f64, height: f64, pw: u32, ph: u32) -> Frame {
        Frame {
            session: "s".into(),
            generation: 1,
            target: crate::mac_screen::Target::Display(1),
            x,
            y,
            width,
            height,
            pixel_width: pw,
            pixel_height: ph,
            taken: Instant::now(),
        }
    }

    #[test]
    fn picture_pixels_become_desktop_points() {
        // a Retina display: 2 pixels a point
        let retina = frame(0.0, 0.0, 1470.0, 956.0, 2940, 1912);
        assert_eq!(retina.to_desktop(2000.0, 1000.0).unwrap(), Point { x: 1000.0, y: 500.0 });
        // a second display to the left, above: negative desktop origin, 1 pixel a point
        let left = frame(-1920.0, -200.0, 1920.0, 1080.0, 1920, 1080);
        assert_eq!(left.to_desktop(10.0, 20.0).unwrap(), Point { x: -1910.0, y: -180.0 });
        // a window, cropped at its place
        let window = frame(300.0, 120.0, 806.0, 713.0, 1612, 1426);
        assert_eq!(window.to_desktop(0.0, 0.0).unwrap(), Point { x: 300.0, y: 120.0 });
        assert!(window.to_desktop(1612.0, 10.0).is_err());
        assert!(window.to_desktop(-1.0, 10.0).is_err());
    }

    #[test]
    fn sills_own_events_are_told_from_the_persons() {
        assert!(ours(MARK, 0), "marked");
        assert!(ours(0, i64::from(std::process::id())), "posted by this process");
        assert!(!ours(0, 0), "the hardware's");
        assert!(!ours(7, 4242), "another program's");
    }

    #[test]
    fn the_persons_input_since_a_request_began_stops_it() {
        let began = since_now();
        std::thread::sleep(Duration::from_millis(5));
        // the watch saw the person after the request began: its rest stops, and a wait ends early
        PERSON_LAST.store(clock_ms(), Ordering::Release);
        assert!(person_since(began));
        assert!(person_active());
        let started = Instant::now();
        wait(2000, began);
        assert!(started.elapsed() < Duration::from_millis(500));
        // a later request does not see it as since its own start
        let later = since_now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(PERSON_LAST.load(Ordering::Acquire) <= later.0);
    }

    #[test]
    fn key_combinations_are_named() {
        assert_eq!(parse_key("cmd+shift+p").unwrap(), (35, COMMAND | SHIFT));
        assert_eq!(parse_key("return").unwrap(), (36, 0));
        assert_eq!(parse_key("ctrl+alt+left").unwrap(), (123, CONTROL | OPTION));
        assert_eq!(parse_key("cmd+1").unwrap(), (18, COMMAND));
        assert_eq!(parse_key("f12").unwrap(), (111, 0));
        assert!(parse_key("cmd+hyper+x").is_err());
        assert!(parse_key("f13").is_err());
        assert!(parse_key("").is_err());
    }
}
