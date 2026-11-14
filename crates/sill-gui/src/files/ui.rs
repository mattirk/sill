//! The open-items row (chips) and the buttons the drawer and the reader share.
use iced::widget::{button, column, container, row, scrollable, text, tooltip};
use iced::{Alignment, Element, Fill};

use sill_client::files::name;

use crate::{
    App, Message, SessionView, keys,
    style::{self, P},
    files::Action,
};

pub(super) fn msg(action: Action) -> Message {
    Message::Files(action)
}
/// A quiet text button the keyboard reaches too.
pub(super) fn action<'a>(label: impl Into<String>, event: Action) -> Element<'a, Message> {
    sized_action(label, style::SMALL, event)
}
/// A text button at `size`: the drawer's Files / Diff switch is body text.
fn sized_action<'a>(label: impl Into<String>, size: f32, event: Action) -> Element<'a, Message> {
    keys::press(
        button(text(label.into()).size(size))
            .padding([3, 7])
            .style(style::quiet),
        msg(event),
    )
}
/// A quiet button that is a line glyph, with what it does as its tooltip.
pub(super) fn glyph_action<'a>(glyph: &str, what: &str, event: Action) -> Element<'a, Message> {
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
pub(super) fn short(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.into()
    } else {
        format!("{}…", value.chars().take(limit - 1).collect::<String>())
    }
}
pub(super) fn tip<'a>(content: impl Into<Element<'a, Message>>, value: String) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(value).size(style::TINY))
            .padding(6)
            .style(style::fill(P.surface)),
        tooltip::Position::Bottom,
    )
    // after the pointer rests, not while it passes over the tree
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
            // a workspace or a repository kept live has its own mark (docs/parallel-coding.md §12)
            let (mark, said) = crate::workspaces_ui::chip_mark(view, &root.path, glyph);
            let mut label = row![
                mark,
                text(short(name(&root.path), 24)).size(style::TEXT),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            // the person's uncommitted work is not in the workspace: a quiet dot, said in the tip
            let apart = view.workspaces.checkout_only(&root.path);
            if apart.is_some() {
                label = label.push(text("•").size(style::SMALL).color(style::alpha(P.warning, 0.7)));
            }
            let said = match (said, apart) {
                (Some(said), Some(apart)) => Some(format!("{said}\n{apart}.")),
                (said, apart) => said.or(apart),
            };
            // Tracked line totals against HEAD, plus untracked files separately.
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
                container(chip).style(style::card(style::alpha(P.fg, 0.07))),
                match said {
                    Some(said) => format!("{}\n{said}", root.path.display),
                    None => root.path.display.clone(),
                },
            ));
        }
        // the first load only: the 15 s refresh keeps the chips as they are until it answers
        let loading = state.roots_pending.is_some() && !state.initialized;
        if loading {
            chips = chips.push(text("Loading pins…").size(style::TINY).color(P.fg_dim));
        }
        if !state.roots.iter().any(|r| !r.global) && !loading {
            chips = chips.push(tip(
                keys::press_maybe(
                    button(crate::icons::glyph("folder-plus", 16.0, P.fg_dim))
                        .padding([3, 8])
                        .style(style::quiet),
                    (self.drawer.folder_picker.is_none() && state.pin_pending.is_none())
                        .then(|| msg(Action::PickFolder)),
                ),
                "Pin a folder".into(),
            ));
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
}
