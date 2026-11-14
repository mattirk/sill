//! The Settings page (reference `03`, `13`, `14`; `shelf/settings.rs`): sections, each a title
//! over a card of rows — the setting and a short dim line on the left, its control on the right,
//! wide controls under their title — held to a reading width. The daemon's settings are shared
//! with the GTK shelf (`_sill/settings.*`, sill-client's `settings`); sill-gui's own (the shelf
//! width) are in `~/.config/sill/sill-gui.toml`. Rows only for what sill-gui or every session
//! uses; the GTK shelf's own window settings (text size, its width, hotkey, tray, default page,
//! host terminal) stay on its page for now.
use iced::widget::{
    Column, button, column, container, pick_list, row, rule, scrollable, space, text, text_editor,
    text_input, toggler,
};
use iced::{Alignment, Border, Color, Element, Fill, Padding, Shrink, Theme};
use sill_client::settings::{self, Snapshot};
use sill_proto::admin::SettingChange;
use sill_proto::sessions::HostPath;

use crate::style::{self, P, alpha};
use crate::{App, Message, keys};

/// The page's reading width, as the shelf's (`MEASURE` in `shelf/settings.rs`).
const MEASURE: f32 = 680.0;

/// Shelf mode's width, a share of the monitor (sill-gui's own setting).
const SHELF_SHARES: &[(f32, &str)] = &[
    (0.2, "20 %"),
    (0.25, "25 %"),
    (0.3, "30 %"),
    (0.35, "35 %"),
    (0.4, "40 %"),
];

pub struct SettingsPage {
    /// What the daemon said; `None` while it is read.
    pub snapshot: Option<Result<Snapshot, String>>,
    pub instructions: text_editor::Content,
    /// The instructions were edited since they were read or saved.
    pub edited: bool,
    pub pin_path: String,
    /// What the last action did, or why it did not.
    pub said: Option<String>,
    /// Delete all ended sessions was pressed once.
    pub armed_delete: bool,
    pub where_open: bool,
    /// The hotkey as typed, before it is applied.
    pub hotkey: Option<String>,
}

impl Default for SettingsPage {
    fn default() -> Self {
        Self {
            snapshot: None,
            instructions: text_editor::Content::new(),
            edited: false,
            pin_path: String::new(),
            said: None,
            armed_delete: false,
            where_open: false,
            hotkey: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Loaded(Box<Result<Snapshot, String>>),
    Change(SettingChange),
    /// A request done: what to say of it, or why it did not take effect.
    Done(Result<String, String>),
    ShelfShare(f32),
    Instructions(text_editor::Action),
    SaveInstructions,
    PinPath(String),
    AddPin,
    RemovePin(HostPath),
    Compact,
    DeleteAll,
    ToggleWhere,
    AlwaysOnTop(bool),
    HotkeyInput(String),
    ApplyHotkey,
}

fn event(event: Event) -> Message {
    Message::Settings(event)
}

/// A choice in a dropdown: its label, shown, and its place in the list.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pick {
    label: &'static str,
    index: usize,
}

impl std::fmt::Display for Pick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label)
    }
}

fn picks(labels: impl IntoIterator<Item = &'static str>) -> Vec<Pick> {
    labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| Pick { label, index })
        .collect()
}

/// A dropdown of `labels` with `selected`, sending `on` the chosen index.
fn choice<'a>(
    labels: impl IntoIterator<Item = &'static str>,
    selected: usize,
    on: impl Fn(usize) -> Message + 'a,
) -> Element<'a, Message> {
    let options = picks(labels);
    let current = options.get(selected).copied();
    pick_list(options, current, move |pick: Pick| on(pick.index))
        .text_size(style::TEXT)
        .padding([6, 12])
        .style(dropdown)
        .menu_style(style::control_menu)
        .into()
}

fn dropdown(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let hovered = matches!(
        status,
        pick_list::Status::Hovered | pick_list::Status::Opened { .. }
    );
    pick_list::Style {
        text_color: P.fg,
        placeholder_color: P.fg_dim,
        handle_color: alpha(P.fg, 0.7),
        background: alpha(P.fg, if hovered { 0.12 } else { 0.07 }).into(),
        border: Border::default(),
    }
}

