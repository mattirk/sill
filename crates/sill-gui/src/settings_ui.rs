//! The Settings page (reference `03`, `13`, `14`): sections, each a title over a card of rows —
//! the setting and a short dim line on the left, its control on the right, wide controls under
//! their title — held to a reading width. The daemon's settings are read and changed over SFP
//! (`_sill/settings.*`, sill-client's `settings`) and live in `sill.toml`; sill-gui's own (the
//! Shelf width, Interface scale, the shortcuts, the window's modes and sizes) are in
//! `~/.config/sill/sill-gui.toml`.
use iced::widget::{Column, button, column, container, pick_list, row, rule, scrollable, space, stack, text, text_editor, text_input, toggler};
use iced::{Alignment, Border, Color, Element, Fill, Padding, Shrink, Theme};
use sill_client::settings::{self, Snapshot};
use sill_proto::admin::SettingChange;
use sill_proto::sessions::HostPath;

use crate::style::{self, P, alpha};
use crate::{App, Message, keys};

/// The page's reading width.
const MEASURE: f32 = 680.0;
/// The last section: the running-time chart, folded until asked for.
const RUNNING_TIME: &str = "Agent running time";
const VIRTUAL_MACHINE: &str = "Virtual machine";

/// Shelf mode's width, a share of the main display (sill-gui's own setting).
const SHELF_SHARES: &[(f32, &str)] = &[(0.2, "20 %"), (0.25, "25 %"), (0.3, "30 %"), (0.35, "35 %"), (0.4, "40 %")];

/// The Settings navigation choice, kept in this window's preferences.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    #[default]
    Interface,
    Sessions,
    AgentBehaviour,
    MemoryHistory,
    System,
}

impl Category {
    const ALL: [Self; 5] = [Self::Interface, Self::Sessions, Self::AgentBehaviour, Self::MemoryHistory, Self::System];
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Interface => "Interface",
            Self::Sessions => "Sessions",
            Self::AgentBehaviour => "Agent behaviour",
            Self::MemoryHistory => "Memory & history",
            Self::System => "System",
        })
    }
}

pub struct SettingsPage {
    pub category: Category,
    pub scroll_offsets: std::collections::BTreeMap<Category, f32>,
    pub autostart: Result<bool, String>,
    pub autostart_busy: bool,
    pub token: String,
    pub load_token: String,
    pub policies: crate::policies::State,
    pub collapsed: std::collections::BTreeSet<&'static str>,
    pub shortcuts: Option<crate::shortcuts::Bindings>,
    /// What the daemon said; `None` while it is read.
    pub snapshot: Option<Result<Snapshot, String>>,
    pub instructions: text_editor::Content,
    /// The instructions were edited since they were read or saved.
    pub edited: bool,
    pub instructions_saving: Option<String>,
    pub pin_path: String,
    /// What the last action did, or why it did not.
    pub said: Option<String>,
    /// Delete all ended sessions was pressed once.
    pub armed_delete: crate::armed::Armed<()>,
    pub where_open: bool,
    /// The hotkey as typed, before it is applied.
    pub hotkey: Option<String>,
}

