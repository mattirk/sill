//! Remote boxes, in Settings › System (docs/server.md): add a Linux box running Silli, open a
//! window on a saved one, disconnect or remove it. What `silli box add|connect|disconnect|remove`
//! do in a terminal, for a desktop without the `silli` command (the Mac app). The boxes are the
//! frontend's own preferences (`~/.config/silli/boxes.json`, `silli_proto::transport`).
//!
//! A window serves one box (`SILLI_BOX`): Open starts this executable again on the chosen box,
//! with a control socket of its own beside this machine's window (`control::box_socket_path`),
//! so this window — and on the Mac its VM — stays as it is. A window already open on that box is
//! shown instead.
//!
//! A saved box can be pinned: it then has a row of its own in the `+` menu, which opens its
//! window as Open here does. Pins are silli-gui's settings (`pinned_boxes`), shared by every
//! window.
use std::collections::{BTreeMap, BTreeSet};

use iced::widget::{Column, column, container, row, text, text_input};
use iced::{Alignment, Element, Fill, Task};
use silli_proto::boxlink;
use silli_proto::transport::{self, BoxTarget};

use crate::settings_ui::quiet_button;
use crate::style::{self, P};
use crate::{App, Message, Page, blocking};

/// The card's title (and its key among the Settings page's folded cards).
pub const TITLE: &str = "Remote boxes";
/// Set on a window Open starts: it comes up in front and focused (`main.rs`, `RAISE`).
pub const RAISE_ENV: &str = "SILLI_GUI_RAISE";
/// Where `packaging/deploy-server.sh` installs Silli on a box (docs/server.md).
pub const DEFAULT_BINARY: &str = "/usr/local/bin/silli";

pub struct State {
    /// The saved boxes, as last read.
    pub(crate) saved: Result<BTreeMap<String, BoxTarget>, String>,
    address: String,
    name: String,
    binary: String,
    /// An Add is signing in or checking the box.
    adding: bool,
    /// Boxes whose shared connection is being closed.
    disconnecting: BTreeSet<String>,
    /// Remove was pressed once for this box.
    armed_remove: Option<String>,
    /// What the last action did, or why it did not.
    said: Option<Result<String, String>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            // a window on a saved box names its host in the top bar from the start
            saved: if current().is_some() { transport::list().map_err(|e| format!("{e:#}")) } else { Ok(BTreeMap::new()) },
            address: String::new(),
            name: String::new(),
            binary: String::new(),
            adding: false,
            disconnecting: BTreeSet::new(),
            armed_remove: None,
            said: None,
        }
    }
}

/// A box added: what its daemon said.
#[derive(Debug, Clone)]
pub struct Added {
    name: String,
    version: String,
    sessions: usize,
    /// SSH logs in without asking (an agent or an unencrypted key).
    unattended: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Read the saved boxes again.
    Reload,
    /// Show this card: Settings › System.
    Show,
    Address(String),
    Name(String),
    Binary(String),
    Add,
    Added(Result<Added, String>),
    /// Open a window on a saved box; None for this machine's own.
    Open(Option<String>),
    Disconnect(String),
    Disconnected(String, Result<(), String>),
    Remove(String),
    /// Pin a saved box to the `+` menu, or unpin it.
    Pin(String),
}

fn event(event: Event) -> Message {
    Message::Boxes(event)
}

/// The machine's own box, as the `+` menu names it.
pub(crate) fn this_machine_name() -> &'static str {
    this_machine()
}

/// The saved box this window serves; None for this machine's own daemon (on the Mac, its VM).
pub fn current() -> Option<String> {
    std::env::var("SILLI_BOX").ok().filter(|name| !name.is_empty() && name != "local")
}

/// What this machine's own box is called here.
fn this_machine() -> &'static str {
    if cfg!(target_os = "macos") { "This Mac" } else { "This computer" }
}

