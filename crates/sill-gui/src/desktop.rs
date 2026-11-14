//! The window on an X11 desktop — the platform side of `geometry.rs`:
//!
//! - **the displays and the window:** RandR's monitors (name, primary, bounds), each one's usable
//!   area from the window manager's work areas, and the window's own geometry, in device pixels;
//! - **placing it:** its place, size and size limits, set as the user's choice in its size hints,
//!   so the window manager neither places it itself on mapping nor holds it to an old limit;
//! - **coming up with the focus:** the window dated as just used (`_NET_WM_USER_TIME`), a
//!   pager's `_NET_ACTIVE_WINDOW` request at the server's time now, and, should the window manager
//!   let that go by, the focus set directly (the window asks again until it is active).
//!
//! Each call opens its own short X connection; without X11 (Wayland) there is no window id to
//! pass them (`main.rs` asks iced for an X11 handle only).
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask,
    PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::geometry::{Display, Limits, Rect};

fn connect() -> Option<(RustConnection, usize)> {
    std::env::var_os("DISPLAY")?;
    x11rb::connect(None).ok()
}

fn atom(conn: &RustConnection, name: &[u8]) -> Option<u32> {
    conn.intern_atom(false, name)
        .ok()?
        .reply()
        .ok()
        .map(|r| r.atom)
}

/// A CARDINAL list property of the root window.
fn cardinals(conn: &RustConnection, root: u32, name: &[u8]) -> Option<Vec<u32>> {
    let property = atom(conn, name)?;
    let reply = conn
        .get_property(false, root, property, AtomEnum::CARDINAL, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    Some(reply.value32()?.collect())
}

fn rects(values: &[u32]) -> Vec<Rect> {
    let (areas, _) = values.as_chunks::<4>();
    areas
        .iter()
        .map(|&[x, y, width, height]| Rect {
            x: x as i32,
            y: y as i32,
            width,
            height,
        })
        .collect()
}

/// The current desktop's work areas: one per monitor where the window manager gives them
/// (`_GTK_WORKAREAS_D<n>`, Mutter's and Muffin's), else the one over all (`_NET_WORKAREA`).
fn work_areas(conn: &RustConnection, root: u32) -> Vec<Rect> {
    let desktop = cardinals(conn, root, b"_NET_CURRENT_DESKTOP")
        .and_then(|v| v.first().copied())
        .unwrap_or(0) as usize;
    if let Some(areas) = cardinals(conn, root, format!("_GTK_WORKAREAS_D{desktop}").as_bytes())
        && !areas.is_empty()
    {
        return rects(&areas);
    }
    cardinals(conn, root, b"_NET_WORKAREA")
        .map(|v| rects(&v).into_iter().skip(desktop).take(1).collect())
        .unwrap_or_default()
}

fn read_displays(conn: &RustConnection, root: u32) -> Option<Vec<Display>> {
    use x11rb::protocol::randr::ConnectionExt as _;
    let reply = conn.randr_get_monitors(root, true).ok()?.reply().ok()?;
    let areas = work_areas(conn, root);
    let displays = reply
        .monitors
        .iter()
        .map(|m| {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_else(|| format!("{}x{}+{}+{}", m.width, m.height, m.x, m.y));
            let bounds = Rect {
                x: m.x as i32,
                y: m.y as i32,
                width: m.width as u32,
                height: m.height as u32,
            };
            Display {
                name,
                primary: m.primary,
                bounds,
                work: crate::geometry::usable(bounds, &areas),
            }
        })
        .collect::<Vec<_>>();
    (!displays.is_empty()).then_some(displays)
}

/// Where the window `xid` is on the screen and how large, device pixels.
fn window_rect(conn: &RustConnection, root: u32, xid: u64) -> Option<Rect> {
    let geometry = conn.get_geometry(xid as u32).ok()?.reply().ok()?;
    let at = conn
        .translate_coordinates(xid as u32, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    Some(Rect {
        x: at.dst_x as i32,
        y: at.dst_y as i32,
        width: geometry.width as u32,
        height: geometry.height as u32,
    })
}

/// The displays as the desktop has them now.
pub fn displays() -> Option<Vec<Display>> {
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].root;
    read_displays(&conn, root)
}

/// The displays and the window `xid` as they are now, in one connection.
pub fn snapshot(xid: u64) -> Option<(Vec<Display>, Option<Rect>)> {
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].root;
    let displays = read_displays(&conn, root)?;
    Some((displays, window_rect(&conn, root, xid)))
}

