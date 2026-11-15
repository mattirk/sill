//! The Changes page (reference `07`, `12`), opened with the herring: what waits for the person,
//! by whose work it is. Each session has one section — its workspaces with what an Apply would
//! bring, file by file; what it wrote outside its pinned folders, held in its overlay until kept
//! or reverted; and what was kept from it, which can be undone. After the sessions come the
//! person's own checkouts (the pinned repositories as git sees them, live on the host) and
//! discarded workspaces. A row opens its diff. Reverting and discarding ask for a second press; a
//! keep is refused for a file the host changed meanwhile or one sill does not keep. History lists
//! what was kept, reverted and undone.
//!
//! What the page lists stays with the window while the page is closed ([`Listing`]), so opening
//! it shows the last listing at once while the new one is read; its actions wait for the new one.
use std::collections::BTreeMap;

use iced::widget::{
    Column, button, column, container, row, rule, scrollable, space, text, tooltip,
};
use iced::{Alignment, Element, Fill, Padding};
use sill_client::changes::{self, Entry, Group, Kind, SessionSection};
use sill_client::files::{key, name, relative};
use sill_client::workspaces as ws;
use sill_proto::changes::HistoryEvent;
use sill_proto::sessions::HostPath;
use sill_proto::workspaces as api;

use crate::armed::Armed;
use crate::style::{self, P, alpha};
use crate::{App, Message, keys};

/// What the page lists, as last read. The window's, not the page's: it outlives the page.
#[derive(Default)]
pub struct Listing {
    /// Pending entries; `None` before the first answer.
    pub entries: Option<Result<Vec<Entry>, String>>,
    /// Sessions whose writes could not be listed: each a note, not an error over the page.
    pub unreadable: Vec<changes::Unreadable>,
    /// Settings → auto-revert, minutes (0 is off).
    pub auto_revert: u64,
    /// Every session's workspaces and the discarded work still undoable (docs/parallel-coding.md
    /// §12); `None` before the first answer.
    pub workspaces: Option<Result<crate::workspaces::All, String>>,
}

impl Listing {
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries
            .as_ref()?
            .as_ref()
            .ok()?
            .iter()
            .find(|e| changes::entry_key(e) == key)
    }

    pub fn details(&self) -> &[api::Detail] {
        match &self.workspaces {
            Some(Ok((_, details))) => details,
            _ => &[],
        }
    }

    /// The workspaces that cannot be read, each with why: a note, not an error over the page.
    pub fn unread_workspaces(&self) -> impl Iterator<Item = (&api::Workspace, &str)> {
        let listed = match &self.workspaces {
            Some(Ok((listed, _))) => listed.workspaces.as_slice(),
            _ => &[],
        };
        listed.iter().filter_map(|w| w.operation.as_ref().filter(|o| o.kind == "read").map(|o| (w, o.detail.as_str())))
    }

    pub fn discarded(&self) -> &[api::Discarded] {
        match &self.workspaces {
            Some(Ok((listed, _))) => &listed.discarded,
            _ => &[],
        }
    }
}

#[derive(Default)]
pub struct ChangesPage {
    pub loading: bool,
    /// The entries, and the workspaces, were read since the page was opened: until then the page
    /// shows the listing from before, and offers no action on it.
    pub fresh_entries: bool,
    pub fresh_workspaces: bool,
    pub history: bool,
    pub events: Vec<HistoryEvent>,
    pub next: Option<String>,
    pub history_error: Option<String>,
    pub history_loading: bool,
    /// Open rows' diffs by entry key ([`workspace_key`] for a workspace's file); `None` while
    /// reading.
    pub open: BTreeMap<String, Option<Result<String, String>>>,
    /// A revert pressed once.
    pub armed: Armed<Reverting>,
    /// What the last action did.
    pub said: Option<String>,
    /// A workspace's Discard pressed once, and the revision the page showed then.
    pub workspace_armed: Armed<(String, api::Revision)>,
    /// An unreadable workspace's Remove pressed once.
    pub workspace_remove_armed: Armed<String>,
    /// A workspace's Apply onto another branch pressed once, and the revision the page showed
    /// then.
    pub workspace_apply_armed: Armed<(String, api::Revision)>,
    /// An Apply onto another branch the person confirmed: the workspace and that branch. It
    /// lasts until the Apply ends, and goes with it to the session when the agent commits first.
    pub workspace_apply_onto: Option<(String, String)>,
}