impl Default for SettingsPage {
    fn default() -> Self {
        Self {
            category: Category::default(),
            scroll_offsets: Default::default(),
            autostart: sill_proto::autostart_enabled().map_err(|e| e.to_string()),
            autostart_busy: false,
            token: sill_client::request_id("settings-page"),
            load_token: String::new(),
            policies: crate::policies::State::default(),
            collapsed: std::collections::BTreeSet::from(["Keyboard shortcuts", "Policies", "Daemon technical details", RUNNING_TIME]),
            shortcuts: None,
            snapshot: None,
            instructions: text_editor::Content::new(),
            edited: false,
            instructions_saving: None,
            pin_path: String::new(),
            said: None,
            armed_delete: Default::default(),
            where_open: false,
            hotkey: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Category(Category),
    Scrolled(Category, f32),
    Autostart(bool),
    AutostartDone(Result<(), String>),
    Policy(crate::policies::Event),
    ToggleSection(&'static str),
    Loaded(String, Box<Result<Snapshot, String>>),
    Change(SettingChange),
    /// A request done: what to say of it, or why it did not take effect.
    Done(Result<String, String>),
    ShelfShare(f32),
    /// Interface scale (sill-gui's own, `crate::settings::SCALES`).
    Scale(f32),
    ShortcutInput(String, String),
    ApplyShortcuts,
    ResetShortcuts,
    Instructions(text_editor::Action),
    InstructionsNewline,
    InstructionsIndent(bool),
    SaveInstructions,
    InstructionsSaved(String, sill_client::actions::Outcome),
    PinPath(String),
    AddPin,
    RemovePin(HostPath),
    RestoreNetworkPrompts,
    Compact,
    DeleteAll,
    ToggleWhere,
    AlwaysOnTop(bool),
    AlwaysApply(bool),
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
    labels.into_iter().enumerate().map(|(index, label)| Pick { label, index }).collect()
}

/// A dropdown of `labels` with `selected`, sending `on` the chosen index.
fn choice<'a>(labels: impl IntoIterator<Item = &'static str>, selected: usize, on: impl Fn(usize) -> Message + 'a) -> Element<'a, Message> {
    let options = picks(labels);
    let current = options.get(selected).copied();
    pick_list(options, current, move |pick: Pick| on(pick.index)).text_size(style::TEXT).padding([6, 12]).style(dropdown).menu_style(style::control_menu).into()
}

pub(crate) fn dropdown(_: &Theme, status: pick_list::Status) -> pick_list::Style {
    let hovered = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened { .. });
    pick_list::Style {
        text_color: P.fg,
        placeholder_color: P.fg_dim,
        handle_color: alpha(P.fg, 0.7),
        background: alpha(P.fg, if hovered { 0.12 } else { 0.07 }).into(),
        border: Border::default(),
    }
}

fn switch<'a>(on: bool, set: impl Fn(bool) -> Message + 'a) -> Element<'a, Message> {
    switch_styled(on, P.accent, 20.0, Some(set))
}

/// The Settings switch: `color` when on, dimmed when it cannot be changed (no `set`).
pub(crate) fn switch_styled<'a>(on: bool, color: Color, size: f32, set: Option<impl Fn(bool) -> Message + 'a>) -> Element<'a, Message> {
    toggler(on)
        .on_toggle_maybe(set)
        .size(size)
        .style(move |_: &Theme, status| {
            let (on, hovered, disabled) = match status {
                toggler::Status::Active { is_toggled } => (is_toggled, false, false),
                toggler::Status::Hovered { is_toggled } => (is_toggled, true, false),
                toggler::Status::Disabled { is_toggled } => (is_toggled, false, true),
            };
            let dim = if disabled { 0.45 } else { 1.0 };
            toggler::Style {
                background: if on { alpha(color, dim) } else { alpha(P.fg, if hovered { 0.2 } else { 0.14 * dim }) }.into(),
                background_border_width: 0.0,
                background_border_color: Color::TRANSPARENT,
                foreground: if on { P.bg } else { alpha(P.fg, 0.8 * dim) }.into(),
                foreground_border_width: 0.0,
                foreground_border_color: Color::TRANSPARENT,
                text_color: None,
                border_radius: Some(2.0.into()),
                padding_ratio: 0.15,
            }
        })
        .into()
}

pub(crate) fn quiet_button<'a>(label: &'a str, message: Option<Message>) -> Element<'a, Message> {
    keys::press_maybe(
        button(text(label).size(style::TEXT)).padding([6, 14]).style(|_, status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(alpha(P.fg, if hovered { 0.12 } else { 0.07 }).into()),
                text_color: if matches!(status, button::Status::Disabled) { alpha(P.fg, 0.35) } else { P.fg },
                ..button::Style::default()
            }
        }),
        message,
    )
}

