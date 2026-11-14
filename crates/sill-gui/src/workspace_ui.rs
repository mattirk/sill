//! The shelf's open-items row, file drawer and reader, drawn from daemon-owned paths.
use iced::widget::{
    Column, button, column, container, markdown, row, rule, scrollable, space, text, text_editor,
    text_input, tooltip,
};
use iced::{Alignment, Element, Fill, Font, Padding};
use sill_proto::sessions::HostPath;

use sill_client::files::{FilesState as State, key, name, relative};

use crate::{
    App, Message, SessionView, keys, selection,
    style::{self, P},
    viewer,
    workspace::Action,
};

fn msg(action: Action) -> Message {
    Message::Workspace(action)
}
/// A quiet text button the keyboard reaches too.
fn action<'a>(label: impl Into<String>, event: Action) -> Element<'a, Message> {
    keys::press(
        button(text(label.into()).size(style::SMALL))
            .padding([3, 7])
            .style(style::quiet),
        msg(event),
    )
}
fn short(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.into()
    } else {
        format!("{}…", value.chars().take(limit - 1).collect::<String>())
    }
}
fn tip<'a>(content: impl Into<Element<'a, Message>>, value: String) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(value).size(style::TINY))
            .padding(6)
            .style(style::fill(P.surface)),
        tooltip::Position::Bottom,
    )
    .into()
}

