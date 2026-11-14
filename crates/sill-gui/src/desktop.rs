//! The window on an X11 desktop (after the GTK shelf's `shelf/x11.rs`):
//!
//! - **where it goes:** Shelf mode on the primary monitor's left edge at its full working height
//!   (panels excluded); Drawer and Full where the window was left — set before the window is
//!   mapped, as the user's choice in its size hints, so the window manager does not place it;
//! - **coming up with the focus:** the window dated as just used (`_NET_WM_USER_TIME`), a
//!   pager's `_NET_ACTIVE_WINDOW` request at the server's time now, and, should the window manager
//!   let that go by, the focus set directly (the window asks again until it is active).
//!
//! Each call opens its own short X connection; without X11 (Wayland) they do nothing.
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask,
    PropMode, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

/// A window's place on the screen, in device pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

fn connect() -> Option<(x11rb::rust_connection::RustConnection, usize)> {
    std::env::var_os("DISPLAY")?;
    x11rb::connect(None).ok()
}

/// The primary monitor (RandR; the first when none is marked primary), in device pixels.
fn primary_monitor() -> Option<(i32, i32, u32, u32)> {
    use x11rb::protocol::randr::ConnectionExt as _;
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].root;
    let reply = conn.randr_get_monitors(root, true).ok()?.reply().ok()?;
    let m = reply
        .monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| reply.monitors.first())?;
    Some((m.x as i32, m.y as i32, m.width as u32, m.height as u32))
}

/// `_NET_WORKAREA` of the current desktop: the screen without the panels (over every monitor).
fn net_workarea() -> Option<(i32, i32, u32, u32)> {
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].root;
    let atom = |name: &[u8]| {
        conn.intern_atom(false, name)
            .ok()?
            .reply()
            .ok()
            .map(|r| r.atom)
    };
    let workarea = atom(b"_NET_WORKAREA")?;
    let current = atom(b"_NET_CURRENT_DESKTOP")?;
    let desktop = conn
        .get_property(false, root, current, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
        .unwrap_or(0) as usize;
    let reply = conn
        .get_property(false, root, workarea, AtomEnum::CARDINAL, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    let v: Vec<u32> = reply.value32()?.collect();
    let i = desktop * 4;
    (v.len() >= i + 4).then(|| (v[i] as i32, v[i + 1] as i32, v[i + 2], v[i + 3]))
}

/// Where Shelf mode goes: the primary monitor's working area, its left edge, `share` of its width.
pub fn shelf_placement(share: f32) -> Option<Placement> {
    let (mx, my, mw, mh) = primary_monitor()?;
    let (x, y, w, h) = match net_workarea() {
        Some((wx, wy, ww, wh)) => {
            let (x0, y0) = (mx.max(wx), my.max(wy));
            let x1 = (mx + mw as i32).min(wx + ww as i32);
            let y1 = (my + mh as i32).min(wy + wh as i32);
            if x1 > x0 && y1 > y0 {
                (x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
            } else {
                (mx, my, mw, mh)
            }
        }
        None => (mx, my, mw, mh),
    };
    Some(Placement {
        x,
        y,
        width: (w as f32 * share).round() as u32,
        height: h,
    })
}

/// The top-left of the primary monitor's working area: where a window goes until it is moved.
pub fn work_origin() -> Option<(i32, i32)> {
    shelf_placement(1.0).map(|p| (p.x, p.y))
}

/// Where the window `xid` is on the screen (its top-left, in device pixels).
pub fn position(xid: u64) -> Option<(i32, i32)> {
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].root;
    let r = conn
        .translate_coordinates(xid as u32, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    Some((r.dst_x as i32, r.dst_y as i32))
}

/// Put the window `xid` at `x`, `y` — sized `size` when given — and say so in its size hints as
/// the user's choice, so the window manager maps it there instead of placing it itself (what
/// put it 50 px from the edge on every show).
pub fn pre_place(xid: u64, x: i32, y: i32, size: Option<(u32, u32)>) {
    use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
    let Some((conn, _)) = connect() else { return };
    let mut aux = ConfigureWindowAux::new().x(x).y(y);
    if let Some((w, h)) = size {
        aux = aux.width(w).height(h);
    }
    let _ = conn.configure_window(xid as u32, &aux);
    let mut hints = WmSizeHints::get_normal_hints(&conn, xid as u32)
        .ok()
        .and_then(|c| c.reply().ok())
        .flatten()
        .unwrap_or_default();
    hints.position = Some((WmSizeHintsSpecification::UserSpecified, x, y));
    if let Some((w, h)) = size {
        hints.size = Some((WmSizeHintsSpecification::UserSpecified, w as i32, h as i32));
    }
    let _ = hints.set_normal_hints(&conn, xid as u32);
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
fn server_time(conn: &x11rb::rust_connection::RustConnection, root: u32) -> Option<u32> {
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

/// The window's own user time, as GTK's present sets it: the window manager weighs a window's
/// last use against the focused one's, and gave the focus back to the other window when ours
/// looked older (a window brought up from hidden, 2026-11-05).
fn set_user_time(conn: &x11rb::rust_connection::RustConnection, xid: u64, time: u32) {
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

/// Primary working area and current client size, all in physical pixels.
pub fn window_geometry(xid: u64) -> Option<(Placement, u32, u32)> {
    let area = shelf_placement(1.0)?;
    let (conn, _) = connect()?;
    let geometry = conn.get_geometry(xid as u32).ok()?.reply().ok()?;
    Some((area, geometry.width as u32, geometry.height as u32))
}
