//! The shelf's open-items row, file drawer and reader, drawn from daemon-owned paths.
use iced::widget::{
    Column, button, column, container, markdown, responsive, row, rule, scrollable, space, text,
    text_editor, text_input, tooltip,
};
use iced::{Alignment, Element, Fill, Font, Padding, Shrink};
use sill_proto::sessions::HostPath;

use sill_client::files::{COUNT_CAP, Change, FilesState as State, key, name, relative};

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
    sized_action(label, style::SMALL, event)
}
/// A text button at `size`: the drawer's Files / Diff switch is body text, as in the shelf.
fn sized_action<'a>(label: impl Into<String>, size: f32, event: Action) -> Element<'a, Message> {
    keys::press(
        button(text(label.into()).size(size))
            .padding([3, 7])
            .style(style::quiet),
        msg(event),
    )
}
/// A quiet button that is a line glyph, with what it does as its tooltip.
fn glyph_action<'a>(glyph: &str, what: &str, event: Action) -> Element<'a, Message> {
    tip(
        keys::press(
            button(crate::icons::glyph(glyph, 13.0, style::alpha(P.fg, 0.7)))
                .padding([5, 7])
                .style(style::quiet),
            msg(event),
        ),
        what.to_string(),
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
    // as the shelf's tooltips: after the pointer rests, not while it passes over the tree
    .delay(std::time::Duration::from_millis(350))
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
            let glyph = crate::icons::for_path(name(&root.path), directory, false);
            let mut label = row![
                crate::icons::glyph(glyph, 14.0, style::alpha(P.fg, 0.6)),
                text(short(name(&root.path), 24)).size(style::TEXT),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            // Match GTK: tracked line totals against HEAD, plus untracked files separately.
            if let Some(repo) = state.repo_of(&root.path) {
                if let Some((added, removed)) = &repo.lines {
                    for (value, sign, color) in [(added, "+", P.success), (removed, "−", P.error)]
                    {
                        if value != "0" {
                            label = label.push(
                                text(format!("{sign}{value}"))
                                    .size(style::SMALL)
                                    .color(color),
                            );
                        }
                    }
                }
                if let Some((_, untracked)) = repo.summary.rsplit_once('?') {
                    let untracked = untracked.trim();
                    if !matches!(untracked, "" | "0") {
                        label = label.push(
                            text(format!("?{untracked}"))
                                .size(style::SMALL)
                                .color(P.fg_dim),
                        );
                    }
                }
            }
            let mut chip = row![keys::press(
                button(label).padding([3, 8]).style(style::quiet),
                msg(open),
            )]
            .align_y(Alignment::Center);
            chip = chip.push(keys::press_maybe(
                button(crate::icons::glyph("x", 13.0, style::alpha(P.fg, 0.6)))
                    .padding([4, 7])
                    .style(style::quiet),
                state.pin_pending.is_none().then(|| {
                    msg(Action::Pin {
                        path: root.path.clone(),
                        on: false,
                    })
                }),
            ));
            chips = chips.push(tip(
                container(chip).style(style::fill(style::alpha(P.fg, 0.07))),
                root.path.display.clone(),
            ));
        }
        // the first load only: the 15 s refresh keeps the chips as they are until it answers
        let loading = state.roots_pending.is_some() && !state.initialized;
        if loading {
            chips = chips.push(text("Loading pins…").size(style::TINY).color(P.fg_dim));
        }
        if !state.roots.iter().any(|r| !r.global) && !loading {
            chips = chips.push(action("+ Pin a folder or file", Action::ToggleDrawer));
        }
        let chips = container(
            scrollable(chips)
                .direction(scrollable::Direction::Horizontal(style::bar()))
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
        let hovered = self.workspace.hovered.as_deref();
        // as the shelf's bar: the drawer's close at the left, its one view switch at the right,
        // showing where it goes — the changes from the tree, the tree from the changes
        let toolbar = row![
            glyph_action(
                "chevron-left",
                "Close the drawer and switch to Shelf",
                Action::CloseDrawerToShelf
            ),
            space().width(Fill),
            if state.diff_mode {
                glyph_action("list-tree", "The file tree", Action::DiffMode(false))
            } else {
                glyph_action(
                    "git-compare",
                    "What changed: the repositories' changed files",
                    Action::DiffMode(true),
                )
            },
        ]
        .align_y(Alignment::Center)
        .padding([2, 4]);
        // no spacing: rows carry their own, so the indent guides run on unbroken
        let mut body = Column::new().width(Fill);
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
        if state.diff_mode {
            body = diff_list(body, state);
        } else {
            let mut global_heading = false;
            for root in &state.roots {
                if root.global && !global_heading {
                    body = body.push(caption("Global pins · use + to focus this session here"));
                    global_heading = true;
                }
                let dir = root.metadata.as_ref().is_some_and(|m| m.directory);
                body = body.push(node(state, hovered, &root.path, dir, 0, !root.global));
                if let Some(error) = &root.error {
                    body = body.push(text(error).size(style::TINY).color(P.error));
                }
                if dir && state.expanded.contains(key(&root.path)) {
                    body = children(body, state, hovered, &root.path, 1);
                }
            }
            body = body.push(rule::horizontal(1).style(style::rule_style(P.border)));
            if let Some(home) = &state.home {
                body = body.push(caption("Browse"));
                body = body.push(node(state, hovered, home, true, 0, state.pinned(home)));
                if state.expanded.contains(key(home)) {
                    body = children(body, state, hovered, home, 1);
                }
            }
        }
        let input = text_input("Absolute path", &state.path_input)
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
        .spacing(4)
        .height(26)
        .align_y(Alignment::Center);
        column![
            toolbar,
            rule::horizontal(1).style(style::rule_style(P.border)),
            scrollable(body.padding([4, 0]))
                .direction(scrollable::Direction::Vertical(style::bar()))
                .id(format!("files-{}", view.id))
                .height(Fill)
                .style(style::feed_scroll),
            if state.diff_mode {
                Element::from(space().height(0))
            } else {
                container(column![input, controls].spacing(4))
                    .padding(Padding {
                        bottom: 4.0,
                        ..Padding::new(8.0)
                    })
                    .into()
            }
        ]
        .height(Fill)
        .into()
    }

    pub(crate) fn file_view<'a>(&'a self, view: &'a SessionView) -> Option<Element<'a, Message>> {
        let state = &view.files;
        let file = state.reader.as_ref()?;
        let rendering = view.reader.as_ref()?;
        let is_diff = file.repo.is_some();
        // one bar, as the shelf's viewer (reference 10, 11): the path — `git diff: <path>` for a
        // diff — dimmed, shortened with ~ and cut at its start; then what can be done; close
        let shown = home_relative(&file.path.display);
        let title = if is_diff {
            format!("git diff: {shown}")
        } else {
            shown
        };
        let mut heading = row![
            container(
                iced_selection::text(title)
                    .size(style::TEXT)
                    .wrapping(text::Wrapping::None)
                    .style(style::selectable(P.fg_dim))
            )
            .width(Fill)
            .clip(true),
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        if is_diff {
            heading = heading.push(action(
                "Open file",
                Action::Open {
                    path: file.path.clone(),
                    repo: None,
                },
            ));
        } else if !state.pinned(&file.path) {
            heading = heading.push(action(
                "+ Pin",
                Action::Pin {
                    path: file.path.clone(),
                    on: true,
                },
            ));
        }
        if file.renderable() {
            heading = heading.push(action(
                if file.source { "Preview" } else { "Source" },
                Action::Source,
            ));
        }
        if rendering.dirty {
            heading = heading.push(text("Unsaved").size(style::SMALL).color(P.warning));
        }
        if !is_diff && (file.source || rendering.dirty) {
            heading = heading.push(keys::press_maybe(
                button(
                    text(if rendering.saving.is_some() {
                        "Saving…"
                    } else {
                        "Save"
                    })
                    .size(style::SMALL),
                )
                .padding([3, 7])
                .style(style::quiet),
                (rendering.dirty && rendering.saving.is_none()).then(|| msg(Action::SaveFile)),
            ));
        }
        heading = heading
            .push(glyph_action(
                "refresh",
                "Read the file again",
                Action::RefreshFile,
            ))
            .push(tip(
                keys::press(
                    button(crate::icons::glyph("copy", 13.0, style::alpha(P.fg, 0.7)))
                        .padding([5, 7])
                        .style(style::quiet),
                    Message::CopyFile,
                ),
                "Copy the text".into(),
            ))
            .push(container(glyph_action("x", "Close", Action::CloseViewer)).id("close-file"));
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
            let digits = rendering.content.line_count().max(1).ilog10() + 1;
            let width =
                (rendering.source_columns as f32 + digits.max(2) as f32 + 4.0) * style::MONO * 0.6
                    + 24.0;
            responsive(move |size| {
                let editor = text_editor(&rendering.content)
                    .id("file-reader")
                    .height(Fill)
                    .width(width.max(size.width))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .padding(12)
                    .size(style::MONO)
                    .font(Font::with_name(crate::fonts::MONO_FAMILY))
                    .style(|theme, status| {
                        let mut style = style::reader(theme, status);
                        style.border = iced::Border::default();
                        style
                    })
                    .line_numbers(|editor| {
                        let mut previous = None;
                        editor
                            .buffer()
                            .layout_runs()
                            .filter_map(|run| {
                                let first = previous != Some(run.line_i);
                                previous = Some(run.line_i);
                                (first && run.glyphs.first().is_none_or(|glyph| glyph.start == 0))
                                    .then_some((run.line_i + 1, run.line_top))
                            })
                            .collect()
                    })
                    .on_action(|a| msg(Action::Text(a)))
                    .key_binding(crate::ui::reading_keys)
                    .highlight(
                        if is_diff {
                            "diff"
                        } else {
                            crate::syntax::token(&file.path.display)
                        },
                        crate::syntax::theme(),
                    );
                scrollable(editor)
                    .id(format!("source-horizontal-{}", rendering.key))
                    .direction(scrollable::Direction::Horizontal(style::bar()))
                    .width(Fill)
                    .height(Fill)
                    .style(style::feed_scroll)
                    .into()
            })
            .into()
        } else if is_diff {
            let diff = diff_rows(&file.text, &file.path.display);
            selection::area(
                scrollable(container(diff).padding(12))
                    .id("diff-reader")
                    .height(Fill)
                    .direction(scrollable::Direction::Both {
                        vertical: style::bar(),
                        horizontal: style::bar(),
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
                        &viewer::SillViewer { media: None },
                    ))
                    .padding(16),
                )
                .direction(scrollable::Direction::Vertical(style::bar()))
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
        let mut page = column![
            container(heading).padding(Padding {
                top: 4.0,
                bottom: 4.0,
                left: 12.0,
                right: 6.0,
            }),
            rule::horizontal(1).style(style::rule_style(P.border)),
        ];
        if let Some(notice) = &rendering.notice {
            let mut notice_row = row![text(notice).size(style::SMALL).width(Fill)]
                .spacing(8)
                .align_y(Alignment::Center);
            if self
                .workspace
                .pending_discard
                .as_ref()
                .is_some_and(|(id, _)| id == &view.id)
            {
                notice_row = notice_row
                    .push(action("Discard", Action::DiscardEdits))
                    .push(action("Cancel", Action::CancelDiscard));
            }
            page = page.push(
                container(notice_row)
                    .padding([8, 12])
                    .style(style::fill(style::alpha(P.warning, 0.10))),
            );
        }
        page = page.push(body);
        if is_diff {
            page = page.push(
                container(
                    text("git's view: the working tree against HEAD (open the file from the tree to edit it)")
                        .size(style::TINY)
                        .color(P.fg_dim),
                )
                .padding([6, 12]),
            );
        }
        Some(page.height(Fill).into())
    }
}

/// A path as the shelf titles it: the home directory as `~`.
fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

fn caption(value: &str) -> Element<'_, Message> {
    container(text(value).size(style::TINY).color(P.fg_dim))
        .padding([5, 10])
        .into()
}

/// One tree row as the shelf draws it (`shelf/tree.rs`): indent guides, the kind's glyph (in the
/// accent when pinned), the name coloured by what git says of it, a repository root's `git
/// <branch>` at the end, and the pin button while the pointer is over the row.
fn node<'a>(
    state: &'a State,
    hovered: Option<&str>,
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
    let color = match state.change(path) {
        Some(Change::Changed | Change::Below) => P.warning,
        Some(Change::New) => P.success,
        Some(Change::Deleted) => P.error,
        None if muted => style::alpha(P.fg, 0.45),
        None => P.fg,
    };
    let glyph = crate::icons::for_path(name(path), directory, expanded);
    let glyph_color = if pinned {
        P.accent
    } else {
        style::alpha(P.fg, if muted { 0.35 } else { 0.7 })
    };
    let event = if directory {
        Action::Toggle(path.clone())
    } else {
        Action::Open {
            path: path.clone(),
            repo: None,
        }
    };
    let mut label = row![
        crate::icons::glyph(glyph, 14.0, glyph_color),
        text(short(
            name(path),
            30_usize.saturating_sub(depth * 2).max(10)
        ))
        .size(style::TEXT)
        .color(color)
        .wrapping(text::Wrapping::None),
        space().width(Fill),
    ]
    .spacing(7)
    .align_y(Alignment::Center);
    let row_key = key(path);
    let show_pin = !pinned && hovered == Some(row_key);
    // a repository root: `git <branch>` ends the row
    if depth == 0
        && let Some(repo) = state.repo_of(path)
        && !repo.head.is_empty()
    {
        label = label.push(
            text(format!("git {}", repo.head))
                .size(style::TINY)
                .font(iced::Font::with_name(crate::fonts::MONO_FAMILY))
                .color(style::alpha(P.accent, 0.9))
                .wrapping(text::Wrapping::None),
        );
    }
    let button = keys::press(
        button(label)
            .padding(Padding {
                top: 3.0,
                bottom: 3.0,
                left: 4.0,
                right: 6.0,
            })
            .style(if active {
                style::row_selected
            } else {
                style::row_button
            })
            .width(Fill),
        msg(event),
    );
    let mut line = row![].height(Shrink).align_y(Alignment::Center);
    // a guide per level, as the shelf's indents draw them
    for _ in 0..depth.min(12) {
        line = line.push(
            container(rule::vertical(1).style(style::rule_style(style::alpha(P.fg, 0.14))))
                .padding(Padding {
                    left: 8.0,
                    ..Padding::ZERO
                })
                .width(15)
                // the row's whole height, so one row's guide meets the next one's
                .height(Fill),
        );
    }
    line = line.push(button);
    // the pin keeps its place while hidden, so the names do not move when it shows
    line = line.push(if show_pin {
        glyph_action(
            "plus",
            "Pin: mount it in the jail",
            Action::Pin {
                path: path.clone(),
                on: true,
            },
        )
    } else {
        Element::from(space().width(27))
    });
    let line = iced::widget::mouse_area(container(line).padding(Padding {
        left: 4.0,
        ..Padding::ZERO
    }))
    .on_enter(msg(Action::Hover(row_key.to_string())))
    .on_exit(msg(Action::Unhover(row_key.to_string())));
    tip(line, path.display.clone())
}

