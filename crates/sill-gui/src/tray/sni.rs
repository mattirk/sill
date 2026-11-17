//! The tray on Linux: a StatusNotifierItem — the freedesktop/KDE protocol current Linux panels
//! take (Cinnamon, KDE, XFCE, MATE, LXQt, Budgie; GNOME with the AppIndicator extension) — over
//! the session bus, by `ksni` (pure Rust). The item registers again when the panel's watcher
//! comes back; with no panel to show it, there is no icon and the hotkey remains. The moving
//! icon is drawn on a thread of its own; no thread when every session is idle.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ksni::blocking::TrayMethods as _;

use super::{Asked, FRAME_MS, FRAMES, TRAY_ICON, frame_svg, rgba};

/// The sizes panels pick from.
const SIZES: [u32; 4] = [22, 24, 32, 48];

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
        self.frames.get(self.frame).or_else(|| self.frames.first()).cloned().unwrap_or_default()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Sill".into(), description: "Click to show or hide".into(), ..ksni::ToolTip::default() }
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
        let (handle, busy, ticking) = (self.handle.clone(), self.busy.clone(), self.ticking.clone());
        let spawned = std::thread::Builder::new().name("sill-gui-tray".into()).spawn(move || {
            // the swell's frames, drawn the first time an agent works
            if handle.update(|t| t.frames.len()) == Some(1) {
                let swell: Vec<Vec<ksni::Icon>> = (1..=FRAMES).map(|i| frame_icons(&frame_svg(i as f64 / FRAMES as f64))).collect();
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

/// Put the icon in the panel; `None` (said on stderr) without a session bus.
pub fn start(asked: async_channel::Sender<Asked>) -> Option<Tray> {
    let tray = SillTray { frames: vec![frame_icons(TRAY_ICON)], frame: 0, asked };
    match tray.spawn() {
        Ok(handle) => Some(Tray { handle: Arc::new(handle), busy: Arc::new(AtomicBool::new(false)), ticking: Arc::new(AtomicBool::new(false)) }),
        Err(e) => {
            eprintln!("sill-gui: no tray icon: {e}");
            None
        }
    }
}

/// An SVG drawn `size` square, as the item's pixmap: ARGB32 in network byte order, straight alpha.
fn pixmap(svg: &str, size: u32) -> Option<ksni::Icon> {
    let mut data = rgba(svg, size)?;
    for p in data.as_chunks_mut::<4>().0 {
        p.rotate_right(1); // RGBA → ARGB
    }
    Some(ksni::Icon { width: size as i32, height: size as i32, data })
}

#[cfg(test)]
mod tests {
    use super::super::{APP_ICON, FRAMES, TRAY_ICON, frame_svg};

    #[test]
    fn the_icons_draw_at_every_size() {
        for svg in [TRAY_ICON, APP_ICON] {
            for size in super::SIZES {
                let icon = super::pixmap(svg, size).expect("drawn");
                assert_eq!(icon.data.len(), (size * size * 4) as usize);
                assert!(icon.data.chunks(4).any(|p| p[0] > 0), "not blank");
            }
        }
    }

    #[test]
    fn the_swell_draws() {
        for i in 0..=FRAMES {
            let frame = super::frame_icons(&frame_svg(i as f64 / FRAMES as f64));
            assert_eq!(frame.len(), super::SIZES.len(), "frame {i}");
        }
    }
}