/// Collapsed cards omit their controls from layout and keyboard focus traversal.
fn section<'a>(page: &SettingsPage, title: &'static str, rows: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let collapsed = page.collapsed.contains(title);
    let header = keys::press(
        button(
            row![
                text(if collapsed { "▸" } else { "▾" }).size(style::SMALL),
                crate::selection::text(title).size(style::SMALL).font(bold()).find_only().style(style::selectable(P.fg)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([10, 4])
        .style(|_, status| button::Style {
            text_color: P.fg,
            background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| alpha(P.fg, 0.07).into()),
            ..button::Style::default()
        }),
        event(Event::ToggleSection(title)),
    );
    let mut group = column![container(header).padding(Padding { top: 8.0, ..Padding::default() })];
    if !collapsed {
        let mut card = Column::new();
        for (index, row) in rows.into_iter().enumerate() {
            if index > 0 {
                card = card.push(rule::horizontal(1).style(style::rule_style(P.bg)));
            }
            card = card.push(row);
        }
        group = group.push(container(card).style(style::fill(P.surface)));
    }
    group.into()
}

fn bold() -> iced::Font {
    iced::Font { weight: iced::font::Weight::Bold, ..iced::Font::with_name(crate::fonts::SANS_FAMILY) }
}

/// The setting's title and its short dim line.
fn titles<'a>(title: &'a str, sub: Option<&'a str>) -> Element<'a, Message> {
    let mut titles = Column::new().spacing(3);
    // selectable, and so searched by Find (docs/find.md, pages)
    if !title.is_empty() {
        titles = titles.push(crate::selection::text(title).size(style::TEXT).style(style::selectable(P.fg)));
    }
    if let Some(sub) = sub {
        titles = titles.push(crate::selection::text(sub).size(style::SMALL).style(style::selectable(P.fg_dim)));
    }
    titles.width(Fill).into()
}

/// A row: titles on the left, the control on the right.
pub(crate) fn setting<'a>(title: &'a str, sub: Option<&'a str>, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(row![titles(title, sub), control.into()].spacing(16).align_y(Alignment::Center)).padding([12, 16]).into()
}

/// A wide row: titles over the control.
pub(crate) fn wide<'a>(title: &'a str, sub: Option<&'a str>, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![titles(title, sub), control.into()].spacing(10)).padding([12, 16]).into()
}

/// A path as the page shows it: the home directory as `~`.
pub(crate) fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