/// The Diff view as the shelf's (`shelf/diffstat.rs`): per pinned repository a dim header — its
/// folder and branch, and `N files · +a −d ?u` — then a row per changed file: git's letter (`N`
/// for new), the file's name (its path below the repository in the tooltip) and its lines added
/// and removed, or `—` without counts.
fn diff_list<'a>(mut body: Column<'a, Message>, state: &'a State) -> Column<'a, Message> {
    let mut any = false;
    for root in state.roots.iter().filter(|r| !r.global) {
        if !root.metadata.as_ref().is_some_and(|m| m.directory) {
            continue;
        }
        let Some(repo) = state.repos.get(key(&root.path)) else {
            continue;
        };
        if let Some(error) = &repo.error {
            body = body.push(note(error, P.error));
        }
        let Some(base) = &repo.root else {
            if repo.pending.is_some() && !repo.read {
                body = body.push(note("Reading the repository…", P.fg_dim));
            }
            continue;
        };
        any = true;
        let head = if repo.head.is_empty() {
            String::new()
        } else {
            format!("  git {}", repo.head)
        };
        let section = |value: String| text(value).size(style::TINY).color(style::alpha(P.fg, 0.5));
        body = body.push(tip(
            container(
                row![
                    container(
                        section(format!("{}{head}", name(base))).wrapping(text::Wrapping::None)
                    )
                    .width(Fill)
                    .clip(true),
                    section(summary_label(repo)).wrapping(text::Wrapping::None),
                ]
                .spacing(6),
            )
            .padding(Padding {
                top: 8.0,
                bottom: 2.0,
                left: 12.0,
                right: 10.0,
            }),
            "Same totals as the pin chip: +tracked insertions −tracked deletions against HEAD, \
             ?untracked files"
                .into(),
        ));
        if repo.entries.is_empty() {
            body = body.push(note("Nothing differs from HEAD.", P.fg_dim));
        }
        let open = state.reader.as_ref().filter(|r| r.repo.is_some());
        for entry in &repo.entries {
            body = body.push(diff_row(
                state,
                entry,
                base,
                open.is_some_and(|r| key(&r.path) == key(&entry.path)),
            ));
        }
        if repo.entries.len() > COUNT_CAP {
            body = body.push(note(
                &format!("past {COUNT_CAP} files the counts are not read"),
                P.fg_dim,
            ));
        }
    }
    if !any && !state.repos.values().any(|r| r.pending.is_some() && !r.read) {
        body = body.push(note(
            "No pinned git repository here: pin a repository's folder (+ on its row) to see its diff.",
            P.fg_dim,
        ));
    }
    body
}

