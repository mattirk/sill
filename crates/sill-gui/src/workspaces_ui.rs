//! The shelf above the prompt (docs/parallel-coding.md §12): once the agent's turn ends, a line
//! for each workspace with work not yet applied — what it would bring, Apply, Discard — and its
//! diff on request. It never blocks the conversation: the person may go on talking. Also the pin
//! chips' marks for workspaces and repositories kept live.
use iced::widget::{Column, button, column, container, row, scrollable, space, text, tooltip};
use iced::{Alignment, Element, Fill, Length};
use sill_client::workspaces as ws;

use crate::style::{self, P, alpha};
use crate::workspaces::Event;
use crate::{App, Message, SessionView, keys};

fn quiet_button<'a>(label: impl Into<String>, on: Option<Message>) -> Element<'a, Message> {
    keys::press_maybe(button(text(label.into()).size(style::SMALL)).padding([3, 8]).style(style::quiet), on)
}

impl App {
    /// The shelf for `view`, or nothing when no workspace has work waiting or the agent is in a turn.
    pub(crate) fn workspace_shelf<'a>(&'a self, view: &'a SessionView) -> Option<Element<'a, Message>> {
        let state = &view.workspaces;
        let offered = state.offered();
        if offered.is_empty() || view.conversation.busy() {
            return None;
        }
        let session = view.id.clone();
        let mut shelf = Column::new().spacing(2);
        for d in offered {
            let id = d.workspace.workspace_id.clone();
            let running = state.running.is_some();
            let ev = |e: Event| (!running).then_some(Message::Workspaces(e));
            let name = d.workspace.source.display.rsplit('/').next().unwrap_or(&d.workspace.source.display).to_string();
            let said = ws::describe(&d.summary);
            let summary = if said.is_empty() { "changes".to_string() } else { said };
            let mut line = row![
                crate::icons::glyph("git-branch", 13.0, alpha(P.accent, 0.9)),
                text(name).size(style::SMALL).width(Fill).wrapping(text::Wrapping::WordOrGlyph),
                tooltip(
                    quiet_button(summary, Some(Message::Workspaces(Event::Diff(session.clone(), id.clone())))),
                    container(text("What Apply would bring: show or hide the diff").size(style::TINY)).padding(6).style(style::fill(P.surface)),
                    tooltip::Position::Top,
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center);
            if state.running.as_deref() == Some(id.as_str()) {
                line = line.push(text("Working…").size(style::TINY).color(P.fg_dim));
            }
            if d.workspace.operation.as_ref().is_some_and(|o| o.state == "conflict") {
                line = line.push(tooltip(
                    keys::press_maybe(
                        button(text("Resolve").size(style::SMALL)).padding([3, 10]).style(style::solid(P.accent)),
                        ev(Event::Resolve(session.clone(), id.clone())),
                    ),
                    container(text("Ask the agent to resolve the conflict and finish the rebase").size(style::TINY)).padding(6).style(style::fill(P.surface)),
                    tooltip::Position::Top,
                ));
                line = line.push(quiet_button("Abort rebase", ev(Event::Abort(session.clone(), id.clone()))));
            } else {
                line = line.push(keys::press_maybe(
                    button(text("Apply").size(style::SMALL)).padding([3, 10]).style(style::solid(P.accent)),
                    ev(Event::Apply(session.clone(), id.clone())),
                ));
            }
            let discard = if state.armed.as_deref() == Some(id.as_str()) {
                format!("Discard {}?", if d.summary.files > 0 { format!("{} file{}", d.summary.files, if d.summary.files == 1 { "" } else { "s" }) } else { "it".into() })
            } else {
                "Discard".into()
            };
            line = line.push(quiet_button(discard, ev(Event::Discard(session.clone(), id.clone()))));
            shelf = shelf.push(line);
            let notice = state.notices.get(&id).map(|(said, bad)| (said.as_str(), if *bad { P.error } else { P.fg_dim }))
                .or_else(|| d.workspace.operation.as_ref().map(|op| (op.detail.as_str(), P.warning)));
            if let Some((said, color)) = notice {
                shelf = shelf.push(text(said).size(style::TINY).color(color).width(Fill).wrapping(text::Wrapping::WordOrGlyph));
            }
            if let Some((open, diff)) = &state.diff
                && *open == id
            {
                let body: Element<'_, Message> = match diff {
                    Ok(t) if t.is_empty() => text("Loading…").size(style::TINY).color(P.fg_dim).into(),
                    Ok(t) => crate::diff_ui::diff_rows(t, "workspace.diff").into(),
                    Err(e) => text(e.clone()).size(style::TINY).color(P.error).into(),
                };
                shelf = shelf.push(
                    container(scrollable(body).direction(scrollable::Direction::Both { vertical: style::bar(), horizontal: style::bar() }).style(style::feed_scroll))
                        .max_height(260)
                        .width(Fill)
                        .padding([4, 6])
                        .style(style::fill(alpha(P.fg, 0.04))),
                );
            }
        }
        Some(
            column![container(shelf).padding([4, 12]).width(Fill).style(style::fill(alpha(P.accent, 0.05)))]
                .into(),
        )
    }