/// Put the window `xid` at `rect`, held to `limits` (none: no limits), and say so in its size
/// hints as the user's choice, so the window manager maps it there instead of placing it itself
/// (what put it 50 px from the edge on every show). The hints go first: a limit left from another
/// mode would otherwise hold the new size to it.
pub fn place(xid: u64, rect: Rect, limits: Option<Limits>) {
    use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
    let Some((conn, _)) = connect() else { return };
    let mut hints = WmSizeHints::get_normal_hints(&conn, xid as u32)
        .ok()
        .and_then(|c| c.reply().ok())
        .flatten()
        .unwrap_or_default();
    let size = |(w, h): (u32, u32)| (w as i32, h as i32);
    hints.position = Some((WmSizeHintsSpecification::UserSpecified, rect.x, rect.y));
    hints.size = Some((
        WmSizeHintsSpecification::UserSpecified,
        rect.width as i32,
        rect.height as i32,
    ));
    hints.min_size = limits.map(|l| size(l.min));
    hints.max_size = limits.map(|l| size(l.max));
    let _ = hints.set_normal_hints(&conn, xid as u32);
    let _ = conn.configure_window(
        xid as u32,
        &ConfigureWindowAux::new()
            .x(rect.x)
            .y(rect.y)
            .width(rect.width)
            .height(rect.height),
    );
    let _ = conn.flush();
}

/// Raise `xid` and give it the input focus ourselves, at the server's time now: for when the
/// window manager let an activation request go by (it does, now and then, without saying so).
/// Window managers follow a focus a client sets on its own window.
pub fn focus(xid: u64) {
    use x11rb::protocol::xproto::{InputFocus, StackMode};
    let Some((conn, screen)) = connect() else {
        return;
    };
    let root = conn.setup().roots[screen].root;
    let time = server_time(&conn, root).unwrap_or(0);
    let _ = conn.configure_window(
        xid as u32,
        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
    );
    let _ = conn.set_input_focus(InputFocus::PARENT, xid as u32, time);
    let _ = conn.flush();
}

/// The window manager's active window (`_NET_ACTIVE_WINDOW`), if any.
fn active_window() -> Option<u64> {
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].root;
    let atom = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .ok()?
        .reply()
        .ok()?
        .atom;
    conn.get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
        .map(u64::from)
}

/// Is `xid` the window manager's active window?
pub fn is_active(xid: u64) -> bool {
    active_window() == Some(xid)
}

/// The X server's time now, as GDK finds it: a property touched on a window of our own reports
/// it. For an activation that no key press of ours dated (`sill toggle` from a desktop shortcut).
fn server_time(conn: &RustConnection, root: u32) -> Option<u32> {
    let window = conn.generate_id().ok()?;
    conn.create_window(
        0,
        window,
        root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .ok()?;
    let atom = conn
        .intern_atom(false, b"_SILL_TIME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    conn.change_property8(PropMode::APPEND, window, atom, AtomEnum::STRING, &[])
        .ok()?;
    conn.flush().ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    let time = loop {
        match conn.poll_for_event() {
            Ok(Some(Event::PropertyNotify(e))) if e.window == window => break Some(e.time),
            Ok(Some(_)) => {}
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => break None,
        }
    };
    let _ = conn.destroy_window(window);
    let _ = conn.flush();
    time
}

/// The window's own user time, set as a toolkit's present does: the window manager weighs a
/// window's last use against the focused one's, and gave the focus back to the other window when
/// ours looked older (a window brought up from hidden, 2026-11-05).
fn set_user_time(conn: &RustConnection, xid: u64, time: u32) {
    if let Ok(c) = conn.intern_atom(false, b"_NET_WM_USER_TIME")
        && let Ok(r) = c.reply()
    {
        let _ = conn.change_property32(
            PropMode::REPLACE,
            xid as u32,
            r.atom,
            AtomEnum::CARDINAL,
            &[time],
        );
        let _ = conn.flush();
    }
}

/// Date the window `xid` as used now: before it is mapped, so the window manager focuses it
/// when it maps it.
pub fn stamp(xid: u64) {
    let Some((conn, screen)) = connect() else {
        return;
    };
    let root = conn.setup().roots[screen].root;
    if let Some(time) = server_time(&conn, root) {
        set_user_time(&conn, xid, time);
    }
}

/// Ask the window manager to activate (raise and focus) `xid`, as the user's own request. `time`
/// is the X server time of the press that asked for it; without one (0), the server's time now.
pub fn activate(xid: u64, time: u32) {
    let Some((conn, screen)) = connect() else {
        return;
    };
    let root = conn.setup().roots[screen].root;
    // always a request on the user's behalf, as a pager's (source 2): an application's (source
    // 1), even dated by the hotkey's own press, was refused now and then by the window manager's
    // focus-stealing prevention — the hotkey then raised nothing, and the next press tried again
    // (the person's "loop between two windows", 2026-11-05). Dated by the press when there was
    // one, else by the server's time now.
    let source = 2u32;
    // the server's time now: a request dated earlier than the window manager's last focus change
    // is ignored as stale, and the press's own time could be (seen in tests, 2026-11-05)
    let time = server_time(&conn, root).unwrap_or(time);
    set_user_time(&conn, xid, time);
    if let Ok(c) = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")
        && let Ok(r) = c.reply()
    {
        let event = ClientMessageEvent::new(32, xid as u32, r.atom, [source, time, 0, 0, 0]);
        let _ = conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        );
        let _ = conn.flush();
    }
}
