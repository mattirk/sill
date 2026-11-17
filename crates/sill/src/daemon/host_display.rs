//! Display operations of Host control (docs/permission-modes.md, Host control): viewing the
//! person's real desktop and giving it input, outside the jail. X11 only: the daemon reads the
//! root window (or one top-level window's area of it) itself and drives the pointer and keyboard
//! through `xdotool`'s XTEST path with actions it validated one by one; a Wayland session, a
//! daemon without a display or a missing `xdotool` answer "unavailable" and nothing happens.
//! No `/dev/uinput`, no shell: the daemon builds every argv itself.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sill_core::desktop_capture::Raw;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, ImageFormat, MapState, Window};
use x11rb::rust_connection::RustConnection;

/// The largest picture read: a few 4K monitors.
const MAX_PIXELS: u64 = 64 * 1024 * 1024;
/// Input bounds: one request is one short interaction, not a script.
const MAX_ACTIONS: usize = 32;
const MAX_TYPED: usize = 1024;
const MAX_KEY: usize = 64;
const MAX_WAIT_MS: u64 = 2000;
/// Milliseconds between typed characters (xdotool's default is 12): the longest `type` stays
/// well inside the jailed client's reply timeout.
const TYPE_DELAY_MS: u64 = 5;

/// Why display operations cannot run here, or Ok.
pub fn availability() -> Result<()> {
    if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") {
        bail!("the host desktop is a Wayland session; display operations need an X11 session");
    }
    if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
        bail!("the daemon has no DISPLAY; display operations need the host's X11 display");
    }
    Ok(())
}