/// What a revert pressed once is of.
#[derive(Debug, Clone, PartialEq)]
pub enum Reverting {
    /// One entry, by its key.
    One(String),
    /// Every entry of a group: the keys it held when pressed, so a group that changed since is
    /// asked about again.
    All(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The entries, the sessions whose writes could not be listed, and auto-revert's minutes.
    Loaded(Result<(Vec<Entry>, Vec<changes::Unreadable>, u64), String>),
    Refresh,
    History(bool),
    HistoryLoaded(Result<sill_proto::changes::HistoryPage, String>, bool),
    MoreHistory,
    Diff(String),
    /// A workspace's file: what an Apply would bring of it.
    WorkspaceDiff(String, HostPath),
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

/// The key a workspace's file has among the open diffs.
pub fn workspace_key(workspace_id: &str, path: &HostPath) -> String {
    format!("ws {workspace_id} {}", key(path))
}

/// The workspace an open diff's key is of, when it is a workspace's file.
pub fn workspace_of(open_key: &str) -> Option<&str> {
    open_key.strip_prefix("ws ")?.split(' ').next()
}

/// A destructive button's text: red once armed, and dimmed like any other while not offered.
fn warn_color(armed: bool, offered: bool) -> iced::Color {
    match (armed, offered) {
        (true, _) => P.error,
        (false, true) => alpha(P.fg, 0.75),
        (false, false) => alpha(P.fg, 0.3),
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

/// A changed file: its name, then its folder dimmed (and what was done, for a keep).
fn path_label<'a>(shown: &str, what: Option<&str>) -> Element<'a, Message> {
    let shown = crate::ui::one_line(shown, 200);
    let (folder, file) = match shown.trim_end_matches('/').rsplit_once('/') {
        Some((folder, file)) => (folder.to_string(), file.to_string()),
        None => (String::new(), shown.clone()),
    };
    let dim = |value: String| {
        text(value)
            .size(style::SMALL)
            .color(alpha(P.fg, 0.45))
            .wrapping(text::Wrapping::None)
    };
    let mut line = row![text(file).size(style::SMALL).wrapping(text::Wrapping::None)]
        .spacing(10)
        .align_y(Alignment::Center);
    if !folder.is_empty() {
        line = line.push(dim(folder));
    }
    if let Some(what) = what {
        line = line.push(dim(what.to_string()));
    }
    line.into()
}

/// `~/…` for a path under the daemon's home.
fn short(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// A row's letter and its colour: added, deleted, anything else.
fn letter_color(letter: &str) -> iced::Color {
    match letter {
        "?" | "A" => P.success,
        "D" => P.error,
        _ => P.accent,
    }
}

/// A file's row: its letter, name and folder, what else to say, and the fold's chevron.
fn file_line<'a>(
    letter: String,
    color: iced::Color,
    label: Element<'a, Message>,
    open: bool,
    on_press: Message,
) -> Element<'a, Message> {
    keys::press(
        button(
            row![
                text(letter).size(style::SMALL).color(color).width(26),
                container(label).width(Fill).clip(true),
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
        on_press,
    )
}

/// An open row's diff, below it.
fn diff_block<'a>(diff: &'a Option<Result<String, String>>, path: &str) -> Element<'a, Message> {
    match diff {
        None => container(text("Reading the diff…").size(style::SMALL).color(P.fg_dim))
            .padding([4, 30])
            .into(),
        Some(Err(error)) => container(text(error).size(style::SMALL).color(P.error))
            .padding([4, 30])
            .into(),
        Some(Ok(diff)) => container(
            container(crate::diff_ui::diff_rows(diff, path))
                .padding([6, 10])
                .style(style::fill(P.code_bg)),
        )
        .padding(Padding {
            top: 2.0,
            right: 4.0,
            bottom: 8.0,
            left: 30.0,
        })
        .into(),
    }
}

/// A section's heading with the line under it.
fn section_head<'a>(body: Column<'a, Message>, header: iced::widget::Row<'a, Message>) -> Column<'a, Message> {
    body.push(container(header).padding(Padding {
        top: 14.0,
        right: 4.0,
        bottom: 4.0,
        left: 4.0,
    }))
    .push(rule::horizontal(1).style(style::rule_style(P.border)))
}

/// A part of a session's section: its heading, smaller, with its actions at the right.
fn part_head<'a>(header: iced::widget::Row<'a, Message>) -> Element<'a, Message> {
    container(header.height(26)).padding(Padding {
        top: 6.0,
        right: 4.0,
        bottom: 0.0,
        left: 4.0,
    })
    .into()
}

impl App {
    pub(crate) fn changes_view<'a>(&'a self, page: &'a ChangesPage) -> Element<'a, Message> {
        let listing = &self.changes_listing;
        let entries: &[Entry] = match &listing.entries {
            Some(Ok(entries)) => entries,
            _ => &[],
        };
        let order: Vec<&str> = self.all_sessions.iter().map(|s| s.session_id.as_str()).collect();
        let sections = changes::sections(entries, listing.details(), &order);
        let unread = listing.entries.is_none() && listing.workspaces.is_none();
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
        let waiting = || {
            let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
            match sections.waiting() {
                _ if unread => "Reading changes…".to_string(),
                (0, 0) => "Nothing waiting".to_string(),
                (w, 0) => format!("{} to apply", plural(w, "workspace", "workspaces")),
                (0, n) => format!("{} to keep or revert", plural(n, "write", "writes")),
                (w, n) => format!(
                    "{} to apply · {} to keep or revert",
                    plural(w, "workspace", "workspaces"),
                    plural(n, "write", "writes")
                ),
            }
        };
        let heading = row![
            text(if page.history {
                "What was kept, reverted and undone".to_string()
            } else {
                waiting()
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
            for error in [
                listing.entries.as_ref().and_then(|e| e.as_ref().err()),
                listing.workspaces.as_ref().and_then(|w| w.as_ref().err()),
            ]
            .into_iter()
            .flatten()
            {
                body = body.push(container(text(error).color(P.error)).padding([4, 4]));
            }
            for u in &listing.unreadable {
                body = body.push(
                    container(
                        column![
                            text(format!("{}: its writes outside a workspace cannot be listed", u.title))
                                .size(style::SMALL)
                                .color(P.fg),
                            text(format!(
                                "{}. A session launched by an older Sill cannot be read by this one: rebuild it, or delete it once its work is applied.",
                                u.why
                            ))
                            .size(style::TINY)
                            .color(P.fg_dim),
                        ]
                        .spacing(2),
                    )
                    .padding([4, 4]),
                );
            }
            for (w, why) in listing.unread_workspaces() {
                let armed = page.workspace_remove_armed.is(&w.workspace_id);
                let fresh = page.fresh_workspaces;
                // its folder goes with whatever it holds and nothing brings it back: asked twice
                let remove = keys::press_maybe(
                    button(text(if armed { "Remove folder and work?" } else { "Remove" }).size(style::SMALL).color(warn_color(armed, fresh)))
                        .padding([2, 8])
                        .style(style::quiet),
                    fresh.then(|| Message::Workspaces(crate::workspaces::Event::PageRemove(w.workspace_id.clone()))),
                );
                body = body.push(
                    container(
                        column![
                            row![
                                text(format!("{}: a workspace that cannot be read", w.name)).size(style::SMALL).color(P.fg),
                                space().width(Fill),
                                remove,
                            ]
                            .spacing(10)
                            .align_y(Alignment::Center),
                            text(why.to_string()).size(style::TINY).color(P.fg_dim),
                        ]
                        .spacing(2),
                    )
                    .padding([4, 4]),
                );
            }
            let discarded = listing.discarded();
            if unread {
                body = body.push(container(text("Reading changes…").color(P.fg_dim)).padding([12, 4]));
            } else if sections.is_empty() && discarded.is_empty() && matches!(listing.entries, Some(Ok(_))) {
                body = body.push(
                    container(
                        text("Nothing waiting: every session's work is applied, kept or reverted.")
                            .color(P.fg_dim),
                    )
                    .padding([12, 4]),
                );
            }
            let now = now();
            for section in &sections.sessions {
                body = self.session_section(page, body, section, now);
            }
            for group in &sections.checkouts {
                body = self.checkout(page, body, group, now);
            }
            if !discarded.is_empty() {
                body = self.discarded_rows(page, body, discarded);
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
            scrollable(container(body).padding(Padding {
                top: 0.0,
                right: 12.0,
                bottom: 12.0,
                left: 12.0
            }))
                .direction(scrollable::Direction::Vertical(style::bar()))
                .id("changes")
                .height(Fill)
                .style(style::feed_scroll),
        ]
        .height(Fill)
        .into()
    }

    /// One session's work: its name, then its workspaces, its writes outside its pins and what
    /// was kept from it.
    fn session_section<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        section: &SessionSection<'a>,
        now: i64,
    ) -> Column<'a, Message> {
        let session = self.all_sessions.iter().find(|s| s.session_id == section.session_id);
        let mut header = row![].spacing(10).align_y(Alignment::Center);
        if let Some(session) = session {
            header = header.push(
                iced::widget::svg(self.agent_icon(&session.agent))
                    .width(15)
                    .height(15)
                    .style(style::icon(P.fg)),
            );
        }
        let title = session
            .map(sill_client::sessions::display_title)
            .or(section.title.as_deref())
            .unwrap_or(&section.session_id);
        header = header.push(text(crate::ui::one_line(title, 60)).size(style::TEXT));
        if section.ghost || session.is_some_and(|s| s.ghost) {
            header = header.push(badge("ghost", P.fg_dim));
        }
        if session.is_some_and(|s| s.ended_at.is_some()) {
            header = header.push(badge("ended", P.fg_dim));
        }
        body = section_head(body, header);
        for d in &section.workspaces {
            body = self.workspace_part(page, body, d, session.is_some_and(|s| s.ended_at.is_none()));
        }
        for group in &section.writes {
            body = self.writes_part(page, body, group, now);
        }
        if !section.kept.is_empty() {
            body = body.push(part_head(
                row![
                    text("Kept").size(style::SMALL),
                    text("on the host now; Undo puts back what was there").size(style::TINY).color(P.fg_dim),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ));
            for entry in &section.kept {
                body = self.entry_row(page, body, None, entry, now);
            }
        }
        body
    }

    /// A workspace with work waiting: the repository, what an Apply would bring, Apply and
    /// Discard, and the files it would change.
    fn workspace_part<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        d: &'a api::Detail,
        session_lives: bool,
    ) -> Column<'a, Message> {
        use crate::workspaces::Event as W;
        let id = d.workspace.workspace_id.clone();
        let fresh = page.fresh_workspaces;
        let mut said = ws::describe(&d.summary);
        // a result whose session goes on: its pin was removed, the jail no longer has it
        if d.workspace.state == api::WorkspaceState::Result && session_lives {
            said.push_str(" · no longer pinned");
        }
        let mut header = row![
            crate::icons::glyph("git-branch", 13.0, alpha(P.accent, 0.9)),
            text(name(&d.workspace.source).to_string()).size(style::SMALL),
            badge("workspace", P.accent),
            text(said).size(style::TINY).color(P.fg_dim),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(op) = &d.workspace.operation {
            header = header.push(
                container(text(op.detail.clone()).size(style::TINY).color(P.warning).wrapping(text::Wrapping::None))
                    .width(Fill)
                    .clip(true),
            );
        } else {
            header = header.push(space().width(Fill));
        }
        // a stopped or interrupted operation is seen to first; a carry's line leaves Apply offered
        if !d.workspace.operation.as_ref().is_some_and(|o| o.state == "conflict" || o.state == "interrupted") {
            let onto = sill_client::workspaces::other_branch(d)
                .map(|(_, now)| now)
                .filter(|now| {
                    page.workspace_apply_armed.get().is_some_and(|(w, seen)| *w == id && *seen == d.revision)
                        || page.workspace_apply_onto.as_ref().is_some_and(|(w, b)| *w == id && b == now)
                });
            let label = onto.map_or("Apply".to_string(), |now| format!("Apply onto {now}?"));
            header = header.push(keys::press_maybe(
                button(text(label).size(style::SMALL)).padding([2, 10]).style(style::solid(P.accent)),
                fresh.then(|| Message::Workspaces(W::PageApply(id.clone()))),
            ));
        }
        let armed = page.workspace_armed.get().is_some_and(|(w, seen)| *w == id && *seen == d.revision);
        header = header.push(keys::press_maybe(
            button(
                text(if armed { "Discard?" } else { "Discard" })
                    .size(style::SMALL)
                    .color(warn_color(armed, fresh)),
            )
            .padding([2, 8])
            .style(style::quiet),
            fresh.then(|| Message::Workspaces(W::PageDiscard(id.clone()))),
        ));
        body = body.push(part_head(header));
        for file in &d.files {
            let key = workspace_key(&id, &file.path);
            let mut label = row![path_label(&relative(&file.path, &d.workspace.source), None)]
                .spacing(10)
                .align_y(Alignment::Center);
            if file.added != "0" || file.removed != "0" {
                label = label.push(
                    text(format!("+{} −{}", file.added, file.removed))
                        .size(style::TINY)
                        .color(alpha(P.fg, 0.45))
                        .wrapping(text::Wrapping::None),
                );
            }
            body = body.push(file_line(
                file.status.clone(),
                letter_color(&file.status),
                label.into(),
                page.open.contains_key(&key),
                event(Event::WorkspaceDiff(id.clone(), file.path.clone())),
            ));
            if let Some(diff) = page.open.get(&key) {
                body = body.push(diff_block(diff, &file.path.display));
            }
        }
        body
    }

    /// A session's writes outside its pinned folders, in one repository (or none): held in its
    /// overlay until kept or reverted.
    fn writes_part<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        group: &Group,
        now: i64,
    ) -> Column<'a, Message> {
        let Kind::Session { repo, .. } = &group.kind else {
            return body;
        };
        let fresh = page.fresh_entries;
        let all: Vec<String> = group.entries.iter().map(changes::entry_key).collect();
        let keepable: Vec<String> = group
            .entries
            .iter()
            .filter(|e| changes::keeps_in_bulk(e))
            .map(changes::entry_key)
            .collect();
        let armed = page.armed.get().is_some_and(|a| matches!(a, Reverting::All(keys) if *keys == all));
        let header = row![
            crate::icons::glyph("file", 13.0, alpha(P.fg, 0.6)),
            text("Outside its pinned folders").size(style::SMALL),
            match repo {
                Some(repo) => badge(name(repo), P.success),
                None => badge("no git", P.warning),
            },
            text("held until kept or reverted").size(style::TINY).color(P.fg_dim),
            space().width(Fill),
            small(
                "Keep all",
                (fresh && !keepable.is_empty()).then(|| event(Event::KeepAll(keepable))),
            ),
            keys::press_maybe(
                button(
                    text(if armed { "Revert all?" } else { "Revert all" })
                        .size(style::SMALL)
                        .color(warn_color(armed, fresh)),
                )
                .padding([2, 8])
                .style(style::quiet),
                fresh.then(|| event(Event::RevertAll(all))),
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        body = body.push(part_head(header));
        for entry in &group.entries {
            body = self.entry_row(page, body, None, entry, now);
        }
        body
    }

    /// One of the person's own checkouts: a pinned repository as git sees it, live on the host.
    fn checkout<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        group: &Group,
        now: i64,
    ) -> Column<'a, Message> {
        let Kind::Live { repo } = &group.kind else {
            return body;
        };
        let header = row![
            crate::icons::glyph("folder-open", 14.0, alpha(P.fg, 0.8)),
            text(short(&repo.display)).size(style::TEXT),
            badge("your checkout", P.success),
            text("as git sees it, on the host now").size(style::TINY).color(P.fg_dim),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        body = section_head(body, header);
        for entry in &group.entries {
            body = self.entry_row(page, body, Some(repo), entry, now);
        }
        body
    }

