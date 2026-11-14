//! X11 glue (SPEC §4.6): place the shelf on the left edge of the primary monitor's work area, keep it
//! above other windows, out of the taskbar and pager. GTK4 has no window-positioning API, so this
//! talks to the X server directly through the surface's XID. Wayland: nothing here applies yet.

use gdk4_x11::X11Surface;
use gtk4 as gtk;
use gtk::prelude::*;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, PropMode};
use x11rb::wrapper::ConnectionExt as _;

/// Fraction of the work area width the shelf takes (settings override).
pub const WIDTH_FRACTION: f64 = 0.38;
thread_local! {
    static WIDTH: std::cell::Cell<f64> = const { std::cell::Cell::new(WIDTH_FRACTION) };
}
pub fn set_width_fraction(f: f64) {
    WIDTH.with(|w| w.set(f.clamp(0.2, 0.8)));
}
pub fn width_fraction() -> f64 {
    WIDTH.with(|w| w.get())
}

pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

fn xid(window: &gtk::Window) -> Option<u32> {
    let surface = window.surface()?;
    let x11 = surface.downcast::<X11Surface>().ok()?;
    Some(x11.xid() as u32)
}

/// Work area of the current desktop (`_NET_WORKAREA`, panels excluded), falling back to the first
/// monitor's geometry. With an explicit width fraction (the Files page widens the shelf to at least half).
pub fn work_area_with(window: &gtk::Window, fraction: f64) -> Option<Placement> {
    let (x, y, w, h) = net_workarea().or_else(|| {
        let display = WidgetExt::display(window);
        let m = display.monitors().item(0)?.downcast::<gtk::gdk::Monitor>().ok()?;
        let g = m.geometry();
        Some((g.x(), g.y(), g.width() as u32, g.height() as u32))
    })?;
    let width = (w as f64 * fraction).round() as u32;
    Some(Placement { x, y, width, height: h })
}

fn net_workarea() -> Option<(i32, i32, u32, u32)> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen_num].root;
    let atom = |name: &[u8]| conn.intern_atom(false, name).ok()?.reply().ok().map(|r| r.atom);
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
    let reply = conn.get_property(false, root, workarea, AtomEnum::CARDINAL, 0, 1024).ok()?.reply().ok()?;
    let v: Vec<u32> = reply.value32()?.collect();
    let i = desktop * 4;
    if v.len() < i + 4 {
        return None;
    }
    Some((v[i] as i32, v[i + 1] as i32, v[i + 2], v[i + 3]))
}

/// Move/resize the mapped window and set the EWMH state (above, skip taskbar/pager).
pub fn place(window: &gtk::Window, p: &Placement) {
    let Some(xid) = xid(window) else { return };
    let Ok((conn, screen_num)) = x11rb::connect(None) else { return };
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let _ = conn.configure_window(
        xid,
        &ConfigureWindowAux::new().x(p.x).y(p.y).width(p.width).height(p.height),
    );
    // _NET_WM_STATE add ABOVE + SKIP_TASKBAR + SKIP_PAGER via client messages to the root window
    let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
    if let (Some(state), Some(above), Some(skip_tb), Some(skip_pg)) =
        (atom("_NET_WM_STATE"), atom("_NET_WM_STATE_ABOVE"), atom("_NET_WM_STATE_SKIP_TASKBAR"), atom("_NET_WM_STATE_SKIP_PAGER"))
    {
        for (a, b) in [(above, skip_tb), (skip_pg, 0)] {
            let ev = ClientMessageEvent::new(32, xid, state, [1u32, a, b, 1, 0]); // 1 = _NET_WM_STATE_ADD, source 1 = application
            let _ = conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
        }
    }
    // also set the property directly in case the WM reads it at map time
    if let (Some(state), Some(above), Some(skip_tb), Some(skip_pg)) =
        (atom("_NET_WM_STATE"), atom("_NET_WM_STATE_ABOVE"), atom("_NET_WM_STATE_SKIP_TASKBAR"), atom("_NET_WM_STATE_SKIP_PAGER"))
    {
        let _ = conn.change_property32(PropMode::REPLACE, xid, state, AtomEnum::ATOM, &[above, skip_tb, skip_pg]);
    }
    let _ = conn.flush();
}

/// Is this window the X server's active window? GTK's own `is_active` is unreliable at hotkey time:
/// the WM's key grab takes the focus away for a moment.
pub fn is_active(window: &gtk::Window) -> bool {
    let Some(xid) = xid(window) else { return false };
    let Ok((conn, screen_num)) = x11rb::connect(None) else { return false };
    let root = conn.setup().roots[screen_num].root;
    let Some(atom) = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok().and_then(|c| c.reply().ok()).map(|r| r.atom) else {
        return false;
    };
    conn.get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().and_then(|mut v| v.next()))
        .map(|active| active == xid)
        .unwrap_or(false)
}

/// Ask the WM to activate (focus + raise) the window.
pub fn activate(window: &gtk::Window) {
    let Some(xid) = xid(window) else { return };
    let Ok((conn, screen_num)) = x11rb::connect(None) else { return };
    let root = conn.setup().roots[screen_num].root;
    if let Ok(c) = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")
        && let Ok(r) = c.reply()
    {
        let ev = ClientMessageEvent::new(32, xid, r.atom, [1u32, x11rb::CURRENT_TIME, 0, 0, 0]);
        let _ = conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
        let _ = conn.flush();
    }
}