impl App {
    pub(crate) fn file_chips<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let state = &view.files;
        let mut chips = row![].spacing(6).align_y(Alignment::Center);
        for root in state.roots.iter().filter(|r| !r.global) {
            let directory = root.metadata.as_ref().is_some_and(|m| m.directory);
            let open = if directory {
                Action::Toggle(root.path.clone())
            } else {
                Action::Open {
                    path: root.path.clone(),
                    repo: None,
                }
            };
            let mut chip = row![action(
                format!(
                    "{} {}",
                    if directory { "▸" } else { "·" },
                    short(name(&root.path), 24)
                ),
                open
            )]
            .align_y(Alignment::Center);
            chip = chip.push(keys::press_maybe(
                button(text("×").size(style::TEXT))
                    .padding([1, 5])
                    .style(style::quiet),
                state.pin_pending.is_none().then(|| {
                    msg(Action::Pin {
                        path: root.path.clone(),
                        on: false,
                    })
                }),
            ));
            chips = chips.push(tip(
                container(chip).style(style::fill(P.surface)),
                root.path.display.clone(),
            ));
        }
        for path in state.open_files.iter().filter(|p| !state.pinned(p)) {
            let chip = row![
                action(
                    short(name(path), 24),
                    Action::Open {
                        path: path.clone(),
                        repo: None
                    }
                ),
                action("×", Action::CloseFile(key(path).into()))
            ]
            .align_y(Alignment::Center);
            chips = chips.push(tip(
                container(chip).style(style::fill(P.surface)),
                path.display.clone(),
            ));
        }
        if state.roots_pending.is_some() {
            chips = chips.push(text("Loading pins…").size(style::TINY).color(P.fg_dim));
        }
        if !state.roots.iter().any(|r| !r.global)
            && state.open_files.is_empty()
            && state.roots_pending.is_none()
        {
            chips = chips.push(action("+ Pin a folder or file", Action::ToggleDrawer));
        }
        let chips = container(
            scrollable(chips)
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::default(),
                ))
                .style(style::feed_scroll),
        )
        .padding([6, 12])
        .width(Fill);
        let mut result = column![chips];
        if let Some(error) = &state.error {
            result = result.push(
                container(
                    row![
                        text(short(error, 90)).size(style::TINY).color(P.error),
                        action("Dismiss", Action::DismissError),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([2, 12]),
            );
        }
        result.into()
    }

    pub(crate) fn file_drawer<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let state = &view.files;
        let toolbar = row![
            action("‹", Action::ToggleDrawer),
            action(
                if state.diff_mode { "Files" } else { "· Files" },
                Action::DiffMode(false)
            ),
            action(
                if state.diff_mode { "· Diff" } else { "Diff" },
                Action::DiffMode(true)
            ),
            space().width(Fill),
            action("↻", Action::Refresh),
            action("−", Action::Collapse),
        ]
        .align_y(Alignment::Center)
        .spacing(2);
        let mut body = Column::new().spacing(2).width(Fill);
        if let Some(error) = &state.error {
            body = body.push(
                column![
                    text(error).size(style::TINY).color(P.error),
                    action("Dismiss", Action::DismissError)
                ]
                .padding(8),
            );
        }
        if state.roots.is_empty() {
            body = body.push(
                container(
                    text("Nothing pinned. Browse below and use + to pin a folder or file.")
                        .size(style::SMALL)
                        .color(P.fg_dim),
                )
                .padding(10),
            );
        }
        let mut global_heading = false;
        for root in &state.roots {
            if root.global && !global_heading {
                body = body.push(caption("Global pins · use + to focus this session here"));
                global_heading = true;
            }
            let dir = root.metadata.as_ref().is_some_and(|m| m.directory);
            body = body.push(node(state, &root.path, dir, 0, !root.global));
            if let Some(error) = &root.error {
                body = body.push(text(error).size(style::TINY).color(P.error));
            }
            if state.diff_mode && dir {
                if let Some(repo) = state.repos.get(key(&root.path)) {
                    if repo.pending.is_some() {
                        body = body.push(caption("Reading repository…"));
                    }
                    if let Some(error) = &repo.error {
                        body = body.push(text(error).size(style::TINY).color(P.error));
                    }
                    if let Some(base) = &repo.root {
                        body = body.push(
                            container(
                                text(format!(
                                    "git {}\n{} files · {}",
                                    repo.head,
                                    repo.entries.len(),
                                    repo.summary
                                ))
                                .size(style::TINY)
                                .color(P.accent),
                            )
                            .padding([5, 10]),
                        );
                        if repo.entries.is_empty() && repo.pending.is_none() {
                            body = body.push(caption("Working tree is clean."));
                        }
                        for entry in &repo.entries {
                            let relative = relative(&entry.path, base);
                            let color = if entry.status.contains('D') {
                                P.error
                            } else if entry.status.contains('?') {
                                P.success
                            } else {
                                P.warning
                            };
                            let label = row![
                                text(&entry.status).size(style::TINY).color(color).width(25),
                                text(short(&relative, 28)).size(style::SMALL)
                            ]
                            .spacing(4);
                            body = body.push(tip(
                                keys::press(
                                    button(label)
                                        .padding([4, 10])
                                        .width(Fill)
                                        .style(style::row_button),
                                    msg(Action::Open {
                                        path: entry.path.clone(),
                                        repo: Some(base.clone()),
                                    }),
                                ),
                                entry.path.display.clone(),
                            ));
                        }
                    } else if repo.pending.is_none() {
                        body = body.push(caption("Not a Git repository."));
                    }
                }
            } else if dir && state.expanded.contains(key(&root.path)) {
                body = children(body, state, &root.path, 1);
            }
        }
        body = body.push(rule::horizontal(1).style(style::rule_style(P.border)));
        if let Some(home) = &state.home {
            body = body.push(caption("Browse"));
            body = body.push(node(state, home, true, 0, state.pinned(home)));
            if state.expanded.contains(key(home)) {
                body = children(body, state, home, 1);
            }
        }
        let input = text_input("Absolute path on daemon host", &state.path_input)
            .size(style::TINY)
            .padding(7)
            .on_input(|s| msg(Action::Path(s)))
            .on_submit(msg(Action::BrowsePath));
        let controls = row![
            action("Open", Action::BrowsePath),
            keys::press_maybe(
                button(
                    text(if state.pin_pending.is_some() {
                        "Pinning…"
                    } else {
                        "+ Pin"
                    })
                    .size(style::SMALL)
                )
                .padding([3, 7])
                .style(style::quiet),
                state.pin_pending.is_none().then(|| msg(Action::PinPath))
            )
        ]
        .spacing(4);
        column![
            toolbar,
            rule::horizontal(1).style(style::rule_style(P.border)),
            scrollable(body.padding([4, 0]))
                .id(format!("files-{}", view.id))
                .height(Fill)
                .style(style::feed_scroll),
            container(column![input, controls].spacing(4)).padding(8)
        ]
        .height(Fill)
        .into()
    }

    pub(crate) fn file_view<'a>(&'a self, view: &'a SessionView) -> Option<Element<'a, Message>> {
        let state = &view.files;
        let file = state.reader.as_ref()?;
        let rendering = view.reader.as_ref()?;
        let is_diff = file.repo.is_some();
        let heading = row![
            action("‹ Conversation", Action::CloseViewer),
            space().width(Fill),
            action("↻", Action::RefreshFile),
            keys::press(
                button(text("Copy").size(style::SMALL))
                    .padding([3, 7])
                    .style(style::quiet),
                Message::CopyFile
            ),
            action("×", Action::CloseViewer)
        ]
        .align_y(Alignment::Center);
        let mut tools = row![].spacing(6).align_y(Alignment::Center);
        if is_diff {
            tools = tools
                .push(text("git diff").size(style::TINY).color(P.accent))
                .push(action(
                    "Open file",
                    Action::Open {
                        path: file.path.clone(),
                        repo: None,
                    },
                ));
        } else {
            tools = tools.push(text("File").size(style::TINY).color(P.fg_dim));
            if !state.pinned(&file.path) {
                tools = tools.push(action(
                    "+ Pin",
                    Action::Pin {
                        path: file.path.clone(),
                        on: true,
                    },
                ));
            }
        }
        if file.renderable() {
            tools = tools.push(action(
                if file.source { "Rendered" } else { "Source" },
                Action::Source,
            ));
        }
        let body: Element<'_, Message> = if let Some(error) = &file.error {
            container(text(error).size(style::TEXT).color(P.error))
                .padding(18)
                .height(Fill)
                .into()
        } else if file.pending.is_some() {
            container(text("Reading…").size(style::TEXT).color(P.fg_dim))
                .padding(18)
                .height(Fill)
                .into()
        } else if file.source {
            text_editor(&rendering.content)
                .id("file-reader")
                .height(Fill)
                .padding(12)
                .size(style::MONO)
                .font(Font::with_name(crate::fonts::MONO_FAMILY))
                .style(style::reader)
                .on_action(|a| msg(Action::Text(a)))
                .key_binding(crate::ui::reading_keys)
                .into()
        } else if is_diff {
            let diff = diff_rows(&file.text);
            selection::area(
                scrollable(container(diff).padding(12))
                    .id("diff-reader")
                    .height(Fill)
                    .direction(scrollable::Direction::Both {
                        vertical: scrollable::Scrollbar::default(),
                        horizontal: scrollable::Scrollbar::default(),
                    })
                    .style(style::feed_scroll),
            )
            .on_copy(Message::Copy)
            .into()
        } else if let Some(markdown) = &rendering.markdown {
            selection::area(
                scrollable(
                    container(markdown::view_with(
                        markdown.items(),
                        crate::ui::markdown_settings(),
                        &viewer::SillViewer,
                    ))
                    .padding(16),
                )
                .id("markdown-reader")
                .height(Fill)
                .style(style::feed_scroll),
            )
            .on_copy(Message::Copy)
            .into()
        } else {
            container(text("Reading…").size(style::TEXT).color(P.fg_dim))
                .padding(18)
                .height(Fill)
                .into()
        };
        let footer = if is_diff {
            "Git’s view: working tree against HEAD."
        } else {
            "Read-only · file on the daemon host"
        };
        Some(
            column![
                heading,
                container(
                    iced_selection::text(&file.path.display)
                        .size(style::SMALL)
                        .style(style::selectable(P.fg))
                )
                .padding([4, 12]),
                container(tools).padding([4, 12]),
                rule::horizontal(1).style(style::rule_style(P.border)),
                body,
                container(text(footer).size(style::TINY).color(P.fg_dim)).padding([6, 12])
            ]
            .height(Fill)
            .into(),
        )
    }
}