fn switch<'a>(on: bool, set: impl Fn(bool) -> Message + 'a) -> Element<'a, Message> {
    toggler(on)
        .on_toggle(set)
        .size(20)
        .style(|_: &Theme, status| {
            let (on, hovered) = match status {
                toggler::Status::Active { is_toggled } => (is_toggled, false),
                toggler::Status::Hovered { is_toggled } => (is_toggled, true),
                toggler::Status::Disabled { is_toggled } => (is_toggled, false),
            };
            toggler::Style {
                background: if on {
                    P.accent
                } else {
                    alpha(P.fg, if hovered { 0.2 } else { 0.14 })
                }
                .into(),
                background_border_width: 0.0,
                background_border_color: Color::TRANSPARENT,
                foreground: if on { P.bg } else { alpha(P.fg, 0.8) }.into(),
                foreground_border_width: 0.0,
                foreground_border_color: Color::TRANSPARENT,
                text_color: None,
                border_radius: Some(2.0.into()),
                padding_ratio: 0.15,
            }
        })
        .into()
}

fn quiet_button<'a>(label: &'a str, message: Option<Message>) -> Element<'a, Message> {
    keys::press_maybe(
        button(text(label).size(style::TEXT))
            .padding([6, 14])
            .style(|_, status| {
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                button::Style {
                    background: Some(alpha(P.fg, if hovered { 0.12 } else { 0.07 }).into()),
                    text_color: if matches!(status, button::Status::Disabled) {
                        alpha(P.fg, 0.35)
                    } else {
                        P.fg
                    },
                    ..button::Style::default()
                }
            }),
        message,
    )
}

/// A section: its title over a card of `rows`, separated by thin lines.
fn section<'a>(title: &'a str, rows: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let mut card = Column::new();
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            card = card.push(rule::horizontal(1).style(style::rule_style(P.bg)));
        }
        card = card.push(row);
    }
    column![
        container(text(title).size(style::SMALL).color(P.accent).font(bold())).padding(Padding {
            top: 18.0,
            bottom: 8.0,
            left: 4.0,
            right: 0.0,
        }),
        container(card).style(style::fill(P.surface)),
    ]
    .into()
}

fn bold() -> iced::Font {
    iced::Font {
        weight: iced::font::Weight::Bold,
        ..iced::Font::with_name(crate::fonts::SANS_FAMILY)
    }
}

/// The setting's title and its short dim line.
fn titles<'a>(title: &'a str, sub: Option<&'a str>) -> Element<'a, Message> {
    let mut titles = Column::new().spacing(3);
    if !title.is_empty() {
        titles = titles.push(text(title).size(style::TEXT).color(P.fg));
    }
    if let Some(sub) = sub {
        titles = titles.push(text(sub).size(style::SMALL).color(P.fg_dim));
    }
    titles.width(Fill).into()
}

/// A row: titles on the left, the control on the right.
fn setting<'a>(
    title: &'a str,
    sub: Option<&'a str>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(
        row![titles(title, sub), control.into()]
            .spacing(16)
            .align_y(Alignment::Center),
    )
    .padding([12, 16])
    .into()
}

/// A wide row: titles over the control.
fn wide<'a>(
    title: &'a str,
    sub: Option<&'a str>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(column![titles(title, sub), control.into()].spacing(10))
        .padding([12, 16])
        .into()
}

/// A path as the shelf shows it: the home directory as `~`.
fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