/// Start a window on `name` (None: this machine's own), beside this one.
fn open_window(name: Option<&str>) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let mut command = std::process::Command::new(&exe);
    command.env_remove("SILLI_BOX").env_remove("SILLI_GUI_SOCKET").env(RAISE_ENV, "1");
    if let Some(name) = name {
        command.env("SILLI_BOX", name).env("SILLI_GUI_SOCKET", crate::control::box_socket_path(name));
    }
    // its own process group: a signal to this window's group leaves the other window
    command.stdin(std::process::Stdio::null()).process_group(0);
    #[cfg(target_os = "macos")]
    crate::macos::yield_activation();
    let mut child = command.spawn().map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    // the Mac: the new window to the front once it runs (X11: it asks the window manager itself)
    #[cfg(target_os = "macos")]
    crate::macos::activate_when_running(child.id());
    std::thread::Builder::new()
        .name("silli-gui-window".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Sign in to `host` with the window's prompter, check its daemon and save it. A failure closes
/// the shared connection only when this sign-in made it: one that was already there carries
/// other windows' and commands' connections to that host, which a failed add must not end.
///
/// `in_use`: this window works on `host`, so a failed sign-in stays on its band.
fn add(name: String, host: String, binary: String, prompter: boxlink::Prompter, in_use: bool) -> Result<Added, String> {
    let made = match boxlink::sign_in(&host, Some(prompter)) {
        Ok(made) => made,
        Err(error) => {
            // nothing of anyone else's to keep (a master there would have been reused); clear
            // the failure so the band does not offer a sign-in to a box this window is not on
            if !in_use {
                let _ = boxlink::disconnect(&host);
            }
            // the band's own advice (Sign in, `silli box add`) does not fit here
            return Err(match error.downcast_ref::<boxlink::Unavailable>() {
                Some(why) => format!("could not sign in to {host}: {}", why.detail),
                None => format!("{error:#}"),
            });
        }
    };
    let result = transport::verify_and_save(&name, &host, &binary).map(|status| Added {
        name,
        version: status.version,
        sessions: status.sessions,
        unattended: boxlink::unattended(&host).unwrap_or(true),
    });
    if result.is_err() && made {
        let _ = boxlink::disconnect(&host);
    }
    result.map_err(|e| format!("{e:#}"))
}

impl App {
    pub(crate) fn boxes_event(&mut self, event: Event) -> Task<Message> {
        let state = &mut self.boxes;
        match event {
            Event::Reload => {
                state.saved = transport::list().map_err(|e| format!("{e:#}"));
            }
            Event::Show => {
                self.settings.settings_category = crate::settings_ui::Category::System;
                if let Some(page) = &mut self.settings_page {
                    page.category = crate::settings_ui::Category::System;
                    page.collapsed.remove(TITLE);
                    return Task::none();
                }
                return self.show_page(Page::Settings);
            }
            Event::Address(address) => state.address = address,
            Event::Name(name) => state.name = name,
            Event::Binary(binary) => state.binary = binary,
            Event::Add => {
                if state.adding {
                    return Task::none();
                }
                let host = state.address.trim().to_string();
                if host.is_empty() {
                    state.said = Some(Err("Enter the box's SSH address, such as user@192.168.1.50 or an alias from ~/.ssh/config.".into()));
                    return Task::none();
                }
                let name = match state.name.trim() {
                    "" => transport::default_name(&host),
                    name => name.to_string(),
                };
                if let Err(e) = transport::validate_name(&name) {
                    state.said = Some(Err(format!("{e}")));
                    return Task::none();
                }
                if state.saved.as_ref().is_ok_and(|saved| saved.contains_key(&name)) {
                    state.said = Some(Err(format!("A box named {name} is already saved: remove it first, or give this one another name.")));
                    return Task::none();
                }
                let binary = match state.binary.trim() {
                    "" => DEFAULT_BINARY.to_string(),
                    binary => binary.to_string(),
                };
                let Some(prompter) = self.box_link.prompter() else {
                    state.said = Some(Err("Boxes cannot be added in the demo.".into()));
                    return Task::none();
                };
                let in_use = transport::selected().ok().flatten().is_some_and(|t| t.host == host);
                state.adding = true;
                state.armed_remove = None;
                state.said = Some(Ok(format!("Signing in to {host}…")));
                return blocking(move || add(name, host, binary, prompter, in_use), |result| Message::Boxes(Event::Added(result)));
            }
            Event::Added(result) => {
                state.adding = false;
                state.saved = transport::list().map_err(|e| format!("{e:#}"));
                state.said = Some(result.map(|added| {
                    state.address.clear();
                    state.name.clear();
                    state.binary.clear();
                    let mut said = format!(
                        "Added {}: Silli {}, {} session{}. Open starts a window on it.",
                        added.name,
                        added.version,
                        added.sessions,
                        if added.sessions == 1 { "" } else { "s" },
                    );
                    if !added.unattended {
                        said.push_str(" SSH asks for a sign-in each time the connection is made again (after a sleep, a reboot of the box or a longer network loss); with an ssh-agent or an unencrypted key it reconnects by itself.");
                    }
                    said
                }));
            }
            Event::Open(name) => {
                if self.demo {
                    return Task::none();
                }
                state.armed_remove = None;
                state.said = match open_window(name.as_deref()) {
                    Ok(()) => None,
                    Err(why) => Some(Err(why)),
                };
            }
            Event::Disconnect(name) => {
                let Some(host) = state.saved.as_ref().ok().and_then(|s| s.get(&name)).map(|t| t.host.clone()) else {
                    return Task::none();
                };
                if !state.disconnecting.insert(name.clone()) {
                    return Task::none();
                }
                state.armed_remove = None;
                return blocking(
                    move || boxlink::disconnect(&host).map_err(|e| format!("{e:#}")),
                    move |result| Message::Boxes(Event::Disconnected(name, result)),
                );
            }
            Event::Disconnected(name, result) => {
                state.disconnecting.remove(&name);
                state.said = Some(result.map(|()| format!("Disconnected from {name}: the next window on it signs in again.")));
            }
            Event::Remove(name) => {
                if state.armed_remove.as_deref() != Some(name.as_str()) {
                    state.armed_remove = Some(name);
                    return Task::none();
                }
                state.armed_remove = None;
                state.said =
                    Some(transport::remove(&name).map(|()| format!("Removed {name}. Its sessions go on running on the box.")).map_err(|e| format!("{e:#}")));
                state.saved = transport::list().map_err(|e| format!("{e:#}"));
                let gone = state.saved.as_ref().is_ok_and(|saved| !saved.contains_key(&name));
                if gone && self.settings.pinned_boxes.contains(&name) {
                    self.pinned_boxes_reload();
                    self.settings.pinned_boxes.retain(|pinned| *pinned != name);
                    self.save_settings();
                }
            }
            Event::Pin(name) => {
                // another window may have pinned since this one read its settings
                self.pinned_boxes_reload();
                let pins = &mut self.settings.pinned_boxes;
                if pins.contains(&name) {
                    pins.retain(|pinned| *pinned != name);
                } else {
                    pins.push(name);
                }
                self.save_settings();
            }
        }
        Task::none()
    }

    /// The pins as the settings file has them now: every window writes the same file.
    pub(crate) fn pinned_boxes_reload(&mut self) {
        if self.persist {
            self.settings.pinned_boxes = crate::settings::load().pinned_boxes;
        }
    }

    /// The pinned boxes the `+` menu offers, in pinning order: saved ones, and not the box this
    /// window serves. In a window on a box, this machine's own comes first, the way back.
    pub(crate) fn menu_boxes(&self) -> Vec<(Option<String>, String)> {
        let current = current();
        let mut boxes = Vec::new();
        if current.is_some() {
            boxes.push((None, if cfg!(target_os = "macos") { "Silli's own virtual machine" } else { "The daemon on this computer" }.to_string()));
        }
        if let Ok(saved) = &self.boxes.saved {
            for name in &self.settings.pinned_boxes {
                if current.as_deref() == Some(name.as_str()) {
                    continue;
                }
                if let Some(target) = saved.get(name) {
                    boxes.push((Some(name.clone()), target.host.clone()));
                }
            }
        }
        boxes
    }

    /// The card's rows: this machine, each saved box, then Add a box.
    pub(crate) fn boxes_rows(&self) -> Vec<Element<'_, Message>> {
        let state = &self.boxes;
        let current = current();
        let live = !self.demo;
        let mut rows = Vec::new();
        let local_note = if cfg!(target_os = "macos") { "Silli's own virtual machine" } else { "The daemon on this computer" };
        rows.push(line(this_machine().to_string(), local_note.to_string(), opener(current.is_none(), live.then(|| event(Event::Open(None))))));
        match &state.saved {
            Err(why) => rows.push(note(why.clone(), P.error)),
            Ok(saved) => {
                for (name, target) in saved {
                    let here = current.as_deref() == Some(name.as_str());
                    let busy = state.disconnecting.contains(name);
                    let armed = state.armed_remove.as_deref() == Some(name.as_str());
                    let pinned = self.settings.pinned_boxes.contains(name);
                    let mut sub = target.host.clone();
                    if target.binary != "silli" && target.binary != DEFAULT_BINARY {
                        sub.push_str(&format!(" · {}", target.binary));
                    }
                    let controls = row![
                        opener(here, live.then(|| event(Event::Open(Some(name.clone()))))),
                        iced::widget::tooltip(
                            quiet_button(if pinned { "Unpin" } else { "Pin" }, live.then(|| event(Event::Pin(name.clone())))),
                            container(
                                text(if pinned { "Take it out of the + menu" } else { "Show it in the + menu, to open its window from there" })
                                    .size(style::TINY)
                            )
                            .padding([3, 8])
                            .style(style::card(P.surface)),
                            iced::widget::tooltip::Position::Bottom,
                        ),
                        // this window's own connection: closing it would only make it again
                        quiet_button(
                            if busy { "Disconnecting…" } else { "Disconnect" },
                            (live && !here && !busy).then(|| event(Event::Disconnect(name.clone()))),
                        ),
                        quiet_button(if armed { "Remove?" } else { "Remove" }, (live && !here).then(|| event(Event::Remove(name.clone()))),),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center);
                    rows.push(line(name.clone(), sub, controls));
                }
            }
        }
        let host = state.address.trim();
        let name_hint = if host.is_empty() { "server".to_string() } else { transport::default_name(host) };
        let field = |label: &'static str, input: Element<'static, Message>| -> Element<'static, Message> {
            row![text(label).size(style::SMALL).color(P.fg_dim).width(110), input].spacing(8).align_y(Alignment::Center).into()
        };
        let input = |hint: &str, value: &str, on: fn(String) -> Event| -> Element<'static, Message> {
            let mut input = text_input(hint, value).size(style::SMALL).padding(7);
            if live && !state.adding {
                input = input.on_input(move |v| event(on(v))).on_submit(event(Event::Add));
            }
            input.into()
        };
        let form = column![
            field("SSH address", input("user@192.168.1.50 or an SSH alias", &state.address, Event::Address)),
            field("Name", input(&name_hint, &state.name, Event::Name)),
            field("Silli on the box", input(DEFAULT_BINARY, &state.binary, Event::Binary)),
            row![quiet_button(if state.adding { "Adding…" } else { "Add box" }, (live && !state.adding && !host.is_empty()).then(|| event(Event::Add)),),],
        ]
        .spacing(8);
        rows.push(
            container(
                column![
                    text("Add a box").size(style::TEXT).color(P.fg),
                    text("A Linux machine with Silli set up for your account (docs/server.md: packaging/deploy-server.sh). Silli connects with this computer's own ssh and ~/.ssh/config; a password or key passphrase is asked at the top of the window and kept nowhere.")
                        .size(style::SMALL)
                        .color(P.fg_dim),
                    form,
                ]
                .spacing(10),
            )
            .padding([12, 16])
            .into(),
        );
        if let Some(said) = &state.said {
            let (said, color) = match said {
                Ok(said) => (said.clone(), P.fg_dim),
                Err(why) => (why.clone(), P.error),
            };
            rows.push(note(said, color));
        }
        rows
    }

    /// The top bar's mark of a window on a saved box: its name, opening this card.
    pub(crate) fn box_chip(&self) -> Option<Element<'_, Message>> {
        let name = current()?;
        let host = self.boxes.saved.as_ref().ok().and_then(|saved| saved.get(&name)).map(|t| t.host.clone()).unwrap_or_default();
        let chip = crate::keys::press(
            iced::widget::button(text(name.clone()).size(style::TINY).color(P.accent)).padding([3, 8]).style(style::quiet),
            event(Event::Show),
        );
        Some(
            iced::widget::tooltip(
                chip,
                container(text(format!("This window works on the box {name} ({host}). Remote boxes in Settings")).size(style::TINY))
                    .padding([3, 8])
                    .style(style::card(P.surface)),
                iced::widget::tooltip::Position::Bottom,
            )
            .into(),
        )
    }
}