impl App {
    pub(crate) fn settings_view<'a>(&'a self, page: &'a SettingsPage) -> Element<'a, Message> {
        let mut tabs = row![].spacing(4);
        for category in Category::ALL {
            let selected = page.category == category;
            // as the top bar's tabs, a line under the chosen one, in a quieter accent
            // (the person, 2026-11-13)
            let tab = keys::press(
                button(text(category.to_string()).size(style::SMALL)).padding([8, 12]).style(move |_, status| button::Style {
                    text_color: if selected { P.fg } else { P.fg_dim },
                    background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| alpha(P.fg, 0.05).into()),
                    ..button::Style::default()
                }),
                event(Event::Category(category)),
            );
            tabs = tabs.push(
                column![tab, rule::horizontal(2).style(style::rule_style(if selected { alpha(P.accent, 0.45) } else { Color::TRANSPARENT })),].width(Shrink),
            );
        }
        column![
            container(
                container(tabs.wrap().vertical_spacing(4))
                    .width(Fill)
                    .max_width(MEASURE)
                    // the chosen tab's line sits on the rule under the row
                    .padding(Padding { top: 8.0, right: 16.0, bottom: 0.0, left: 16.0 })
            )
            .center_x(Fill),
            rule::horizontal(1).style(style::rule_style(P.surface)),
            self.settings_content(page),
        ]
        .height(Fill)
        .into()
    }

    fn settings_content<'a>(&'a self, page: &'a SettingsPage) -> Element<'a, Message> {
        let body: Element<'a, Message> = match &page.snapshot {
            None => container(text("Reading the settings…").size(style::TEXT).color(P.fg_dim)).padding(24).into(),
            Some(Err(error)) => container(
                column![text(error).size(style::TEXT).color(P.error), quiet_button("Try again", Some(Message::ShowPage(crate::Page::Settings))),].spacing(12),
            )
            .padding(24)
            .into(),
            Some(Ok(snapshot)) => self.settings_sections(page, snapshot),
        };
        let mut page_column = Column::new();
        if page.category == Category::System {
            page_column = page_column.push(section(page, "Daemon", self.daemon_rows(page)));
        }
        // the Mac window's own VM: apart from the daemon's settings, which come from inside it
        if page.category == Category::System && self.vm_shown() {
            page_column = page_column.push(section(page, VIRTUAL_MACHINE, self.vm_rows()));
        }
        if page.category == Category::System {
            page_column = page_column.push(section(page, crate::boxes_ui::TITLE, self.boxes_rows()));
        }
        if let Some(said) = &page.said {
            page_column = page_column.push(container(text(said).size(style::SMALL).color(P.fg_dim)).padding([10, 4]));
        }
        let page_column = page_column.push(body).max_width(MEASURE).padding(Padding { top: 4.0, bottom: 28.0, left: 16.0, right: 16.0 });
        // one selection over the whole page; its text is one row for Find, which counts what was
        // laid out (`find::Surface::Settings`)
        let finding = self.find.shown && self.find.surface == Some(crate::find::Surface::Settings);
        let mut area = crate::selection::area(
            scrollable(crate::rows::scoped(0, container(page_column).center_x(Fill)))
                .direction(scrollable::Direction::Vertical(style::bar()))
                .id("settings")
                .on_scroll(move |viewport| event(Event::Scrolled(page.category, viewport.absolute_offset().y)))
                .width(Fill)
                .height(Fill)
                .style(style::feed_scroll),
        )
        .on_copy(Message::Copy)
        .selection(&self.find.selected_text)
        .visible_leaves(&self.find.visible_leaves);
        if finding {
            area = area.find(self.find.one_row_query()).on_text_change(|| Message::Find(crate::find::Event::Refresh));
        }
        // the area stays the stack's first layer with the bar open or closed
        if finding { stack![area, self.find.bar()].into() } else { stack![area].into() }
    }

    fn settings_sections<'a>(&'a self, page: &'a SettingsPage, snapshot: &'a Snapshot) -> Element<'a, Message> {
        let s = &snapshot.settings;
        let change = |c: SettingChange| event(Event::Change(c));
        let mut page_column = Column::new();

        if page.category == Category::Interface {
            // ---- Appearance
            let theme = settings::THEMES.iter().position(|(id, _)| *id == style::theme_id()).unwrap_or(0);
            let share = self.shelf_share();
            let share_at = settings::nearest(SHELF_SHARES, (share * 100.0) as i64, |(s, _)| (s * 100.0) as i64);
            let scales = crate::settings::SCALES;
            let scale = self.settings.scale();
            let scale_at = scales.iter().position(|s| (s - scale).abs() < 0.001).unwrap_or_else(|| {
                // a hand-edited value between presets shows as the nearest one
                (0..scales.len()).min_by(|a, b| (scales[*a] - scale).abs().total_cmp(&(scales[*b] - scale).abs())).unwrap_or(2)
            });
            let scale_row = row![choice(["75 %", "90 %", "100 %", "110 %", "125 %", "150 %"], scale_at, move |i| event(Event::Scale(scales[i])),),]
                .spacing(8)
                .align_y(Alignment::Center);
            let scale_row = if scale == 1.0 { scale_row } else { scale_row.push(quiet_button("Reset", Some(event(Event::Scale(1.0))))) };
            page_column = page_column.push(section(
                page,
                "Appearance",
                vec![
                    setting("Interface scale", Some("Text, icons and controls together, on top of the desktop's scaling (sill-gui)"), scale_row),
                    setting(
                        "Theme",
                        None,
                        choice(settings::THEMES.iter().map(|(_, n)| *n), theme, |i| {
                            event(Event::Change(SettingChange::Theme { value: settings::THEMES[i].0.to_string() }))
                        }),
                    ),
                    setting(
                        "Default page",
                        Some("The page Escape opens from a conversation"),
                        choice(
                            ["Changes", "Agents", "Settings"],
                            match s.default_page.as_deref() {
                                Some("agents") => 1,
                                Some("settings") => 2,
                                _ => 0,
                            },
                            |i| event(Event::Change(SettingChange::DefaultPage { value: Some(["changes", "agents", "settings"][i].into()) })),
                        ),
                    ),
                    setting(
                        "Shelf width",
                        Some("Shelf mode's share of the main display; dragging its right edge sets it too (sill-gui)"),
                        choice(SHELF_SHARES.iter().map(|(_, l)| *l), share_at, |i| event(Event::ShelfShare(SHELF_SHARES[i].0))),
                    ),
                ],
            ));

            let bindings = page.shortcuts.as_ref().unwrap_or(&self.settings.shortcuts);
            let mut rows = Vec::new();
            for (i, (id, label, _)) in crate::shortcuts::ACTIONS.iter().enumerate() {
                rows.push(setting(
                    label,
                    None,
                    text_input("Disabled", &crate::keys::label(crate::shortcuts::value(bindings, i)))
                        .width(180)
                        .padding(7)
                        .size(style::SMALL)
                        .on_input(move |value| event(Event::ShortcutInput((*id).into(), value))),
                ));
            }
            let error = crate::shortcuts::validate(bindings).err();
            let mut controls = column![
                text(format!("Use {} or Alt with a key, optionally Shift. Blank disables a shortcut. Use Plus for +.", crate::keys::PRIMARY))
                    .size(style::SMALL)
            ];
            if let Some(why) = &error {
                controls = controls.push(text(why.clone()).size(style::SMALL).color(P.error));
            }
            controls = controls.push(
                row![
                    quiet_button("Apply", (page.shortcuts.is_some() && error.is_none()).then(|| event(Event::ApplyShortcuts))),
                    quiet_button("Reset defaults", Some(event(Event::ResetShortcuts))),
                ]
                .spacing(8),
            );
            rows.push(wide("Shortcut bindings", None, controls));
            page_column = page_column.push(section(page, "Keyboard shortcuts", rows));

            // ---- Window
            let spec = page.hotkey.clone().unwrap_or_else(|| s.hotkey.clone().unwrap_or_else(|| crate::hotkey::DEFAULT.into()));
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
                    quiet_button("Apply", (typed_ok.is_none() && spec.trim() != saved).then(|| event(Event::ApplyHotkey)),),
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
                page,
                "Window",
                vec![
                    setting(
                        "Always on top",
                        Some("The window stays above others (sill-gui)"),
                        switch(self.settings.always_on_top(), |v| event(Event::AlwaysOnTop(v))),
                    ),
                    setting(
                        "Tray icon",
                        Some("Click to show or hide; its menu quits the window (a daemon setting)"),
                        switch(s.tray.unwrap_or(true), move |v| change(SettingChange::Tray { value: Some(v) })),
                    ),
                    wide("Hotkey", Some("Shows, raises or hides the window — for example <Control>Tab, <Alt>space, <Super>grave, F12"), hotkey),
                ],
            ));
        }

        if page.category == Category::Sessions {
            // ---- Sessions
            let minutes = s.auto_revert_minutes.unwrap_or(settings::AUTO_REVERT_DEFAULT);
            let revert_at = settings::nearest(settings::AUTO_REVERT, minutes as i64, |c| c.0 as i64);
            let mut pins = Column::new().spacing(4);
            for pin in &snapshot.global_pins {
                pins = pins.push(
                    row![
                        text(home_relative(&pin.display)).size(style::TEXT).width(Fill),
                        keys::press(
                            button(crate::icons::glyph("minus", 13.0, alpha(P.fg, 0.7))).padding([6, 8]).style(style::quiet),
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
            page,
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
                setting(
                    "Always Apply",
                    Some("Choosing an agent starts a workspace session and stages edits to pinned folders for review"),
                    switch(self.settings.always_apply, |v| event(Event::AlwaysApply(v))),
                ),
                setting(
                    "Automatic workspaces",
                    Some("New sessions work in their own copy of each pinned repository; you Apply their work. The button beside the ghost in + makes a live session"),
                    switch(s.automatic_workspaces.unwrap_or(false), move |v| {
                        change(SettingChange::AutomaticWorkspaces { value: Some(v) })
                    }),
                ),
                wide(
                    "Shortcuts",
                    Some(
                        "Saved folders and files for navigation. Use the pin button to add one to a session.",
                    ),
                    pins,
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
        }

        if page.category == Category::AgentBehaviour {
            // ---- Instructions
            let editor = text_editor(&page.instructions)
                .on_action(|a| event(Event::Instructions(a)))
                .key_binding(|press| {
                    // Cmd/Ctrl+S saves the instructions, as the Save button does
                    if press.modifiers.command()
                        && matches!(press.status, text_editor::Status::Focused { .. })
                        && matches!(press.key.as_ref(), iced::keyboard::Key::Character(c) if c.eq_ignore_ascii_case("s"))
                    {
                        return Some(text_editor::Binding::Custom(event(Event::SaveInstructions)));
                    }
                    crate::ui::list_keys(&press, false)
                        .map(|key| {
                            text_editor::Binding::Custom(event(match key {
                                crate::ui::ListKey::Newline => Event::InstructionsNewline,
                                crate::ui::ListKey::Indent(outdent) => Event::InstructionsIndent(outdent),
                            }))
                        })
                        .or_else(|| text_editor::Binding::from_key_press(press))
                })
                .highlight_with::<crate::lists::Marks>(crate::lists::colours(), crate::lists::format)
                .height(Shrink)
                .min_height(120)
                .padding(10)
                .size(style::TEXT)
                .style(style::reader);
            page_column = page_column.push(section(
                page,
                "Instructions",
                vec![wide(
                    "",
                    Some("For every agent, in Markdown: sent once after it starts and again after an edit"),
                    column![
                        editor,
                        row![
                            space().width(Fill),
                            quiet_button(
                                if page.instructions_saving.is_some() {
                                    "Saving…"
                                } else if page.edited {
                                    "Save"
                                } else {
                                    "Saved"
                                },
                                (page.edited && page.instructions_saving.is_none()).then(|| event(Event::SaveInstructions)),
                            ),
                        ],
                    ]
                    .spacing(8),
                )],
            ));

            page_column = page_column.push(section(page, "Crosscheck", self.crosscheck_settings()));

            page_column = page_column.push(section(page, "Policies", vec![wide("", None, page.policies.view())]));
            if !page.policies.status.is_empty() {
                page_column = page_column.push(text(&page.policies.status).size(style::SMALL).color(P.fg_dim));
            }
            page_column = page_column.push(section(
                page,
                "Network",
                vec![setting(
                    "Dismissed network prompts",
                    Some("Restore prompts hidden with Don’t ask again across all sessions. Network access stays unchanged."),
                    quiet_button("Restore all", (!s.network_quiet_hosts.is_empty()).then(|| event(Event::RestoreNetworkPrompts))),
                )],
            ));
        }

        if page.category == Category::MemoryHistory {
            // ---- Memory
            let recall = s.memory_recall.unwrap_or(settings::RECALL_DEFAULT);
            let recall_at = settings::nearest(settings::RECALL, recall as i64, |c| c.0 as i64);
            let duration = |value: Option<&str>, default: u32| settings::nearest(settings::DURATIONS, settings::days(value, default) as i64, |d| d.2 as i64);
            let duration_choice = |at: usize, make: fn(Option<String>) -> SettingChange| {
                choice(settings::DURATIONS.iter().map(|d| d.1), at, move |i| event(Event::Change(make(Some(settings::DURATIONS[i].0.to_string())))))
            };
            let actions = row![
                quiet_button("Compact now", Some(event(Event::Compact))),
                keys::press(
                    button(
                        text(if page.armed_delete.is(&()) { "Delete every ended session?" } else { "Delete all…" })
                            .size(style::TEXT)
                            .color(if page.armed_delete.is(&()) { P.error } else { P.fg }),
                    )
                    .padding([6, 14])
                    .style(|_, status| button::Style {
                        background: Some(alpha(P.fg, if matches!(status, button::Status::Hovered) { 0.12 } else { 0.07 },).into(),),
                        ..button::Style::default()
                    }),
                    event(Event::DeleteAll),
                ),
            ]
            .spacing(8);
            page_column = page_column.push(section(
                page,
                "Memory",
                vec![
                    setting(
                        "Memory",
                        Some("A summary when a session's tab closes"),
                        switch(s.memory.unwrap_or(true), move |v| change(SettingChange::Memory { value: Some(v) })),
                    ),
                    setting(
                        "Recall",
                        Some("Earlier sessions a new agent is told about"),
                        choice(settings::RECALL.iter().map(|(_, l)| *l), recall_at, move |i| {
                            change(SettingChange::MemoryRecall { value: Some(settings::RECALL[i].0) })
                        }),
                    ),
                    setting(
                        "Ghosts see memory",
                        Some("Ghost sessions read earlier sessions (never write)"),
                        switch(s.ghost_memory.unwrap_or(false), move |v| change(SettingChange::GhostMemory { value: Some(v) })),
                    ),
                    setting(
                        "Compact after",
                        Some("After the end: thoughts, tool outputs and diffs go"),
                        duration_choice(duration(s.compact_after.as_deref(), settings::COMPACT_DEFAULT), |value| SettingChange::CompactAfter { value }),
                    ),
                    setting(
                        "Full text for",
                        Some("Then the agent's messages go too; summary, notes and prompts stay"),
                        duration_choice(duration(s.keep_full_transcripts.as_deref(), settings::FULL_TEXT_DEFAULT), |value| {
                            SettingChange::KeepFullTranscripts { value }
                        }),
                    ),
                    setting(
                        "Delete after",
                        Some("Then the session is deleted outright"),
                        duration_choice(duration(s.delete_after.as_deref(), settings::DELETE_DEFAULT), |value| SettingChange::DeleteAfter { value }),
                    ),
                    container(actions).padding([12, 16]).into(),
                ],
            ));

            page_column = page_column.push(section(page, RUNNING_TIME, vec![crate::usage::view(snapshot.active_hours.as_deref())]));
        }

        if page.category == Category::System {
            // ---- Where things are: reference, folded
            let fold = keys::press(
                button(
                    row![
                        crate::icons::glyph(if page.where_open { "chevron-down" } else { "chevron-right" }, 13.0, P.accent,),
                        text("Where things are").size(style::SMALL).color(P.accent).font(bold()),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .padding(Padding { top: 18.0, bottom: 8.0, left: 4.0, right: 8.0 })
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
                                crate::selection::text(value).size(style::SMALL).style(style::selectable(P.fg)),
                            ]
                            .spacing(12),
                        )
                        .padding([8, 16]),
                    );
                }
                page_column = page_column.push(container(card).style(style::fill(P.surface)));
            }
        }
        page_column.into()
    }
}

/// The page's reference rows: where sill keeps what, and sill-gui's keys.
fn where_things_are() -> Vec<(&'static str, String)> {
    let home = std::env::var("HOME").unwrap_or_default();
    vec![
        (
            "Config",
            format!("{home}/.config/sill/sill.toml (the daemon's settings, edited here); {home}/.config/sill/sill-gui.toml (this window's modes and sizes)"),
        ),
        ("Instructions", format!("{home}/.config/sill/instructions.md — Agent behaviour → Instructions")),
        ("Policy", "~/.config/sill/policies/, then /etc/sill/policies/, then the built-in default; Agent behaviour → Policies".into()),
        ("Sessions", format!("{home}/.local/state/sill/sill.db; jails under /var/lib/sill/<uid>/sessions/")),
        ("Daemon", "sill daemon status; logs: journalctl --user -t sill-<id>".into()),
        (
            "Keys",
            crate::keys::label(
                "Ctrl+Shift+D shelf ⇄ drawer · Ctrl+Shift+F full · Escape session ⇄ backend · Enter send · Shift+Enter newline · Ctrl+T new session · Ctrl+Shift+T shell · Ctrl+W close session · Ctrl+1–9 select tab · Ctrl+,/. previous/next · Ctrl+Q close",
            ),
        ),
    ]
}