impl App {
    pub(crate) fn settings_view<'a>(&'a self, page: &'a SettingsPage) -> Element<'a, Message> {
        let body: Element<'a, Message> = match &page.snapshot {
            None => container(
                text("Reading the settings…")
                    .size(style::TEXT)
                    .color(P.fg_dim),
            )
            .padding(24)
            .into(),
            Some(Err(error)) => container(
                column![
                    text(error).size(style::TEXT).color(P.error),
                    quiet_button("Try again", Some(Message::ShowPage(crate::Page::Settings))),
                ]
                .spacing(12),
            )
            .padding(24)
            .into(),
            Some(Ok(snapshot)) => self.settings_sections(page, snapshot),
        };
        let page_column = column![body].max_width(MEASURE).padding(Padding {
            top: 4.0,
            bottom: 28.0,
            left: 16.0,
            right: 16.0,
        });
        scrollable(container(page_column).center_x(Fill))
            .direction(scrollable::Direction::Vertical(style::bar()))
            .id("settings")
            .height(Fill)
            .style(style::feed_scroll)
            .into()
    }

    fn settings_sections<'a>(
        &'a self,
        page: &'a SettingsPage,
        snapshot: &'a Snapshot,
    ) -> Element<'a, Message> {
        let s = &snapshot.settings;
        let change = |c: SettingChange| event(Event::Change(c));
        let mut page_column = Column::new();
        if let Some(said) = &page.said {
            page_column = page_column
                .push(container(text(said).size(style::SMALL).color(P.fg_dim)).padding([10, 4]));
        }

        // ---- Appearance
        let theme = settings::THEMES
            .iter()
            .position(|(id, _)| *id == style::theme_id())
            .unwrap_or(0);
        let share = self.settings.shelf_share();
        let share_at = settings::nearest(SHELF_SHARES, (share * 100.0) as i64, |(s, _)| {
            (s * 100.0) as i64
        });
        page_column = page_column.push(section(
            "Appearance",
            vec![
                setting(
                    "Theme",
                    None,
                    choice(settings::THEMES.iter().map(|(_, n)| *n), theme, |i| {
                        event(Event::Change(SettingChange::Theme {
                            value: settings::THEMES[i].0.to_string(),
                        }))
                    }),
                ),
                setting(
                    "Shelf width",
                    Some("Shelf mode's share of the monitor (sill-gui)"),
                    choice(SHELF_SHARES.iter().map(|(_, l)| *l), share_at, |i| {
                        event(Event::ShelfShare(SHELF_SHARES[i].0))
                    }),
                ),
            ],
        ));

        // ---- Window
        let spec = page.hotkey.clone().unwrap_or_else(|| {
            s.hotkey
                .clone()
                .unwrap_or_else(|| crate::hotkey::DEFAULT.into())
        });
        let typed_ok = crate::hotkey::validate(&spec).err();
        let saved = s.hotkey.as_deref().unwrap_or(crate::hotkey::DEFAULT);
        let mut hotkey = column![
            row![
                text_input("<Control>Tab", &spec)
                    .size(style::SMALL)
                    .padding(7)
                    .width(180)
                    .on_input(|v| event(Event::HotkeyInput(v)))
                    .on_submit(event(Event::ApplyHotkey)),
                quiet_button(
                    "Apply",
                    (typed_ok.is_none() && spec.trim() != saved).then(|| event(Event::ApplyHotkey)),
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(6);
        if let Some(why) = typed_ok {
            hotkey = hotkey.push(text(format!("{why:#}")).size(style::SMALL).color(P.error));
        }
        if let Some(status) = &self.hotkey_status {
            hotkey = hotkey.push(text(status).size(style::SMALL).color(P.fg_dim));
        }
        page_column = page_column.push(section(
            "Window",
            vec![
                setting(
                    "Always on top",
                    Some("The window stays above others (sill-gui)"),
                    switch(self.settings.always_on_top(), |v| {
                        event(Event::AlwaysOnTop(v))
                    }),
                ),
                setting(
                    "Tray icon",
                    Some("Click to show or hide; its menu quits the window (shared with the shelf)"),
                    switch(s.tray.unwrap_or(true), move |v| {
                        change(SettingChange::Tray { value: Some(v) })
                    }),
                ),
                wide(
                    "Hotkey",
                    Some(
                        "Shows, raises or hides the window — GTK accelerator syntax: <Control>Tab, <Alt>space, <Super>grave, F12",
                    ),
                    hotkey,
                ),
            ],
        ));

        // ---- Sessions
        let desktop = |value: Option<bool>| match value {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let from = |i: usize| match i {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        };
        let desktop_row = row![
            text("X11").size(style::SMALL).color(P.fg_dim),
            choice(["policy", "on", "off"], desktop(s.desktop_x11), move |i| {
                change(SettingChange::DesktopX11 { value: from(i) })
            }),
            text("D-Bus").size(style::SMALL).color(P.fg_dim),
            choice(["policy", "on", "off"], desktop(s.desktop_dbus), move |i| {
                change(SettingChange::DesktopDbus { value: from(i) })
            }),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let minutes = s
            .auto_revert_minutes
            .unwrap_or(settings::AUTO_REVERT_DEFAULT);
        let revert_at = settings::nearest(settings::AUTO_REVERT, minutes as i64, |c| c.0 as i64);
        let mut pins = Column::new().spacing(4);
        for pin in &snapshot.global_pins {
            pins = pins.push(
                row![
                    text(home_relative(&pin.display))
                        .size(style::TEXT)
                        .width(Fill),
                    keys::press(
                        button(crate::icons::glyph("minus", 13.0, alpha(P.fg, 0.7)))
                            .padding([6, 8])
                            .style(style::quiet),
                        event(Event::RemovePin(pin.clone())),
                    ),
                ]
                .align_y(Alignment::Center),
            );
        }
        if snapshot.global_pins.is_empty() {
            pins = pins.push(text("None").size(style::SMALL).color(P.fg_dim));
        }
        let path = page.pin_path.trim();
        pins = pins.push(
            row![
                text_input("Absolute path of a folder or file", &page.pin_path)
                    .size(style::SMALL)
                    .padding(7)
                    .on_input(|v| event(Event::PinPath(v)))
                    .on_submit(event(Event::AddPin)),
                quiet_button("Add", path.starts_with('/').then(|| event(Event::AddPin))),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        page_column = page_column.push(section(
            "Sessions",
            vec![
                setting(
                    "Timer autoshow",
                    Some("Show the timer while the agent works; hover to show it anytime"),
                    switch(s.timer_auto.unwrap_or(false), move |v| {
                        change(SettingChange::TimerAuto { value: Some(v) })
                    }),
                ),
                setting(
                    "Drawer follows changes",
                    Some("Diff while the first pinned repository has changes; Files when clean"),
                    switch(s.drawer_follow_changes.unwrap_or(false), move |v| {
                        change(SettingChange::DrawerFollowChanges { value: Some(v) })
                    }),
                ),
                setting(
                    "Remember pins",
                    Some("A new session starts with the latest session's pins"),
                    switch(s.remember_pins.unwrap_or(false), move |v| {
                        change(SettingChange::RememberPins { value: Some(v) })
                    }),
                ),
                wide(
                    "Global pins",
                    Some(
                        "Folders and files every new session may work in; a chip and the agent's focus only when pinned in the session too",
                    ),
                    pins,
                ),
                setting(
                    "Desktop",
                    Some("For new sessions; a session's own menu can override"),
                    desktop_row,
                ),
                setting(
                    "Auto-revert",
                    Some("Unkept changes outside git and pins go back after this long"),
                    choice(settings::AUTO_REVERT.iter().map(|(_, l)| *l), revert_at, move |i| {
                        change(SettingChange::AutoRevertMinutes {
                            value: Some(settings::AUTO_REVERT[i].0),
                        })
                    }),
                ),
            ],
        ));

        // ---- Instructions
        let editor = text_editor(&page.instructions)
            .on_action(|a| event(Event::Instructions(a)))
            .height(Shrink)
            .min_height(120)
            .padding(10)
            .size(style::TEXT)
            .style(style::reader);
        page_column = page_column.push(section(
            "Instructions",
            vec![wide(
                "",
                Some("For every agent, in Markdown: sent once after it starts and again after an edit"),
                column![
                    editor,
                    row![
                        space().width(Fill),
                        quiet_button(
                            if page.edited { "Save" } else { "Saved" },
                            page.edited.then(|| event(Event::SaveInstructions)),
                        ),
                    ],
                ]
                .spacing(8),
            )],
        ));

        // ---- Memory
        let recall = s.memory_recall.unwrap_or(settings::RECALL_DEFAULT);
        let recall_at = settings::nearest(settings::RECALL, recall as i64, |c| c.0 as i64);
        let duration = |value: Option<&str>, default: u32| {
            settings::nearest(
                settings::DURATIONS,
                settings::days(value, default) as i64,
                |d| d.2 as i64,
            )
        };
        let duration_choice = |at: usize, make: fn(Option<String>) -> SettingChange| {
            choice(settings::DURATIONS.iter().map(|d| d.1), at, move |i| {
                event(Event::Change(make(Some(
                    settings::DURATIONS[i].0.to_string(),
                ))))
            })
        };
        let actions = row![
            quiet_button("Compact now", Some(event(Event::Compact))),
            keys::press(
                button(
                    text(if page.armed_delete {
                        "Delete every ended session?"
                    } else {
                        "Delete all…"
                    })
                    .size(style::TEXT)
                    .color(if page.armed_delete { P.error } else { P.fg }),
                )
                .padding([6, 14])
                .style(|_, status| button::Style {
                    background: Some(
                        alpha(
                            P.fg,
                            if matches!(status, button::Status::Hovered) {
                                0.12
                            } else {
                                0.07
                            },
                        )
                        .into(),
                    ),
                    ..button::Style::default()
                }),
                event(Event::DeleteAll),
            ),
        ]
        .spacing(8);
        page_column = page_column.push(section(
            "Memory",
            vec![
                setting(
                    "Memory",
                    Some("A summary when a session's tab closes"),
                    switch(s.memory.unwrap_or(true), move |v| {
                        change(SettingChange::Memory { value: Some(v) })
                    }),
                ),
                setting(
                    "Recall",
                    Some("Earlier sessions a new agent is told about"),
                    choice(
                        settings::RECALL.iter().map(|(_, l)| *l),
                        recall_at,
                        move |i| {
                            change(SettingChange::MemoryRecall {
                                value: Some(settings::RECALL[i].0),
                            })
                        },
                    ),
                ),
                setting(
                    "Ghosts see memory",
                    Some("Ghost sessions read earlier sessions (never write)"),
                    switch(s.ghost_memory.unwrap_or(false), move |v| {
                        change(SettingChange::GhostMemory { value: Some(v) })
                    }),
                ),
                setting(
                    "Compact after",
                    Some("After the end: thoughts, tool outputs and diffs go"),
                    duration_choice(
                        duration(s.compact_after.as_deref(), settings::COMPACT_DEFAULT),
                        |value| SettingChange::CompactAfter { value },
                    ),
                ),
                setting(
                    "Full text for",
                    Some("Then the agent's messages go too; summary, notes and prompts stay"),
                    duration_choice(
                        duration(
                            s.keep_full_transcripts.as_deref(),
                            settings::FULL_TEXT_DEFAULT,
                        ),
                        |value| SettingChange::KeepFullTranscripts { value },
                    ),
                ),
                setting(
                    "Delete after",
                    Some("Then the session is deleted outright"),
                    duration_choice(
                        duration(s.delete_after.as_deref(), settings::DELETE_DEFAULT),
                        |value| SettingChange::DeleteAfter { value },
                    ),
                ),
                container(actions).padding([12, 16]).into(),
            ],
        ));

        // ---- Where things are: reference, folded
        let fold = keys::press(
            button(
                row![
                    crate::icons::glyph(
                        if page.where_open {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        },
                        13.0,
                        P.accent,
                    ),
                    text("Where things are")
                        .size(style::SMALL)
                        .color(P.accent)
                        .font(bold()),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding(Padding {
                top: 18.0,
                bottom: 8.0,
                left: 4.0,
                right: 8.0,
            })
            .style(style::quiet),
            event(Event::ToggleWhere),
        );
        page_column = page_column.push(fold);
        if page.where_open {
            let mut card = Column::new();
            for (key, value) in where_things_are() {
                card = card.push(
                    container(
                        row![
                            text(key).size(style::SMALL).color(P.fg_dim).width(110),
                            crate::selection::text(value)
                                .size(style::SMALL)
                                .style(style::selectable(P.fg)),
                        ]
                        .spacing(12),
                    )
                    .padding([8, 16]),
                );
            }
            page_column = page_column.push(container(card).style(style::fill(P.surface)));
        }
        crate::selection::area(page_column)
            .on_copy(Message::Copy)
            .into()
    }
}

/// The page's reference rows: where sill keeps what, and sill-gui's keys.
fn where_things_are() -> Vec<(&'static str, String)> {
    let home = std::env::var("HOME").unwrap_or_default();
    vec![
        (
            "Config",
            format!(
                "{home}/.config/sill/sill.toml (shared with the shelf); {home}/.config/sill/sill-gui.toml (this window's modes and sizes)"
            ),
        ),
        (
            "Instructions",
            format!("{home}/.config/sill/instructions.md — the box above"),
        ),
        (
            "Policy",
            "~/.config/sill/policies/, then /etc/sill/policies/, then the built-in default; edited on the shelf's Settings page for now".into(),
        ),
        (
            "Sessions",
            format!("{home}/.local/state/sill/sill.db; jails under /var/lib/sill/<uid>/sessions/"),
        ),
        (
            "Daemon",
            "sill daemon status; logs: journalctl --user -t sill-<id>".into(),
        ),
        (
            "Keys",
            "Ctrl+Shift+D shelf ⇄ drawer · Ctrl+Shift+F full · Escape session ⇄ backend · Enter send · Shift+Enter newline · Ctrl+T shell · Ctrl+Shift+Left/Right tabs · Ctrl+Q close".into(),
        ),
    ]
}
