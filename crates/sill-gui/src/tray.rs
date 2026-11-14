//! The tray icon: the herring on its sill (`sill-tray.svg`, not the application icon). A click
//! shows, raises or hides the window as the hotkey does; its menu has the same and Quit, which
//! ends this window (the daemon and its sessions go on). While an agent works the icon moves —
//! the sill ripples, the fish rides it — a frame every 125 ms; still when every session is idle.
//!
//! On Linux it is a StatusNotifierItem (`tray/sni.rs`), on the Mac an item in the menu bar
//! (`tray/menu_bar.rs`): the same clicks, menu and motion.
use std::f64::consts::PI;

#[cfg(target_os = "macos")]
mod menu_bar;
#[cfg(not(target_os = "macos"))]
mod sni;

#[cfg(target_os = "macos")]
pub use menu_bar::{Tray, start};
#[cfg(not(target_os = "macos"))]
pub use sni::{Tray, start};

/// What the tray asks of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    Toggle,
    Quit,
}

/// The application icon (the herring in a jar): the window's icon, as the panel's and menu's.
pub const APP_ICON: &str = include_str!("../../../packaging/icons/sill.svg");
/// The tray's own picture, the herring on its sill, drawn at the sizes panels pick from.
pub(crate) const TRAY_ICON: &str = include_str!("../../../packaging/icons/sill-tray.svg");
/// The moving icon: this many frames round one swell, this long each (≈ 8/s, a 1.5 s swell).
pub(crate) const FRAMES: usize = 12;
pub(crate) const FRAME_MS: u64 = 125;

/// The tray picture at `phase` (0..1) of its swell: the
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

#[cfg(test)]
mod tests {
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
    fn the_swell_returns_to_rest() {
        // a whole swell ends where it began
        assert_eq!(
            super::frame_svg(0.0),
            super::frame_svg(1.0).replace("-0.00", "0.00")
        );
    }
}
