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
/// The file tree (the drawer) takes this share of the shelf's width when it opens, until the
/// handle between the panes has been dragged: then the dragged width, logical pixels, remembered.
pub const DRAWER_SHARE: f64 = 0.3;
pub const DRAWER_MIN: i32 = 160;
/// What the main pane keeps at least when the drawer is open.
pub const MAIN_MIN: i32 = 240;
thread_local! {
    static WIDTH: std::cell::Cell<f64> = const { std::cell::Cell::new(WIDTH_FRACTION) };
    static DRAWER: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}
pub fn set_width_fraction(f: f64) {
    WIDTH.with(|w| w.set(f.clamp(0.2, 0.8)));
}
pub fn width_fraction() -> f64 {
    WIDTH.with(|w| w.get())
}
pub fn set_drawer_width(px: i32) {
    DRAWER.with(|w| w.set(Some(px.max(DRAWER_MIN))));
}
/// The drawer's width for a shelf `total` wide: the dragged width when there is one, else the
/// default share — always leaving the main pane its minimum.
pub fn drawer_width(total: i32) -> i32 {
    let d = DRAWER.with(|w| w.get()).unwrap_or((total as f64 * DRAWER_SHARE).round() as i32);
    d.clamp(DRAWER_MIN, (total - MAIN_MIN).max(DRAWER_MIN))
}

/// The window's scale (device pixels per logical pixel). Before the window is realized GTK can't
/// tell, so the first monitor's scale is used.
pub fn scale_of(window: &gtk::Window) -> i32 {
    let s = window.scale_factor();
    if s > 1 {
        return s;
    }
    let display = WidgetExt::display(window);
    display.monitors().item(0).and_downcast::<gtk::gdk::Monitor>().map(|m| m.scale_factor()).unwrap_or(1).max(1)
}

/// The compositor's opacity for the window (`_NET_WM_WINDOW_OPACITY`): 0 keeps a mapped window
/// invisible until its first settled frame, 1 (the property removed) shows it. A compositor without
/// support ignores it, which only brings back the old behaviour.
pub fn set_opacity(window: &gtk::Window, opacity: f64) {
    let Some(xid) = xid(window) else { return };
    let Ok((conn, _)) = x11rb::connect(None) else { return };
    let Some(atom) = conn.intern_atom(false, b"_NET_WM_WINDOW_OPACITY").ok().and_then(|c| c.reply().ok()).map(|r| r.atom) else { return };
    if opacity >= 1.0 {
        let _ = conn.delete_property(xid, atom);
    } else {
        let v = (opacity.clamp(0.0, 1.0) * u32::MAX as f64) as u32;
        let _ = conn.change_property32(PropMode::REPLACE, xid, atom, AtomEnum::CARDINAL, &[v]);
    }
    let _ = conn.flush();
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

/// The primary monitor (RandR; the first one when none is marked primary), in device pixels.
fn primary_monitor() -> Option<(i32, i32, u32, u32)> {
    use x11rb::protocol::randr::ConnectionExt as _;
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen_num].root;
    let reply = conn.randr_get_monitors(root, true).ok()?.reply().ok()?;
    let m = reply.monitors.iter().find(|m| m.primary).or_else(|| reply.monitors.first())?;
    Some((m.x as i32, m.y as i32, m.width as u32, m.height as u32))
}

/// The primary monitor's work area — its geometry cut down to `_NET_WORKAREA` (panels excluded;
/// that property spans the whole virtual screen, so on its own it made the shelf a fraction of all
/// monitors together) — with the shelf's width as `fraction` of it. Falls back to GDK's first
/// monitor (logical pixels, scaled up) without RandR.
pub fn work_area_with(window: &gtk::Window, fraction: f64) -> Option<Placement> {
    let (mx, my, mw, mh) = primary_monitor().or_else(|| {
        let display = WidgetExt::display(window);
        let m = display.monitors().item(0)?.downcast::<gtk::gdk::Monitor>().ok()?;
        let g = m.geometry();
        let k = m.scale_factor().max(1);
        Some((g.x() * k, g.y() * k, (g.width() * k) as u32, (g.height() * k) as u32))
    })?;
    let (x, y, w, h) = match net_workarea() {
        Some((wx, wy, ww, wh)) => {
            let x0 = mx.max(wx);
            let y0 = my.max(wy);
            let x1 = (mx + mw as i32).min(wx + ww as i32);
            let y1 = (my + mh as i32).min(wy + wh as i32);
            if x1 > x0 && y1 > y0 { (x0, y0, (x1 - x0) as u32, (y1 - y0) as u32) } else { (mx, my, mw, mh) }
        }
        None => (mx, my, mw, mh),
    };
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
/// Geometry for a window that is realized but not yet mapped: set it on the X window and in its
/// size hints as user-specified, so the window manager maps it there in one go instead of placing
/// it, then watching us move and resize it (the visible jump on every show). GTK4 destroys the X
/// window on hide, so this runs on every realize.
pub fn pre_place(window: &gtk::Window, p: &Placement) {
    use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
    let Some(xid) = xid(window) else { return };
    let Ok((conn, _)) = x11rb::connect(None) else { return };
    let _ = conn.configure_window(xid, &ConfigureWindowAux::new().x(p.x).y(p.y).width(p.width).height(p.height));
    let mut hints = WmSizeHints::get_normal_hints(&conn, xid).ok().and_then(|c| c.reply().ok()).flatten().unwrap_or_default();
    hints.position = Some((WmSizeHintsSpecification::UserSpecified, p.x, p.y));
    hints.size = Some((WmSizeHintsSpecification::UserSpecified, p.width as i32, p.height as i32));
    let _ = hints.set_normal_hints(&conn, xid);
    let _ = conn.flush();
}

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
/// Ask the window manager to focus the shelf. `time` is the X server time of the user action that
/// asked for it (the hotkey press); with 0 (`CURRENT_TIME`) focus-stealing prevention may only mark
/// the window as demanding attention instead of raising it.
pub fn activate(window: &gtk::Window, time: u32) {
    let Some(xid) = xid(window) else { return };
    let Ok((conn, screen_num)) = x11rb::connect(None) else { return };
    let root = conn.setup().roots[screen_num].root;
    if let Ok(c) = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")
        && let Ok(r) = c.reply()
    {
        let ev = ClientMessageEvent::new(32, xid, r.atom, [1u32, time, 0, 0, 0]);
        let _ = conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
        let _ = conn.flush();
    }
}
