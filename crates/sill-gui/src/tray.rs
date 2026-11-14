//! The tray icon (the GTK shelf's `shelf/tray.rs`): a StatusNotifierItem — the freedesktop/KDE
//! protocol current Linux panels take (Cinnamon, KDE, XFCE, MATE, LXQt, Budgie; GNOME with the
//! AppIndicator extension) — over the session bus, by `ksni` (pure Rust, no GTK). A click shows,
//! raises or hides the window as the hotkey does; its menu has the same and Quit, which ends
//! this window (the daemon and its sessions go on). The item registers again when the panel's
//! watcher comes back; with no panel to show it, there is no icon and the hotkey remains.
//!
//! The icon is the herring on its sill (`sill-tray.svg`, not the application icon). While an
//! agent works it moves as the shelf's does — the sill ripples, the fish rides it — a frame every
//! 125 ms on a thread of its own; still, and no thread, when every session is idle.
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ksni::blocking::TrayMethods as _;

/// What the tray asks of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    Toggle,
    Quit,
}

/// The application icon (the herring in a jar): the window's icon, as the panel's and menu's.
pub const APP_ICON: &str = include_str!("../../../packaging/icons/sill.svg");
/// The tray's own picture, the herring on its sill, drawn at the sizes panels pick from.
const TRAY_ICON: &str = include_str!("../../../packaging/icons/sill-tray.svg");
const SIZES: [u32; 4] = [22, 24, 32, 48];
/// The moving icon: this many frames round one swell, this long each (≈ 8/s, a 1.5 s swell).
const FRAMES: usize = 12;
const FRAME_MS: u64 = 125;

struct SillTray {
    /// The icon at rest, then the swell's frames once an agent has worked.
    frames: Vec<Vec<ksni::Icon>>,
    frame: usize,
    asked: async_channel::Sender<Asked>,
}

impl ksni::Tray for SillTray {
    fn id(&self) -> String {
        "sill".into()
    }

    fn title(&self) -> String {
        "Sill".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.frames
            .get(self.frame)
            .or_else(|| self.frames.first())
            .cloned()
            .unwrap_or_default()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Sill".into(),
            description: "Click to show or hide".into(),
            ..ksni::ToolTip::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.asked.send_blocking(Asked::Toggle);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show / hide sill".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.asked.send_blocking(Asked::Toggle);
                }),
                ..StandardItem::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit the window (sessions go on)".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.asked.send_blocking(Asked::Quit);
                }),
                ..StandardItem::default()
            }
            .into(),
        ]
    }
}

/// The running item; dropping it removes the icon.
pub struct Tray {
    handle: Arc<ksni::blocking::Handle<SillTray>>,
    /// An agent works: the icon moves.
    busy: Arc<AtomicBool>,
    /// The thread that moves it runs.
    ticking: Arc<AtomicBool>,
}

impl Tray {
    pub fn shutdown(&self) {
        self.busy.store(false, Ordering::Relaxed);
        self.handle.shutdown().wait();
    }

    /// An agent is in a turn (or none is): the icon moves while `on`, rests otherwise.
    pub fn set_busy(&self, on: bool) {
        self.busy.store(on, Ordering::Relaxed);
        if !on || self.ticking.swap(true, Ordering::Relaxed) {
            return; // the ticker sees `busy` drop and puts the still icon back
        }
        let (handle, busy, ticking) =
            (self.handle.clone(), self.busy.clone(), self.ticking.clone());
        let spawned = std::thread::Builder::new()
            .name("sill-gui-tray".into())
            .spawn(move || {
                // the swell's frames, drawn the first time an agent works
                if handle.update(|t| t.frames.len()) == Some(1) {
                    let swell: Vec<Vec<ksni::Icon>> = (1..=FRAMES)
                        .map(|i| frame_icons(&frame_svg(i as f64 / FRAMES as f64)))
                        .collect();
                    handle.update(|t| t.frames.extend(swell));
                }
                loop {
                    let moving = busy.load(Ordering::Relaxed);
                    let shown = handle.update(|t| {
                        t.frame = if moving { t.frame % FRAMES + 1 } else { 0 };
                    });
                    if shown.is_none() || !moving {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(FRAME_MS));
                }
                ticking.store(false, Ordering::Relaxed);
            });
        if spawned.is_err() {
            self.ticking.store(false, Ordering::Relaxed);
        }
    }
}

/// One picture at every size the panel picks from.
fn frame_icons(svg: &str) -> Vec<ksni::Icon> {
    SIZES.iter().filter_map(|size| pixmap(svg, *size)).collect()
}