    /// Refresh the Changes page when it is open (an Apply changed a checkout it shows).
    pub(crate) fn changes_refresh_if_shown(&self) -> iced::Task<Message> {
        if self.changes.is_some() { self.changes_load() } else { iced::Task::none() }
    }
}

/// A pin chip's mark: the branch glyph for a workspace, the folder's own for live, with a warning
/// glyph and its reason when the repository could not have a workspace.
pub(crate) fn chip_mark<'a>(view: &SessionView, path: &sill_proto::sessions::HostPath, glyph: &'static str) -> (Element<'a, Message>, Option<String>) {
    if let Some(w) = view.workspaces.workspace_of(path) {
        return (
            crate::icons::glyph("git-branch", 14.0, alpha(P.accent, 0.9)).into(),
            Some(format!("Workspace: this session's own copy, branch {}. Your checkout changes only when you apply its work.", w.branch)),
        );
    }
    if let Some(live) = view.workspaces.live_reason(path) {
        return (
            row![crate::icons::glyph(glyph, 14.0, alpha(P.fg, 0.6)), crate::icons::glyph("alert", 11.0, P.warning)].spacing(2).align_y(Alignment::Center).into(),
            Some(live.message.clone()),
        );
    }
    (crate::icons::glyph(glyph, 14.0, alpha(P.fg, 0.6)).into(), None)
}

impl App {
    /// The Changes page's Workspaces section: every session's workspace with work not yet
    /// applied, Apply and Discard, then discarded work with Undo (7 days).
    pub(crate) fn workspace_rows<'a>(&'a self, page: &'a crate::changes_ui::ChangesPage, mut body: Column<'a, Message>) -> Column<'a, Message> {
        let Some(Ok((listed, details))) = &page.workspaces else { return body };
        let waiting: Vec<_> = details.iter().filter(|d| ws::pending(d)).collect();
        if waiting.is_empty() && listed.discarded.is_empty() {
            return body;
        }
        body = body.push(container(text("Workspaces").size(style::SMALL).color(P.fg_dim)).padding([6, 4]));
        for d in waiting {
            let id = d.workspace.workspace_id.clone();
            let name = d.workspace.source.display.clone();
            let session = self
                .all_sessions
                .iter()
                .find(|s| s.session_id == d.workspace.session_id)
                .map(|s| s.title.clone())
                .unwrap_or_else(|| d.workspace.session_id.clone());
            let state = match d.workspace.state {
                sill_proto::workspaces::WorkspaceState::Open => "",
                sill_proto::workspaces::WorkspaceState::Result => " · session ended",
            };
            let discard = if page.workspace_armed.as_deref() == Some(id.as_str()) { "Discard?" } else { "Discard" };
            let mut line = row![
                crate::icons::glyph("git-branch", 13.0, alpha(P.accent, 0.9)),
                text(name).size(style::SMALL),
                text(format!("{} · {session}{state}", ws::describe(&d.summary))).size(style::TINY).color(P.fg_dim),
                space().width(Length::Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            if let Some(op) = &d.workspace.operation {
                line = line.push(text(op.detail.clone()).size(style::TINY).color(P.warning));
            }
            // a stopped or interrupted operation is seen to first; a carry's line leaves Apply offered
            if !d.workspace.operation.as_ref().is_some_and(|o| o.state == "conflict" || o.state == "interrupted") {
                line = line.push(keys::press(
                    button(text("Apply").size(style::SMALL)).padding([3, 10]).style(style::solid(P.accent)),
                    Message::Workspaces(Event::PageApply(id.clone())),
                ));
            }
            line = line.push(quiet_button(discard, Some(Message::Workspaces(Event::PageDiscard(id)))));
            body = body.push(container(line).padding([3, 4]));
        }
        for d in &listed.discarded {
            let line = row![
                crate::icons::glyph("git-branch", 13.0, alpha(P.fg, 0.4)),
                text(d.source.display.clone()).size(style::SMALL).color(P.fg_dim),
                text(format!("discarded · kept until {}", d.until.get(..10).unwrap_or(&d.until))).size(style::TINY).color(P.fg_dim),
                space().width(Length::Fill),
                quiet_button("Undo", Some(Message::Workspaces(Event::PageUndo(d.discard_id.clone())))),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            body = body.push(container(line).padding([3, 4]));
        }
        body.push(space().height(8))
    }
}