fn connect() -> Result<(RustConnection, Window)> {
    let (conn, screen) = x11rb::connect(None).context("connecting to the host display")?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

/// A top-level window of the desktop, as `view {"list":true}` reports it.
fn windows(conn: &RustConnection, root: Window) -> Result<Vec<Value>> {
    let list = conn.intern_atom(true, b"_NET_CLIENT_LIST")?.reply()?.atom;
    let net_name = conn.intern_atom(true, b"_NET_WM_NAME")?.reply()?.atom;
    let utf8 = conn.intern_atom(true, b"UTF8_STRING")?.reply()?.atom;
    let none: Atom = AtomEnum::NONE.into();
    // the window manager's list of application windows; without one (no EWMH), the root's
    // mapped children
    let mut ids: Vec<Window> = if list == none {
        vec![]
    } else {
        conn.get_property(false, root, list, AtomEnum::WINDOW, 0, 1024)?.reply()?.value32().map(|v| v.collect()).unwrap_or_default()
    };
    if ids.is_empty() {
        ids = conn.query_tree(root)?.reply()?.children;
        ids.truncate(1024);
    }
    let mut rows = vec![];
    for id in ids {
        let Ok(geometry) = conn.get_geometry(id)?.reply() else { continue };
        let Ok(at) = conn.translate_coordinates(id, root, 0, 0)?.reply() else { continue };
        let Ok(attributes) = conn.get_window_attributes(id)?.reply() else { continue };
        let name = [(net_name, utf8), (AtomEnum::WM_NAME.into(), AtomEnum::STRING.into())]
            .into_iter()
            .filter(|(property, _)| *property != none)
            .find_map(|(property, kind)| {
                let reply = conn.get_property(false, id, property, kind, 0, 256).ok()?.reply().ok()?;
                let bytes: Vec<u8> = reply.value8()?.collect();
                (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
            })
            .unwrap_or_default();
        rows.push(json!({
            "window": id, "name": name, "x": at.dst_x, "y": at.dst_y,
            "width": geometry.width, "height": geometry.height,
            "visible": attributes.map_state == MapState::VIEWABLE,
        }));
    }
    Ok(rows)
}

/// What `view` found: the desktop's windows, or a picture.
pub enum View {
    Windows(Vec<Value>),
    Picture { raw: Raw, window: Option<Window>, x: i32, y: i32 },
}

/// `{}`: the whole desktop; `{"list":true}`: its top-level windows; `{"window":ID}`: that
/// window's area of the desktop (what is on screen there, as the person sees it).
pub fn view(args: &Value) -> Result<View> {
    let (conn, root) = connect()?;
    if args.get("list").is_some_and(|l| l == &Value::Bool(true)) {
        return Ok(View::Windows(windows(&conn, root)?));
    }
    let screen = conn.get_geometry(root)?.reply().context("the display's size")?;
    let (window, x, y, width, height) = match args.get("window") {
        None | Some(Value::Null) => (None, 0i32, 0i32, i32::from(screen.width), i32::from(screen.height)),
        Some(id) => {
            let id = id.as_u64().and_then(|v| u32::try_from(v).ok()).context("window must be an id from view {\"list\":true}")?;
            let geometry = conn.get_geometry(id)?.reply().context("no such window")?;
            let at = conn.translate_coordinates(id, root, 0, 0)?.reply()?;
            // the window's area clipped to the screen; an off-screen window has no picture
            let x0 = i32::from(at.dst_x).max(0);
            let y0 = i32::from(at.dst_y).max(0);
            let x1 = (i32::from(at.dst_x) + i32::from(geometry.width)).min(i32::from(screen.width));
            let y1 = (i32::from(at.dst_y) + i32::from(geometry.height)).min(i32::from(screen.height));
            if x1 <= x0 || y1 <= y0 {
                bail!("the window is off screen");
            }
            (Some(id), x0, y0, x1 - x0, y1 - y0)
        }
    };
    if width <= 0 || height <= 0 || width as u64 * height as u64 > MAX_PIXELS {
        bail!("unexpected picture size {width}×{height}");
    }
    let format = conn.setup().pixmap_formats.iter().find(|f| f.depth == screen.depth).context("the display's pixel format")?;
    if format.bits_per_pixel != 32 {
        bail!("the display is not a 32-bit RGB one ({} bits per pixel)", format.bits_per_pixel);
    }
    let image = conn.get_image(ImageFormat::Z_PIXMAP, root, x as i16, y as i16, width as u16, height as u16, !0)?.reply().context("reading the display")?;
    let mut bgrx = image.data;
    anyhow::ensure!(bgrx.len() == width as usize * height as usize * 4, "short picture");
    if conn.setup().image_byte_order == x11rb::protocol::xproto::ImageOrder::MSB_FIRST {
        for px in bgrx.chunks_exact_mut(4) {
            px.reverse();
        }
    }
    Ok(View::Picture { raw: Raw { width: width as u32, height: height as u32, bgrx }, window, x, y })
}

/// One validated input action and the `xdotool` argv (after the command name) that performs it.
#[derive(Debug, PartialEq)]
pub enum Action {
    Move(u32, u32),
    Click(u8),
    Key(String),
    Type(String),
    Wait(u64),
}

impl Action {
    /// `{"move":[x,y]}`, `{"click":1..5}`, `{"key":"ctrl+l"}`, `{"type":"text"}`, `{"wait":ms}`.
    pub fn parse(v: &Value) -> Result<Self> {
        let object = v.as_object().filter(|o| o.len() == 1).context("an action is an object with one key: move, click, key, type or wait")?;
        let (name, value) = object.iter().next().unwrap();
        Ok(match name.as_str() {
            "move" => {
                let at: [u32; 2] = serde_json::from_value(value.clone()).context("move takes [x, y]")?;
                anyhow::ensure!(at[0] < 32768 && at[1] < 32768, "move: out of range");
                Action::Move(at[0], at[1])
            }
            "click" => {
                let button = value.as_u64().filter(|b| (1..=5).contains(b)).context("click takes a button 1..5 (4 and 5 scroll)")?;
                Action::Click(button as u8)
            }
            "key" => {
                let key = value.as_str().context("key takes a name such as Return or ctrl+l")?;
                anyhow::ensure!(
                    !key.is_empty() && key.len() <= MAX_KEY && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '_'),
                    "key: a keysym name, with + for modifiers"
                );
                Action::Key(key.into())
            }
            "type" => {
                let text = value.as_str().context("type takes a string")?;
                anyhow::ensure!(
                    !text.is_empty() && text.len() <= MAX_TYPED && text.chars().all(|c| !c.is_control() || c == '\n' || c == '\t'),
                    "type: up to {MAX_TYPED} bytes, no control characters but newline and tab"
                );
                Action::Type(text.into())
            }
            "wait" => {
                let ms = value.as_u64().filter(|ms| *ms <= MAX_WAIT_MS).context("wait takes milliseconds, at most 2000")?;
                Action::Wait(ms)
            }
            other => bail!("unknown action {other:?}"),
        })
    }
    pub fn argv(&self) -> Option<Vec<String>> {
        Some(match self {
            Action::Move(x, y) => vec!["mousemove".into(), x.to_string(), y.to_string()],
            Action::Click(b) => vec!["click".into(), b.to_string()],
            Action::Key(k) => vec!["key".into(), "--".into(), k.clone()],
            Action::Type(t) => vec!["type".into(), "--delay".into(), TYPE_DELAY_MS.to_string(), "--".into(), t.clone()],
            Action::Wait(_) => return None,
        })
    }
}

pub fn actions(args: &Value) -> Result<Vec<Action>> {
    let list = args.get("actions").and_then(Value::as_array).context("input takes {\"actions\":[...]}")?;
    anyhow::ensure!(!list.is_empty() && list.len() <= MAX_ACTIONS, "input: 1 to {MAX_ACTIONS} actions per request");
    list.iter().map(Action::parse).collect()
}

/// Perform the actions in order on the host display, stopping at the first that fails. Reports
/// how many were done and where the pointer is afterwards.
pub fn input(args: &Value) -> Result<Value> {
    let actions = actions(args)?;
    let xdotool = ["/usr/local/bin/xdotool", "/usr/bin/xdotool"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .context("xdotool is not installed; display input needs it")?;
    let mut performed = 0usize;
    for action in &actions {
        match action.argv() {
            None => {
                if let Action::Wait(ms) = action {
                    std::thread::sleep(std::time::Duration::from_millis(*ms));
                }
            }
            Some(argv) => {
                let out = std::process::Command::new(xdotool).args(&argv).stdin(std::process::Stdio::null()).output().context("running xdotool")?;
                if !out.status.success() {
                    bail!("action {} ({}) failed after {performed} done: {}", performed + 1, argv[0], String::from_utf8_lossy(&out.stderr).trim());
                }
            }
        }
        performed += 1;
    }
    let pointer = connect().and_then(|(conn, root)| Ok(conn.query_pointer(root)?.reply()?)).ok();
    Ok(json!({
        "performed": performed,
        "pointer": pointer.map(|p| json!({"x": p.root_x, "y": p.root_y})),
        "location": "host",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_validated_one_by_one_and_never_shell() {
        let ok = actions(&json!({"actions":[{"move":[10,20]},{"click":1},{"key":"ctrl+l"},{"type":"hi there\n"},{"wait":100}]})).unwrap();
        assert_eq!(ok[0], Action::Move(10, 20));
        assert_eq!(ok[2].argv().unwrap(), ["key", "--", "ctrl+l"]);
        assert_eq!(ok[3].argv().unwrap(), ["type", "--delay", "5", "--", "hi there\n"]);
        assert!(ok[4].argv().is_none());
        for bad in [
            json!({"actions":[]}),
            json!({"actions":[{"click":6}]}),
            json!({"actions":[{"key":"a; rm -rf /"}]}),
            json!({"actions":[{"key":"-"}]}),
            json!({"actions":[{"type":"\u{1b}[A"}]}),
            json!({"actions":[{"type":"x".repeat(MAX_TYPED + 1)}]}),
            json!({"actions":[{"wait":5000}]}),
            json!({"actions":[{"move":[1,2],"click":1}]}),
            json!({"actions":[{"run":"xterm"}]}),
            json!({"actions": vec![json!({"move":[1,2]}); MAX_ACTIONS + 1]}),
            json!({"argv":["xdotool","key","a"]}),
        ] {
            assert!(actions(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn display_operations_are_unavailable_without_an_x11_display() {
        // the process environment is shared: only the outcomes that hold whatever it is
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") || std::env::var_os("DISPLAY").is_none() {
            assert!(availability().is_err());
        }
    }

    /// `cargo test -- --ignored host_display`: a picture and the window list of the real display.
    #[test]
    #[ignore]
    fn the_real_display_can_be_viewed() {
        availability().unwrap();
        let View::Picture { raw, .. } = view(&json!({})).unwrap() else { panic!("picture") };
        assert!(raw.width > 0 && raw.height > 0 && raw.bgrx.len() == raw.width as usize * raw.height as usize * 4);
        let View::Windows(windows) = view(&json!({"list":true})).unwrap() else { panic!("list") };
        eprintln!(
            "{}×{}, {} windows: {}",
            raw.width,
            raw.height,
            windows.len(),
            windows.iter().filter(|w| w["visible"] == true).map(|w| w["name"].to_string()).collect::<Vec<_>>().join(", ")
        );
        assert!(!windows.is_empty());
    }
}