fn diff_row<'a>(
    state: &'a State,
    entry: &'a sill_proto::discovery::StatusEntry,
    base: &'a HostPath,
    selected: bool,
) -> Element<'a, Message> {
    let mono = Font::with_name(crate::fonts::MONO_FAMILY);
    let (letter, color, what) = match entry.status.as_str() {
        "?" => ("N", P.success, "new: untracked in git"),
        "A" => ("A", P.success, "added to the index"),
        "D" => ("D", P.error, "deleted"),
        "R" => ("R", P.warning, "renamed"),
        "M" => ("M", P.warning, "modified"),
        other => (other, P.warning, "changed"),
    };
    let mut line = row![
        tip(
            text(letter)
                .size(style::TINY)
                .font(mono)
                .color(color)
                .width(12),
            what.into()
        ),
        container(
            text(name(&entry.path))
                .size(style::TEXT)
                .wrapping(text::Wrapping::None)
        )
        .width(Fill)
        .clip(true),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let counts = state
        .repos
        .values()
        .find_map(|r| r.counts.get(key(&entry.path)));
    match counts {
        Some(Some((added, removed))) => {
            for (n, sign, color) in [(added, "+", P.success), (removed, "−", P.error)] {
                if *n > 0 {
                    line = line.push(
                        text(format!("{sign}{n}"))
                            .size(style::SMALL)
                            .font(mono)
                            .color(color),
                    );
                }
            }
        }
        Some(None) => {
            line = line.push(tip(
                text("—").size(style::SMALL).color(style::alpha(P.fg, 0.5)),
                "no line counts: binary, too large, or past the first files".into(),
            ));
        }
        None => {}
    }
    tip(
        keys::press(
            button(line)
                .padding(Padding {
                    top: 3.0,
                    bottom: 3.0,
                    left: 12.0,
                    right: 10.0,
                })
                .width(Fill)
                .style(if selected {
                    style::row_selected
                } else {
                    style::row_button
                }),
            msg(Action::Open {
                path: entry.path.clone(),
                repo: Some(base.clone()),
            }),
        ),
        relative(&entry.path, base),
    )
}

