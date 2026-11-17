//! The tray on the Mac: an item in the menu bar, by `tray-icon`. The picture is a template (the
//! herring and its sill without the plate), so the menu bar tints it for its own light or dark.
//! Everything here happens on the main thread, where the window's messages are handled; the
//! moving icon is stepped by the window's own clock (`Tray::tick`).
use std::cell::{Cell, RefCell};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{Asked, FRAMES, TRAY_ICON, frame_svg, rgba};

/// The picture's side in pixels: the menu bar's 22 points on a Retina display.
const SIZE: u32 = 44;

/// The running item; dropping it removes the icon.
pub struct Tray {
    icon: TrayIcon,
    /// The icon at rest, then the swell's frames once an agent has worked.
    frames: RefCell<Vec<Icon>>,
    frame: Cell<usize>,
    busy: Cell<bool>,
}

impl Tray {
    pub fn shutdown(&self) {
        let _ = self.icon.set_visible(false);
    }

    /// An agent is in a turn (or none is): the icon moves while `on` (`tick`), rests otherwise.
    pub fn set_busy(&self, on: bool) {
        self.busy.set(on);
        if !on {
            self.show(0);
        }
    }

    /// The next frame of the swell, while an agent works.
    pub fn tick(&self) {
        if !self.busy.get() {
            return;
        }
        // the swell's frames, drawn the first time an agent works
        if self.frames.borrow().len() == 1 {
            let swell = (1..=FRAMES).filter_map(|i| picture(&frame_svg(i as f64 / FRAMES as f64)));
            self.frames.borrow_mut().extend(swell);
        }
        self.show(self.frame.get() % FRAMES + 1);
    }

    fn show(&self, frame: usize) {
        if self.frame.replace(frame) == frame {
            return;
        }
        let frames = self.frames.borrow();
        if let Some(icon) = frames.get(frame).or_else(|| frames.first()) {
            let _ = self.icon.set_icon_templated(Some(icon.clone()));
        }
    }
}

/// The tray's SVG as the menu bar's template: the plate gone, the herring and the sill left for
/// the menu bar to tint.
fn picture(svg: &str) -> Option<Icon> {
    let plate = svg.find(r#"<rect x="4" y="4""#)?;
    let end = plate + svg[plate..].find("/>")? + 2;
    let bare = format!("{}{}", &svg[..plate], &svg[end..]);
    Icon::from_rgba(rgba(&bare, SIZE)?, SIZE, SIZE).ok()
}

/// Put the icon in the menu bar; `None` (said on stderr) when it cannot be made.
pub fn start(asked: async_channel::Sender<Asked>) -> Option<Tray> {
    if objc2::MainThreadMarker::new().is_none() {
        eprintln!("sill-gui: no menu bar icon: not on the main thread");
        return None;
    }
    let toggle = MenuItem::new("Show / hide sill", true, None);
    // with the window's own VM, quitting ends it, and the sessions in it
    let quit = MenuItem::new(if crate::local_vm::phase().is_some() { "Quit Sill" } else { "Quit the window (sessions go on)" }, true, None);
    let menu = Menu::new();
    if let Err(e) = menu.append_items(&[&toggle, &PredefinedMenuItem::separator(), &quit]) {
        eprintln!("sill-gui: no menu bar icon: {e}");
        return None;
    }
    let (toggle, quit) = (toggle.id().clone(), quit.id().clone());
    let chosen = asked.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == toggle {
            let _ = chosen.send_blocking(Asked::Toggle);
        } else if event.id == quit {
            let _ = chosen.send_blocking(Asked::Quit);
        }
    }));
    // a click shows or hides the window, as on Linux; the menu is on the other button
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            let _ = asked.send_blocking(Asked::Toggle);
        }
    }));
    let rest = picture(TRAY_ICON)?;
    let built = TrayIconBuilder::new()
        .with_icon_templated(rest.clone())
        .with_tooltip("Sill: click to show or hide")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build();
    match built {
        Ok(icon) => Some(Tray { icon, frames: RefCell::new(vec![rest]), frame: Cell::new(0), busy: Cell::new(false) }),
        Err(e) => {
            eprintln!("sill-gui: no menu bar icon: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_template_is_the_herring_without_its_plate() {
        let svg = super::TRAY_ICON;
        let plate = svg.find(r#"<rect x="4" y="4""#).expect("the plate");
        let end = plate + svg[plate..].find("/>").unwrap() + 2;
        let bare = format!("{}{}", &svg[..plate], &svg[end..]);
        let pixels = super::rgba(&bare, super::SIZE).expect("drawn");
        // a corner the plate covered is clear; the sill is not
        assert_eq!(pixels[(6 * super::SIZE as usize + 6) * 4 + 3], 0);
        assert!(pixels.chunks(4).any(|p| p[3] > 0));
    }
}
