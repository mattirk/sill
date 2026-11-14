//! The Changes page (reference `07`, `12`), opened with the herring: what
//! sessions wrote outside their pins, held in their overlays until kept or reverted; the pinned
//! repositories' own changes, live on the host; and recent keeps, which can be undone. A row opens
//! its diff. Reverting asks for a second press; a keep is refused for a file the host changed
//! meanwhile or one sill does not keep. History lists what was kept, reverted and undone.
use std::collections::BTreeMap;

use iced::widget::{
    Column, button, column, container, row, rule, scrollable, space, text, tooltip,
};
use iced::{Alignment, Element, Fill, Padding};
use sill_client::changes::{self, Entry, Group, Kind};
use sill_client::files::{name, relative};
use sill_proto::changes::HistoryEvent;

use crate::style::{self, P, alpha};
use crate::{App, Message, keys};

#[derive(Default)]
pub struct ChangesPage {
    /// Pending entries; `None` before the first answer.
    pub entries: Option<Result<Vec<Entry>, String>>,
    pub loading: bool,
    /// Settings → auto-revert, minutes (0 is off).
    pub auto_revert: u64,
    pub history: bool,
    pub events: Vec<HistoryEvent>,
    pub next: Option<String>,
    pub history_error: Option<String>,
    pub history_loading: bool,
    /// Open rows' diffs by entry key; `None` while reading.
    pub open: BTreeMap<String, Option<Result<String, String>>>,
    /// A revert pressed once: the entry's key, or `all ` and its group's first key.
    pub armed: Option<String>,
    /// What the last action did.
    pub said: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    Loaded(Result<(Vec<Entry>, u64), String>),
    Refresh,
    History(bool),
    HistoryLoaded(Result<sill_proto::changes::HistoryPage, String>, bool),
    MoreHistory,
    Diff(String),
    DiffLoaded(String, Result<String, String>),
    Keep(String),
    KeepAll(Vec<String>),
    Revert(String),
    RevertAll(Vec<String>),
    Undo(String),
}

fn event(event: Event) -> Message {
    Message::Changes(event)
}

impl ChangesPage {
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries
            .as_ref()?
            .as_ref()
            .ok()?
            .iter()
            .find(|e| changes::entry_key(e) == key)
    }
}

fn small<'a>(label: &'a str, message: Option<Message>) -> Element<'a, Message> {
    keys::press_maybe(
        button(text(label).size(style::SMALL))
            .padding([2, 8])
            .style(style::quiet),
        message,
    )
}

fn badge<'a>(label: impl Into<String>, color: iced::Color) -> Element<'a, Message> {
    text(label.into())
        .size(style::SMALL)
        .font(iced::Font::with_name(crate::fonts::MONO_FAMILY))
        .color(color)
        .into()
}