    /// Discarded workspaces, undoable while they are kept.
    fn discarded_rows<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        discarded: &'a [api::Discarded],
    ) -> Column<'a, Message> {
        body = section_head(
            body,
            row![text("Discarded workspaces").size(style::TEXT).color(P.fg_dim)],
        );
        for d in discarded {
            let line = row![
                crate::icons::glyph("git-branch", 13.0, alpha(P.fg, 0.4)),
                text(name(&d.source).to_string()).size(style::SMALL).color(P.fg_dim),
                text(format!("kept until {}", d.until.get(..10).unwrap_or(&d.until)))
                    .size(style::TINY)
                    .color(P.fg_dim),
                space().width(Fill),
                small(
                    "Undo",
                    page.fresh_workspaces
                        .then(|| Message::Workspaces(crate::workspaces::Event::PageUndo(d.discard_id.clone()))),
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            body = body.push(container(line).padding([3, 4]));
        }
        body
    }

    /// An entry's row with its actions, and its diff below when open. `repo`: the checkout the
    /// entry is shown inside, its path relative to it.
    fn entry_row<'a>(
        &'a self,
        page: &'a ChangesPage,
        mut body: Column<'a, Message>,
        repo: Option<&HostPath>,
        entry: &Entry,
        now: i64,
    ) -> Column<'a, Message> {
        let key = changes::entry_key(entry);
        let armed = page.armed.get().is_some_and(|a| matches!(a, Reverting::One(k) if *k == key));
        let fresh = page.fresh_entries;
        let auto_revert = self.changes_listing.auto_revert;
        let (letter, color) = match entry {
            Entry::Live { status, .. } => (status.clone(), letter_color(status)),
            Entry::Overlay { kind, conflict, .. } => {
                let letter = match kind.as_str() {
                    "added" => "A",
                    "deleted" => "D",
                    _ => "M",
                };
                (
                    format!("{letter}{}", if *conflict { "!" } else { "" }),
                    letter_color(letter),
                )
            }
            Entry::Kept { .. } => ("K".into(), P.fg_dim),
        };
        let (shown, what) = match (repo, entry) {
            (Some(repo), _) => (relative(changes::path(entry), repo), None),
            (_, Entry::Kept { what, path, .. }) => (short(&path.display), Some(what.as_str())),
            _ => (short(&changes::path(entry).display), None),
        };
        let line = file_line(
            letter,
            color,
            path_label(&shown, what),
            page.open.contains_key(&key),
            event(Event::Diff(key.clone())),
        );
        let mut actions = row![].spacing(2).align_y(Alignment::Center);
        match entry {
            Entry::Live { .. } => {
                actions = actions.push(revert(
                    &key,
                    armed,
                    fresh,
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
                } else if let Some(seconds) = changes::reverts_in(entry, auto_revert, now) {
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
                    (fresh && changes::keepable(entry)).then(|| event(Event::Keep(key.clone()))),
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
                actions = actions.push(revert(&key, armed, fresh, "drop the session's write"));
            }
            Entry::Kept { .. } => {
                actions = actions.push(small("Undo", fresh.then(|| event(Event::Undo(key.clone())))));
            }
        }
        body = body.push(row![line, actions].align_y(Alignment::Center));
        if let Some(diff) = page.open.get(&key) {
            body = body.push(diff_block(diff, &changes::path(entry).display));
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
fn revert<'a>(key: &str, armed: bool, offered: bool, tip: &'a str) -> Element<'a, Message> {
    tooltip(
        keys::press_maybe(
            button(
                text(if armed { "Revert?" } else { "Revert" })
                    .size(style::SMALL)
                    .color(warn_color(armed, offered)),
            )
            .padding([2, 8])
            .style(style::quiet),
            offered.then(|| event(Event::Revert(key.to_string()))),
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