/// `9 files · +12 −3 ?9`, or `clean`.
fn summary_label(repo: &sill_client::files::Repo) -> String {
    let files = repo.entries.len();
    let untracked = repo
        .summary
        .rsplit_once('?')
        .map(|(_, n)| n.trim().to_string())
        .unwrap_or_default();
    let Some((added, removed)) = &repo.lines else {
        return format!("{files} files · counts unavailable");
    };
    if files == 0 && added == "0" && removed == "0" && matches!(untracked.as_str(), "" | "0") {
        return "clean".into();
    }
    let mut label = format!(
        "{files} file{} · +{added} −{removed}",
        if files == 1 { "" } else { "s" }
    );
    if !matches!(untracked.as_str(), "" | "0") {
        label.push_str(&format!(" ?{untracked}"));
    }
    label
}

fn note<'a>(value: &str, color: iced::Color) -> Element<'a, Message> {
    container(text(value.to_string()).size(style::TINY).color(color))
        .padding([5, 12])
        .into()
}

fn children<'a>(
    mut body: Column<'a, Message>,
    state: &'a State,
    hovered: Option<&'a str>,
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
                hovered,
                &entry.path,
                entry.metadata.directory,
                depth,
                state.pinned(&entry.path),
            ));
            if entry.metadata.directory && state.expanded.contains(key(&entry.path)) {
                body = children(body, state, hovered, &entry.path, depth + 1);
            }
        }
        // a reload (the 15 s refresh) shows the listing as it was until the new one comes
        if dir.pending.is_some() && !dir.reloading {
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

/// A diff as the shelf shows it: one column of line numbers down the diff's own text, in the
/// code's face and size so each number sits on its line; additions and deletions in git's
/// colours, hunk headers in the accent, the file headers bold.
pub(crate) fn diff_rows<'a>(source: &'a str, path: &str) -> Column<'a, Message> {
    let mono = Font::with_name(crate::fonts::MONO_FAMILY);
    let bold = Font {
        weight: iced::font::Weight::Bold,
        ..mono
    };
    let highlighted = crate::syntax::diff(source, path);
    let lines = numbered_diff(source);
    let digits = lines.len().max(1).to_string().len();
    let gutter = style::MONO * 0.62 * digits as f32 + 6.0;
    let mut rows = Column::new();
    for (index, (old, new, line)) in lines.into_iter().enumerate() {
        let file_header = ["diff ", "--- ", "+++ "]
            .iter()
            .any(|p| line.starts_with(p))
            && old.is_none()
            && new.is_none();
        let (color, font) = if old.is_none() && new.is_some() {
            (P.success, mono)
        } else if old.is_some() && new.is_none() {
            (P.error, mono)
        } else if line.starts_with("@@ ") {
            (P.accent, mono)
        } else if file_header {
            (P.fg, bold)
        } else if line.starts_with("index ") && old.is_none() && new.is_none() {
            (P.fg_dim, mono)
        } else {
            (P.fg, mono)
        };
        let mut spans: Vec<iced::widget::text::Span<'_, (), Font>> = Vec::new();
        let mut end = 0;
        if let Some(tokens) = highlighted.get(index) {
            for (range, token_color) in tokens {
                if range.start > end {
                    spans.push(iced::widget::text::Span::new(&line[end..range.start]).color(color));
                }
                spans.push(iced::widget::text::Span::new(&line[range.clone()]).color(*token_color));
                end = range.end;
            }
        }
        if end < line.len() || spans.is_empty() {
            spans.push(iced::widget::text::Span::new(&line[end..]).color(color));
        }
        let content = selection::rich_text(spans)
            .size(style::MONO)
            .font(font)
            .style(style::selectable(color));
        let background = if old.is_none() && new.is_some() {
            style::alpha(P.success, 0.08)
        } else if old.is_some() && new.is_none() {
            style::alpha(P.error, 0.08)
        } else {
            iced::Color::TRANSPARENT
        };
        rows = rows.push(
            row![
                text((index + 1).to_string())
                    .size(style::MONO)
                    .font(mono)
                    .color(P.fg_dim)
                    .width(gutter)
                    .align_x(iced::alignment::Horizontal::Right),
                container(content).style(style::fill(background)),
            ]
            .spacing(12),
        );
    }
    rows
}