/// `~/…` for a path under the daemon's home.
fn short(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

impl App {
    pub(crate) fn changes_view<'a>(&'a self, page: &'a ChangesPage) -> Element<'a, Message> {
        let count = match &page.entries {
            Some(Ok(entries)) => entries
                .iter()
                .filter(|e| !matches!(e, Entry::Kept { .. }))
                .count(),
            _ => 0,
        };
        let toggle = |label: &'static str, history: bool| {
            let on = page.history == history;
            keys::press(
                button(
                    text(label)
                        .size(style::SMALL)
                        .color(if on { P.fg } else { P.fg_dim }),
                )
                .padding([2, 8])
                .style(style::quiet),
                event(Event::History(history)),
            )
        };
        let heading = row![
            text(if page.history {
                "What was kept, reverted and undone".to_string()
            } else {
                format!("{count} pending change(s)")
            })
            .size(style::TEXT)
            .color(P.fg_dim),
            space().width(Fill),
            toggle("Pending", false),
            toggle("History", true),
            keys::press_maybe(
                button(crate::icons::glyph(
                    "refresh",
                    13.0,
                    alpha(
                        P.fg,
                        if page.loading || page.history_loading {
                            0.3
                        } else {
                            0.7
                        }
                    ),
                ))
                .padding([4, 8])
                .style(style::quiet),
                Some(event(Event::Refresh)),
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let mut body = Column::new().spacing(2);
        if let Some(said) = &page.said {
            body =
                body.push(container(text(said).size(style::SMALL).color(P.fg_dim)).padding([4, 4]));
        }
        if page.history {
            body = self.history_rows(page, body);
        } else {
            match &page.entries {
                None => body = body.push(text("Reading changes…").color(P.fg_dim)),
                Some(Err(error)) => body = body.push(text(error).color(P.error)),
                Some(Ok(entries)) if entries.is_empty() => {
                    body = body.push(
                        container(
                            text("Nothing pending: every session's writes are kept or reverted.")
                                .color(P.fg_dim),
                        )
                        .padding([12, 4]),
                    )
                }
                Some(Ok(entries)) => {
                    let now = now();
                    for group in changes::groups(entries) {
                        body = self.group(page, body, &group, now);
                    }
                }
            }
        }
        column![
            container(heading).padding(Padding {
                top: 8.0,
                right: 12.0,
                bottom: 6.0,
                left: 16.0
            }),
            rule::horizontal(1).style(style::rule_style(P.border)),
            scrollable(container(body).padding([8, 12]))
                .direction(scrollable::Direction::Vertical(style::bar()))
                .id("changes")
                .height(Fill)
                .style(style::feed_scroll),
        ]
        .height(Fill)
        .into()
    }

    fn group<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        group: &Group,
        now: i64,
    ) -> Column<'a, Message> {
        let keys_of = |g: &Group| g.entries.iter().map(changes::entry_key).collect::<Vec<_>>();
        let mut header = row![].spacing(10).align_y(Alignment::Center);
        match &group.kind {
            Kind::Live { repo } => {
                header = header
                    .push(text(short(&repo.display)).size(style::TEXT))
                    .push(badge("git", P.success))
                    .push(badge("live", P.accent));
            }
            Kind::Session {
                title, ghost, repo, ..
            } => {
                header = header.push(text(crate::ui::one_line(title, 40)).size(style::TEXT));
                header = header.push(match repo {
                    Some(repo) => badge(name(repo), P.success),
                    None => badge("no git", P.warning),
                });
                if *ghost {
                    header = header.push(badge("ghost", P.fg_dim));
                }
                let all = keys_of(group);
                let keepable: Vec<String> = group
                    .entries
                    .iter()
                    .filter(|e| changes::keepable(e))
                    .map(changes::entry_key)
                    .collect();
                let armed =
                    page.armed.as_deref() == all.first().map(|k| format!("all {k}")).as_deref();
                header = header
                    .push(space().width(Fill))
                    .push(small(
                        "Keep all",
                        (!keepable.is_empty()).then(|| event(Event::KeepAll(keepable))),
                    ))
                    .push(keys::press(
                        button(
                            text(if armed { "Revert all?" } else { "Revert all" })
                                .size(style::SMALL)
                                .color(if armed { P.error } else { alpha(P.fg, 0.75) }),
                        )
                        .padding([2, 8])
                        .style(style::quiet),
                        event(Event::RevertAll(all)),
                    ));
            }
            Kind::Kept { title, .. } => {
                header = header
                    .push(
                        text(format!("Kept from {}", crate::ui::one_line(title, 36)))
                            .size(style::TEXT),
                    )
                    .push(badge("undo", P.fg_dim));
            }
        }
        body = body
            .push(container(header).padding(Padding {
                top: 12.0,
                right: 4.0,
                bottom: 4.0,
                left: 4.0,
            }))
            .push(rule::horizontal(1).style(style::rule_style(P.border)));
        for entry in &group.entries {
            body = self.entry_row(page, body, group, entry, now);
        }
        body
    }

    fn entry_row<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        group: &Group,
        entry: &Entry,
        now: i64,
    ) -> Column<'a, Message> {
        let key = changes::entry_key(entry);
        let armed = page.armed.as_deref() == Some(key.as_str());
        let (letter, color) = match entry {
            Entry::Live { status, .. } => (
                status.clone(),
                match status.as_str() {
                    "?" | "A" => P.success,
                    "D" => P.error,
                    _ => P.accent,
                },
            ),
            Entry::Overlay { kind, conflict, .. } => {
                let letter = match kind.as_str() {
                    "added" => "A",
                    "deleted" => "D",
                    _ => "M",
                };
                (
                    format!("{letter}{}", if *conflict { "!" } else { "" }),
                    match letter {
                        "A" => P.success,
                        "D" => P.error,
                        _ => P.accent,
                    },
                )
            }
            Entry::Kept { .. } => ("K".into(), P.fg_dim),
        };
        let shown = match (&group.kind, entry) {
            (Kind::Live { repo }, _) => relative(changes::path(entry), repo),
            (_, Entry::Kept { what, path, .. }) => format!("{} · {what}", short(&path.display)),
            _ => short(&changes::path(entry).display),
        };
        let open = page.open.contains_key(&key);
        let line = keys::press(
            button(
                row![
                    text(letter).size(style::SMALL).color(color).width(26),
                    container(
                        text(crate::ui::one_line(&shown, 120))
                            .size(style::SMALL)
                            .wrapping(text::Wrapping::None),
                    )
                    .width(Fill)
                    .clip(true),
                    crate::icons::glyph(
                        if open {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        },
                        12.0,
                        P.fg_dim
                    ),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding([3, 4])
            .width(Fill)
            .style(style::row_button),
            event(Event::Diff(key.clone())),
        );
        let mut actions = row![].spacing(2).align_y(Alignment::Center);
        match entry {
            Entry::Live { .. } => {
                actions = actions.push(revert(
                    &key,
                    armed,
                    "git checkout HEAD (an untracked file is removed)",
                ));
            }
            Entry::Overlay {
                conflict,
                protected,
                ..
            } => {
                if *conflict {
                    actions = actions.push(
                        text("changed on the host too")
                            .size(style::TINY)
                            .color(P.warning),
                    );
                } else if let Some(seconds) = changes::reverts_in(entry, page.auto_revert, now) {
                    actions = actions.push(
                        text(if seconds == 0 {
                            "reverting".to_string()
                        } else {
                            format!("reverts in {}", changes::human(seconds))
                        })
                        .size(style::TINY)
                        .color(P.fg_dim),
                    );
                }
                let keep = small(
                    "Keep",
                    changes::keepable(entry).then(|| event(Event::Keep(key.clone()))),
                );
                actions = actions.push(match protected {
                    Some(why) => tooltip(
                        keep,
                        container(
                            text(format!(
                                "not kept by sill: {why} — do it by hand if you mean it"
                            ))
                            .size(style::TINY),
                        )
                        .padding([3, 8])
                        .style(style::fill(P.surface)),
                        tooltip::Position::Left,
                    )
                    .into(),
                    None => keep,
                });
                actions = actions.push(revert(&key, armed, "drop the session's write"));
            }
            Entry::Kept { .. } => {
                actions = actions.push(small("Undo", Some(event(Event::Undo(key.clone())))));
            }
        }
        body = body.push(row![line, actions].align_y(Alignment::Center));
        if let Some(diff) = page.open.get(&key) {
            body = body.push(match diff {
                None => container(text("Reading the diff…").size(style::SMALL).color(P.fg_dim))
                    .padding([4, 30])
                    .into(),
                Some(Err(error)) => container(text(error).size(style::SMALL).color(P.error))
                    .padding([4, 30])
                    .into(),
                Some(Ok(diff)) => Element::from(
                    container(
                        container(crate::diff_ui::diff_rows(
                            diff,
                            &changes::path(entry).display,
                        ))
                        .padding([6, 10])
                        .style(style::fill(P.code_bg)),
                    )
                    .padding(Padding {
                        top: 2.0,
                        right: 4.0,
                        bottom: 8.0,
                        left: 30.0,
                    }),
                ),
            });
        }
        body
    }

    fn history_rows<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
    ) -> Column<'a, Message> {
        if let Some(error) = &page.history_error {
            body = body.push(text(error).color(P.error));
        }
        if page.events.is_empty() && !page.history_loading && page.history_error.is_none() {
            body = body.push(text("No history yet.").color(P.fg_dim));
        }
        for event in &page.events {
            let what = match (&event.path, event.paths.len()) {
                (Some(path), _) => short(&path.display),
                (None, n) if n > 0 => format!("{n} files"),
                _ => event.what.clone(),
            };
            let when = event
                .timestamp
                .parse::<i64>()
                .map_or_else(|_| event.timestamp.clone(), |t| ago(now() - t));
            body = body.push(
                row![
                    text(when).size(style::TINY).color(P.fg_dim).width(70),
                    text(&event.kind)
                        .size(style::SMALL)
                        .color(P.accent)
                        .width(80),
                    container(
                        text(crate::ui::one_line(
                            &format!("{what} — {}", event.session_title),
                            110
                        ))
                        .size(style::SMALL)
                        .wrapping(text::Wrapping::None),
                    )
                    .width(Fill)
                    .clip(true),
                ]
                .spacing(6)
                .padding([2, 4])
                .align_y(Alignment::Center),
            );
        }
        if page.next.is_some() {
            body = body.push(small(
                if page.history_loading {
                    "Loading…"
                } else {
                    "Load more…"
                },
                (!page.history_loading).then(|| event(Event::MoreHistory)),
            ));
        }
        body
    }
}

/// Revert, on a second press.
fn revert<'a>(key: &str, armed: bool, tip: &'a str) -> Element<'a, Message> {
    tooltip(
        keys::press(
            button(
                text(if armed { "Revert?" } else { "Revert" })
                    .size(style::SMALL)
                    .color(if armed { P.error } else { alpha(P.fg, 0.75) }),
            )
            .padding([2, 8])
            .style(style::quiet),
            event(Event::Revert(key.to_string())),
        ),
        container(text(if armed { "Press again to revert" } else { tip }).size(style::TINY))
            .padding([3, 8])
            .style(style::fill(P.surface)),
        tooltip::Position::Left,
    )
    .into()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `just now`, `3 min ago`, `2 h ago`, `4 d ago`.
fn ago(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    if minutes < 1 {
        "just now".into()
    } else if minutes < 120 {
        format!("{minutes} min ago")
    } else if minutes < 48 * 60 {
        format!("{} h ago", minutes / 60)
    } else {
        format!("{} d ago", minutes / 1440)
    }
}