fn caption(value: &str) -> Element<'_, Message> {
    container(text(value).size(style::TINY).color(P.fg_dim))
        .padding([5, 10])
        .into()
}

fn node<'a>(
    state: &'a State,
    path: &'a HostPath,
    directory: bool,
    depth: usize,
    pinned: bool,
) -> Element<'a, Message> {
    let expanded = state.expanded.contains(key(path));
    let active = state
        .reader
        .as_ref()
        .is_some_and(|f| key(&f.path) == key(path));
    let muted =
        name(path).starts_with('.') || matches!(name(path), "target" | "node_modules" | "build");
    let color = if active {
        P.accent
    } else if muted {
        P.fg_dim
    } else {
        P.fg
    };
    let glyph = if directory {
        if expanded { "▾" } else { "▸" }
    } else {
        "·"
    };
    let event = if directory {
        Action::Toggle(path.clone())
    } else {
        Action::Open {
            path: path.clone(),
            repo: None,
        }
    };
    let label = row![
        text(glyph).size(style::SMALL).color(color).width(13),
        text(short(
            name(path),
            30_usize.saturating_sub(depth * 2).max(10)
        ))
        .size(style::SMALL)
        .color(color)
    ]
    .spacing(3);
    let button = keys::press(
        button(label)
            .padding([4, 3])
            .style(style::row_button)
            .width(Fill),
        msg(event),
    );
    let mut line = row![button].align_y(Alignment::Center);
    if !pinned {
        line = line.push(action(
            "+",
            Action::Pin {
                path: path.clone(),
                on: true,
            },
        ));
    }
    tip(
        container(line).padding(Padding {
            left: 8.0 + (depth.min(12) * 12) as f32,
            ..Padding::ZERO
        }),
        path.display.clone(),
    )
}