/// The tray picture at `phase` (0..1) of its swell, as the shelf draws it (`shelf/tray.rs`): the
/// sill a low wave travelling right, the fish lifted by the crest under it and leaning into the
/// slope. Same plate, colours and sizes as `sill-tray.svg`.
fn frame_svg(phase: f64) -> String {
    let t = 2.0 * PI * phase;
    // one and a half waves along the sill, 0.9 px high: a ripple, not a sea
    let wave = |x: f64| 45.0 + 0.9 * (2.0 * PI * 1.5 * (x - 10.0) / 44.0 - t).sin();
    let mut d = String::new();
    for i in 0..=22 {
        let x = 10.0 + 2.0 * i as f64;
        d.push_str(&format!(
            "{}{x:.1} {:.2}",
            if i == 0 { "M" } else { " L" },
            wave(x)
        ));
    }
    // the fish sits over x≈13..50: it rides the water at its middle and tilts with the slope there
    let (fx, fy) = (
        10.4 + 0.5 * (t + 0.5 * PI).sin(),
        8.8 + (wave(32.0) - 45.0) * 0.8,
    );
    let tilt = 1.4 * (t - 0.3 * PI).sin();
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64"><rect x="4" y="4" width="56" height="56" rx="12" fill="#2b2f36"/><path d="{d}" fill="none" stroke="#e8e8e8" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/><g transform="translate({fx:.2} {fy:.2}) rotate({tilt:.2} 21.6 21.6) scale(1.8)"><path fill="#e8e8e8" d="M1.5 12C4.5 6.5 12 5 17.5 10.5L22.5 6.5 20.5 12 22.5 17.5 17.5 13.5C12 19 4.5 17.5 1.5 12ZM4.7 11a1.3 1.3 0 1 0 2.6 0a1.3 1.3 0 1 0-2.6 0"/></g></svg>"##
    )
}

/// Put the icon in the panel; `None` (said on stderr) without a session bus.
pub fn start(asked: async_channel::Sender<Asked>) -> Option<Tray> {
    let tray = SillTray {
        frames: vec![frame_icons(TRAY_ICON)],
        frame: 0,
        asked,
    };
    match tray.spawn() {
        Ok(handle) => Some(Tray {
            handle: Arc::new(handle),
            busy: Arc::new(AtomicBool::new(false)),
            ticking: Arc::new(AtomicBool::new(false)),
        }),
        Err(e) => {
            eprintln!("sill-gui: no tray icon: {e}");
            None
        }
    }
}

/// An SVG drawn `size` square: straight-alpha RGBA, row by row.
pub fn rgba(svg: &str, size: u32) -> Option<Vec<u8>> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let mut canvas = tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut canvas.as_mut(),
    );
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for pixel in canvas.pixels() {
        let c = pixel.demultiply();
        data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(data)
}

/// An SVG drawn `size` square, as the item's pixmap: ARGB32 in network byte order, straight alpha.
fn pixmap(svg: &str, size: u32) -> Option<ksni::Icon> {
    let mut data = rgba(svg, size)?;
    for p in data.as_chunks_mut::<4>().0 {
        p.rotate_right(1); // RGBA → ARGB
    }
    Some(ksni::Icon {
        width: size as i32,
        height: size as i32,
        data,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_icons_draw_at_every_size() {
        for svg in [super::TRAY_ICON, super::APP_ICON] {
            for size in super::SIZES {
                let icon = super::pixmap(svg, size).expect("drawn");
                assert_eq!(icon.data.len(), (size * size * 4) as usize);
                assert!(icon.data.chunks(4).any(|p| p[0] > 0), "not blank");
            }
        }
    }

    /// gdk-pixbuf (Cinnamon's panel and menu) tells an SVG by its opening tag in the file's first
    /// bytes: pushed past them by a long comment, the panel drew an empty grey button (2026-11-05).
    #[test]
    fn the_icons_open_early_as_svg() {
        for svg in [super::TRAY_ICON, super::APP_ICON] {
            let at = svg.find("<svg").expect("an svg");
            assert!(at < 100, "<svg> at byte {at}");
        }
    }

    #[test]
    fn the_swell_draws_and_returns_to_rest() {
        for i in 0..=super::FRAMES {
            let frame = super::frame_icons(&super::frame_svg(i as f64 / super::FRAMES as f64));
            assert_eq!(frame.len(), super::SIZES.len(), "frame {i}");
        }
        // a whole swell ends where it began
        assert_eq!(
            super::frame_svg(0.0),
            super::frame_svg(1.0).replace("-0.00", "0.00")
        );
    }
}
