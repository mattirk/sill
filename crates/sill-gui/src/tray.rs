//! The tray icon (the GTK shelf's `shelf/tray.rs`): a StatusNotifierItem — the freedesktop/KDE
//! protocol current Linux panels take (Cinnamon, KDE, XFCE, MATE, LXQt, Budgie; GNOME with the
//! AppIndicator extension) — over the session bus, by `ksni` (pure Rust, no GTK). A click shows,
//! raises or hides the window as the hotkey does; its menu has the same and Quit, which ends
//! this window (the daemon and its sessions go on). The item registers again when the panel's
//! watcher comes back; with no panel to show it, there is no icon and the hotkey remains.
use ksni::blocking::TrayMethods as _;

/// What the tray asks of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    Toggle,
    Quit,
}

/// The logo, drawn at the sizes panels pick from.
const LOGO: &str = include_str!("../../../packaging/icons/sill.svg");
const SIZES: [u32; 4] = [22, 24, 32, 48];

struct SillTray {
    icon: Vec<ksni::Icon>,
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
        self.icon.clone()
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
pub struct Tray(ksni::blocking::Handle<SillTray>);

impl Tray {
    pub fn shutdown(&self) {
        self.0.shutdown().wait();
    }
}

/// Put the icon in the panel; `None` (said on stderr) without a session bus.
pub fn start(asked: async_channel::Sender<Asked>) -> Option<Tray> {
    let tray = SillTray {
        icon: SIZES
            .iter()
            .filter_map(|size| pixmap(LOGO, *size))
            .collect(),
        asked,
    };
    match tray.spawn() {
        Ok(handle) => Some(Tray(handle)),
        Err(e) => {
            eprintln!("sill-gui: no tray icon: {e}");
            None
        }
    }
}

/// An SVG drawn `size` square, as the item's pixmap: ARGB32 in network byte order, straight alpha.
fn pixmap(svg: &str, size: u32) -> Option<ksni::Icon> {
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
        data.extend_from_slice(&[c.alpha(), c.red(), c.green(), c.blue()]);
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
    fn the_logo_draws_at_every_size() {
        for size in super::SIZES {
            let icon = super::pixmap(super::LOGO, size).expect("drawn");
            assert_eq!(icon.data.len(), (size * size * 4) as usize);
            assert!(icon.data.chunks(4).any(|p| p[0] > 0), "not blank");
        }
    }
}
