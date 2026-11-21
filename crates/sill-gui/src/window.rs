//! The window itself: where it goes in each mode and what the person made of it
//! (`geometry.rs`), showing and hiding it, Interface scale, always on top and the mode.
use std::time::Instant;

use iced::Task;

use crate::{App, Message, Read, blocking, desktop, geometry, layout, settings};

impl App {
    /// Put the window where it belongs in its mode (`geometry::target`), off the window's thread:
    /// the displays read as they are now, the window placed and held to its mode's limits. What
    /// it was placed with comes back (`Message::Placed`).
    fn placing(&self) -> impl Fn() -> Option<(Vec<geometry::Display>, Option<geometry::Target>)> + Send + Clone + 'static {
        let (xid, mode, settings) = (self.xid, self.layout.mode, self.settings.clone());
        move || {
            let xid = xid?;
            let displays = desktop::displays()?;
            let target = geometry::target(mode, &settings, &displays);
            if let Some(t) = &target {
                desktop::place(xid, t.rect, t.limits);
            }
            Some((displays, target))
        }
    }

    /// Show the window: placed, mapped, placed again, then activated with the time of the press
    /// that asked for it (0 when none did).
    pub(crate) fn show(&mut self, time: u32) -> Task<Message> {
        let Some(id) = self.window_id else {
            return Task::none();
        };
        self.visible = true;
        let xid = self.xid;
        let place = self.placing();
        let again = place.clone();
        blocking(
            move || {
                if let Some(xid) = xid {
                    desktop::stamp(xid);
                }
                place()
            },
            Message::Placed,
        )
        .chain(iced::window::set_mode(id, iced::window::Mode::Windowed))
        .chain(blocking(
            move || {
                let xid = xid?;
                // the window manager maps it first; then it is placed again and activated —
                // asked again a moment later, should the first ask come before it was managed
                std::thread::sleep(std::time::Duration::from_millis(60));
                let placed = again();
                desktop::activate(xid, time);
                // the window manager now and then lets the request go by, or gives the focus
                // back to the window before: asked again, and the focus set directly, until
                // the window is active — for a little over a second, never once it is
                let mut asks = 1;
                for wait in [200u64, 250, 300, 400] {
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                    if desktop::is_active(xid) {
                        break;
                    }
                    asks += 1;
                    desktop::activate(xid, time);
                    desktop::focus(xid);
                }
                // to the journal when it took more than one ask: how often the window
                // manager lets a request go by
                if asks > 1 {
                    eprintln!("sill-gui: brought up: {} after {asks} asks", if desktop::is_active(xid) { "active" } else { "still not active" });
                }
                placed
            },
            Message::Placed,
        ))
        // the Mac: the app made active and the window key and in front (X11 is asked above)
        .chain(if cfg!(target_os = "macos") { iced::window::gain_focus(id) } else { Task::none() })
        // and the keyboard in the session's prompt
        .chain(self.focus_prompt())
    }

    /// Hide the window; it goes on, and the hotkey or `sill toggle` shows it again. The window is
    /// read first: a change the person made to it is kept before it goes.
    pub(crate) fn hide(&mut self) -> Task<Message> {
        if self.window_id.is_none() {
            return Task::none();
        }
        self.visible = false;
        match self.xid {
            Some(xid) => blocking(move || desktop::snapshot(xid), |read| Message::Geometry(Read::Hide, read)),
            None => self.hidden(),
        }
    }

    /// The settings saved, and the window unmapped.
    fn hidden(&mut self) -> Task<Message> {
        self.save_settings();
        match self.window_id {
            Some(id) => iced::window::set_mode(id, iced::window::Mode::Hidden),
            None => Task::none(),
        }
    }

    /// The hotkey's toggle: shown and active, it hides; shown behind others, it comes up; hidden,
    /// it shows.
    pub(crate) fn toggle(&mut self, time: u32) -> Task<Message> {
        if !self.visible {
            return self.show(time);
        }
        let Some(xid) = self.xid else {
            return self.hide();
        };
        blocking(move || desktop::is_active(xid), move |active| Message::ToggleDecided(active, time))
    }

    /// The window where it belongs (`placing`) on X11; elsewhere Shelf mode's width rule
    /// (`fit_without_x11`).
    pub(crate) fn place(&self) -> Task<Message> {
        if self.xid.is_none() {
            return self.fit_without_x11(true);
        }
        blocking(self.placing(), Message::Placed)
    }

    /// Placed for the current mode after a change into or out of Shelf mode (`switching`).
    pub(crate) fn place_switch(&mut self) -> Task<Message> {
        self.switching = Some(self.layout.mode);
        blocking(self.placing(), Message::SwitchPlaced)
    }

    /// The displays and the window as read (`Message::Geometry`): the regular look, or the last
    /// one before the window hides or leaves a mode, when the person's change is kept at once.
    pub(crate) fn geometry_read(&mut self, why: Read, read: Option<(Vec<geometry::Display>, Option<geometry::Rect>)>) -> Task<Message> {
        match why {
            Read::Poll => {
                // hidden since it was asked: showing reads the displays afresh
                let Some((displays, window)) = read.filter(|_| self.visible) else {
                    return Task::none();
                };
                let target = geometry::target(self.layout.mode, &self.settings, &displays);
                match self.geometry.observe(displays, window, target.as_ref(), Instant::now()) {
                    geometry::Step::Nothing => {}
                    geometry::Step::Place => return self.place(),
                    geometry::Step::Keep(rect) => self.keep(self.layout.mode, rect),
                }
                Task::none()
            }
            Read::Hide => {
                self.flush_geometry(self.layout.mode, read.and_then(|(_, window)| window));
                // shown again while it was read: it stays
                if self.visible {
                    self.save_settings();
                    return Task::none();
                }
                self.hidden()
            }
            Read::Leave(old) => {
                self.flush_geometry(old, read.and_then(|(_, window)| window));
                // out of Shelf mode the window stays on the display it was on: the main one
                if old == layout::Mode::Shelf
                    && let Some(main) = geometry::main_display(&self.geometry.displays)
                {
                    self.settings.display = Some(main.name.clone());
                }
                self.save_settings();
                self.place_switch()
            }
        }
    }

    /// The person's own geometry in `mode`, kept (`geometry::remember`).
    fn keep(&mut self, mode: layout::Mode, rect: geometry::Rect) {
        if geometry::remember(mode, &mut self.settings, &self.geometry.displays, rect) {
            self.save_settings();
        }
    }

    /// The person's last change kept before the window hides, changes mode or quits: `window` as
    /// read now, else as last seen.
    pub(crate) fn flush_geometry(&mut self, mode: layout::Mode, window: Option<geometry::Rect>) {
        if let Some(rect) = self.geometry.flush(window, Instant::now()) {
            self.keep(mode, rect);
        }
    }

    /// Shelf mode's share on the main display, as Settings shows it.
    pub fn shelf_share(&self) -> f32 {
        geometry::main_display(&self.geometry.displays).map_or_else(|| self.settings.shelf_share(), |main| self.settings.shelf_share_on(&main.name))
    }

    /// Interface scale changes now: everything inside the window grows or shrinks, the window
    /// itself stays where it is, as large as it was. iced's sizes are in units of the scale, so
    /// the window's and the monitor's are converted to the new one. On X11 the window's place,
    /// size and limits are device pixels and stay as they are; without X11 Shelf mode is fitted
    /// again once iced reports the monitor in the new units.
    pub(crate) fn set_scale(&mut self, scale: f32) -> Task<Message> {
        let (old, new) = (self.settings.scale(), settings::valid_scale(scale));
        if old == new {
            return Task::none();
        }
        let ratio = old / new;
        self.width *= ratio;
        self.height *= ratio;
        self.monitor = self.monitor.map(|m| iced::Size::new(m.width * ratio, m.height * ratio));
        self.settings.interface_scale = Some(new);
        self.save_settings();
        match self.window_id {
            Some(id) if self.no_x11 => iced::window::monitor_size(id).map(Message::Monitor),
            _ => Task::none(),
        }
    }

    pub(crate) fn set_always_on_top(&mut self, on: bool) -> Task<Message> {
        self.settings.always_on_top = Some(on);
        self.save_settings();
        let Some(id) = self.window_id else {
            return Task::none();
        };
        iced::window::set_level(id, if on { iced::window::Level::AlwaysOnTop } else { iced::window::Level::Normal })
    }

    pub(crate) fn save_settings(&mut self) {
        self.settings.mode = Some(self.layout.mode.name().into());
        self.settings.drawer_width = self.layout.drawer;
        if self.persist {
            settings::save(&self.settings);
        }
    }

    /// Without X11 (Wayland) there are no displays to read and no place to set: Shelf mode is held
    /// to its share of the monitor iced reports (a maximum the compositor may honour), and Drawer
    /// and Full get half its width when entered. Nothing is kept per display.
    pub(crate) fn fit_without_x11(&self, entering: bool) -> Task<Message> {
        if self.layout.plain {
            return Task::none();
        }
        let (Some(id), Some(monitor), true) = (self.window_id, self.monitor, self.no_x11) else {
            return Task::none();
        };
        if self.layout.mode == layout::Mode::Shelf {
            let width = (monitor.width * self.settings.shelf_share()).round();
            let max = iced::Size::new((monitor.width * layout::SHELF_MAX).round(), monitor.height.max(self.height));
            return iced::window::set_max_size(id, Some(max)).chain(iced::window::resize(id, iced::Size::new(width, self.height)));
        }
        let unlimited = iced::window::set_max_size(id, None);
        if !entering {
            return unlimited;
        }
        let width = (monitor.width * layout::WORK_SHARE).round();
        unlimited.chain(iced::window::resize(id, iced::Size::new(width, self.height)))
    }

    /// Into `mode`: the drawer in Drawer mode only (a quick look elsewhere closes), the window
    /// from the Shelf's edge to its working size and place and back; Drawer ⇄ Full keep the
    /// window as it is.
    pub(crate) fn set_mode(&mut self, mode: layout::Mode) -> Task<Message> {
        use layout::Mode;
        let old = self.layout.mode;
        if old == mode {
            return Task::none();
        }
        if mode == Mode::Full {
            self.layout.before_full = old;
        }
        self.layout.mode = mode;
        let opening = mode == Mode::Drawer && !self.drawer.shown;
        self.drawer.shown = mode == Mode::Drawer;
        self.save_settings();
        let mut tasks = vec![self.restore_scroll()];
        if opening && let Some(id) = self.selected.clone() {
            tasks.push(self.files_refresh(&id));
        }
        if (old == Mode::Shelf) != (mode == Mode::Shelf) {
            // the window read first, so what the person made of it in the old mode is kept; then
            // placed where the new mode has it
            match self.xid {
                // a change already under way: the window is still as that one found it, and is
                // placed for the mode current when it lands (`Read::Leave`, `SwitchPlaced`)
                Some(_) if self.switching.is_some() => {}
                Some(xid) => {
                    self.switching = Some(mode);
                    tasks.push(blocking(move || desktop::snapshot(xid), move |read| Message::Geometry(Read::Leave(old), read)));
                }
                None => tasks.push(self.place()),
            }
        }
        Task::batch(tasks)
    }
}