fn children<'a>(
    mut body: Column<'a, Message>,
    state: &'a State,
    path: &'a HostPath,
    depth: usize,
) -> Column<'a, Message> {
    if depth > 24 {
        return body.push(caption("Open this folder by path to browse deeper."));
    }
    if let Some(dir) = state.directories.get(key(path)) {
        for entry in &dir.entries {
            body = body.push(node(
                state,
                &entry.path,
                entry.metadata.directory,
                depth,
                state.pinned(&entry.path),
            ));
            if entry.metadata.directory && state.expanded.contains(key(&entry.path)) {
                body = children(body, state, &entry.path, depth + 1);
            }
        }
        if dir.pending.is_some() {
            body = body.push(caption("Loading…"));
        } else if let Some(error) = &dir.error {
            body = body.push(container(text(error).size(style::TINY).color(P.error)).padding(8));
        } else if dir.next.is_some() {
            body = body.push(action("Load more…", Action::More(path.clone())));
        } else if dir.entries.is_empty() {
            body = body.push(caption("Empty folder"));
        }
    }
    body
}

/// Unified diff lines have independent old/new counters. Untracked file content is ordinary
/// text: a Markdown '-' at its start is not a deleted line.
pub(crate) fn numbered_diff(source: &str) -> Vec<(Option<usize>, Option<usize>, &str)> {
    let mut old = 0;
    let mut new = 0;
    let mut hunk = false;
    let untracked = source.starts_with("(untracked)");
    source
        .lines()
        .map(|line| {
            if !untracked && line.starts_with("@@ ") {
                let mut words = line.split_whitespace();
                words.next();
                let start = |s: &str| {
                    s.get(1..)
                        .and_then(|s| s.split(',').next())
                        .and_then(|s| s.parse().ok())
                };
                if let (Some(a), Some(b)) =
                    (words.next().and_then(start), words.next().and_then(start))
                {
                    old = a;
                    new = b;
                    hunk = true;
                }
                return (None, None, line);
            }
            if line.starts_with("diff ") {
                hunk = false;
            }
            let numbers = if hunk && line.starts_with('+') {
                let n = new;
                new += 1;
                (None, Some(n))
            } else if hunk && line.starts_with('-') {
                let n = old;
                old += 1;
                (Some(n), None)
            } else if hunk && line.starts_with(' ') {
                let a = old;
                let b = new;
                old += 1;
                new += 1;
                (Some(a), Some(b))
            } else {
                (None, None)
            };
            (numbers.0, numbers.1, line)
        })
        .collect()
}

pub(crate) fn diff_rows(source: &str) -> Column<'_, Message> {
    let mut rows = Column::new().spacing(1);
    for (old, new, line) in numbered_diff(source) {
        let color = if old.is_none() && new.is_some() {
            P.success
        } else if old.is_some() && new.is_none() {
            P.error
        } else if line.starts_with("@@ ") {
            P.accent
        } else {
            P.fg
        };
        rows = rows.push(
            row![
                text(old.map_or_else(String::new, |n| n.to_string()))
                    .width(40)
                    .size(style::TINY)
                    .color(P.fg_dim),
                text(new.map_or_else(String::new, |n| n.to_string()))
                    .width(40)
                    .size(style::TINY)
                    .color(P.fg_dim),
                selection::text(line)
                    .size(style::MONO)
                    .font(Font::with_name(crate::fonts::MONO_FAMILY))
                    .style(style::selectable(color)),
            ]
            .spacing(4),
        );
    }
    rows
}
