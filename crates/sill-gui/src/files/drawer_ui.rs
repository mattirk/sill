//! The file drawer: its toolbar, the tree of pinned folders and the Changes list.
use iced::widget::{
    Column, button, column, container, responsive, row, rule, scrollable, space, text, text_input,
};
use iced::{Alignment, Element, Fill, Font, Padding, Shrink};
use sill_proto::sessions::HostPath;

use sill_client::files::{Change, FilesState as State, key, name, relative};

use super::ui::{action, glyph_action, msg, short, tip};
use crate::{
    App, Message, SessionView, keys,
    style::{self, P},
    files::Action,
};

impl App {
    pub(crate) fn file_drawer<'a>(&'a self, view: &'a SessionView) -> Element<'a, Message> {
        let state = &view.files;
        let hovered = self.drawer.hovered.as_deref();
        // The view switch shows where it goes: tree or changes.
        let toolbar = row![
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
            body = diff_list(body, state, &view.workspaces);
        } else {
            let mut global_heading = false;
            for root in &state.roots {
                if root.global && !global_heading {
                    body = body.push(caption("Shortcuts"));
                    global_heading = true;
                }
                let dir = root.metadata.as_ref().is_some_and(|m| m.directory);
                body = body.push(node(
                    state, hovered, &root.path, dir, 0, !root.global,
                    view.workspaces.workspace_of(&root.path).is_some(),
                ));
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
                body = body.push(node(state, hovered, home, true, 0, state.pinned(home), false));
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
}


fn caption(value: &str) -> Element<'_, Message> {
    container(text(value).size(style::TINY).color(P.fg_dim))
        .padding([5, 10])
        .into()
}

/// One tree row: indent guides, the kind's glyph (in the
/// accent when pinned), the name coloured by what git says of it, a repository root's branch
/// (or `workspace`) at the end, and the pin button while the pointer is over the row.
fn node<'a>(
    state: &'a State,
    hovered: Option<&str>,
    path: &'a HostPath,
    directory: bool,
    depth: usize,
    pinned: bool,
    workspace: bool,
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
    ]
    .spacing(7)
    .align_y(Alignment::Center);
    let row_key = key(path);
    let show_pin = !pinned && hovered == Some(row_key);
    let mut hover = path.display.clone();
    // A workspace needs no generated ID here; ordinary branches fit the remaining width.
    if depth == 0
        && let Some(repo) = state.repo_of(path)
        && !repo.head.is_empty()
    {
        hover.push_str(&format!("\ngit {}", repo.head));
        let head = &repo.head;
        label = label.push(responsive(move |size| {
            let value = if workspace {
                "workspace".to_string()
            } else {
                format!("git {head}")
            };
            let mono = Font::with_name(crate::fonts::MONO_FAMILY);
            let mut limit = value.chars().count();
            let mut shown = value.clone();
            while limit > 0 && crate::ui::text_width_in_font(&shown, style::TINY, mono) > size.width {
                limit -= 1;
                shown = if limit == 0 {
                    String::new()
                } else {
                    short(&value, limit)
                };
            }
            container(text(shown)
                .size(style::TINY)
                .font(mono)
                .color(style::alpha(P.accent, 0.9))
                .wrapping(text::Wrapping::None))
                .align_right(Fill)
                .into()
        }).height(Shrink));
    } else {
        label = label.push(space().width(Fill));
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
    // a guide per level
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
    // Keep the path tooltip off the pin button, which has its own tooltip.
    line = line.push(tip(button, hover));
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
    line.into()
}

/// The Diff view: per pinned repository a dim header — its
/// folder and branch, and `N files · +a −d ?u` — then a row per changed file: git's letter (`N`
/// for new), the file's name (its path below the repository in the tooltip) and its lines added
/// and removed, or `—` without counts.
fn diff_list<'a>(mut body: Column<'a, Message>, state: &'a State, workspaces: &crate::workspaces::State) -> Column<'a, Message> {
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
        // a workspace starts from HEAD: what is only in the person's checkout is not listed here
        if let Some(apart) = workspaces.checkout_only(&root.path) {
            body = body.push(note(&apart, style::alpha(P.fg, 0.5)));
        }
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
        if repo.truncated {
            body = body.push(note(
                &format!("past {} files the counts are not read", repo.counts.len()),
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
                false,
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