/// Open, or a dim "This window" for the box this window serves.
fn opener(here: bool, open: Option<Message>) -> Element<'static, Message> {
    if here {
        return container(text("This window").size(style::SMALL).color(P.fg_dim)).padding([6, 14]).into();
    }
    quiet_button("Open", open)
}

/// A row: the box's name over a dim line, its controls on the right.
fn line<'a>(title: String, sub: String, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(
        row![column![text(title).size(style::TEXT).color(P.fg), text(sub).size(style::SMALL).color(P.fg_dim),].spacing(3).width(Fill), control.into(),]
            .spacing(16)
            .align_y(Alignment::Center),
    )
    .padding([12, 16])
    .into()
}

fn note<'a>(said: String, color: iced::Color) -> Element<'a, Message> {
    Column::new().push(container(text(said).size(style::SMALL).color(color)).padding([10, 16])).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_explains_what_is_missing_before_touching_ssh() {
        let (mut app, _) = App::new(true, None);
        let _ = app.boxes_event(Event::Add);
        assert!(matches!(&app.boxes.said, Some(Err(why)) if why.contains("SSH address")));
        let _ = app.boxes_event(Event::Address("matti@192.168.1.50".into()));
        let _ = app.boxes_event(Event::Name("not local!".into()));
        let _ = app.boxes_event(Event::Add);
        assert!(matches!(&app.boxes.said, Some(Err(why)) if why.contains("box name")));
        let _ = app.boxes_event(Event::Name(String::new()));
        // the demo has no prompter: nothing is signed in to
        let _ = app.boxes_event(Event::Add);
        assert!(matches!(&app.boxes.said, Some(Err(why)) if why.contains("demo")));
        assert!(!app.boxes.adding);
    }

    #[test]
    fn a_saved_name_is_not_overwritten_and_remove_asks_twice() {
        let (mut app, _) = App::new(true, None);
        app.boxes.saved = Ok(BTreeMap::from([("server".to_string(), BoxTarget { host: "me@server".into(), binary: DEFAULT_BINARY.into(), ssh_config: None })]));
        let _ = app.boxes_event(Event::Address("me@other".into()));
        let _ = app.boxes_event(Event::Name("server".into()));
        let _ = app.boxes_event(Event::Add);
        assert!(matches!(&app.boxes.said, Some(Err(why)) if why.contains("already saved")));
        let _ = app.boxes_event(Event::Remove("server".into()));
        assert_eq!(app.boxes.armed_remove.as_deref(), Some("server"));
        let _ = app.boxes_event(Event::Remove("other".into()));
        assert_eq!(app.boxes.armed_remove.as_deref(), Some("other"), "a press on another box arms that one");
    }

    #[test]
    fn a_pinned_box_is_offered_by_the_menu_until_unpinned() {
        let (mut app, _) = App::new(true, None);
        let target = |host: &str| BoxTarget { host: host.into(), binary: DEFAULT_BINARY.into(), ssh_config: None };
        app.boxes.saved = Ok(BTreeMap::from([("lab".to_string(), target("me@lab")), ("server".to_string(), target("me@server"))]));
        assert!(app.menu_boxes().is_empty(), "nothing pinned, nothing offered");
        let _ = app.boxes_event(Event::Pin("server".into()));
        let _ = app.boxes_event(Event::Pin("lab".into()));
        // in pinning order; a pin whose box is no longer saved is not offered
        app.settings.pinned_boxes.push("gone".into());
        assert_eq!(app.menu_boxes(), vec![(Some("server".to_string()), "me@server".to_string()), (Some("lab".to_string()), "me@lab".to_string())]);
        let _ = app.boxes_event(Event::Pin("server".into()));
        assert_eq!(app.menu_boxes(), vec![(Some("lab".to_string()), "me@lab".to_string())]);
    }
}
