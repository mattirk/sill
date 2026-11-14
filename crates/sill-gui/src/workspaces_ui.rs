//! The shelf above the prompt (docs/parallel-coding.md §12): once the agent's turn ends, a line
//! for each workspace with work not yet applied — what it would bring, Apply, Discard — and its
//! diff on request. It never blocks the conversation: the person may go on talking. Also the pin
//! chips' marks for workspaces and repositories kept live.
use iced::widget::{Column, button, column, container, row, scrollable, text, tooltip};
use iced::{Alignment, Element, Fill};
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
            let rebase_stopped = d.workspace.operation.as_ref().is_some_and(|o| o.state == "conflict");
            if rebase_stopped {
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
                // a second opinion before applying (docs/crosscheck.md)
                if let Some(crosscheck) = self.crosscheck_button(d) {
                    line = line.push(crosscheck);
                }
                let onto = ws::other_branch(d)
                    .map(|(_, now)| now)
                    .filter(|now| state.apply_armed.as_ref().is_some_and(|(w, b)| *w == id && b == now));
                let label = onto.map_or("Apply".to_string(), |now| format!("Apply onto {now}?"));
                let apply = keys::press_maybe(
                    button(text(label).size(style::SMALL)).padding([3, 10]).style(style::solid(P.accent)),
                    ev(Event::Apply(session.clone(), id.clone())),
                );
                // Apply brings commits only: said before the click when it will ask for one first
                let n = d.summary.uncommitted.len();
                if n > 0 {
                    let who = if d.workspace.state == sill_proto::workspaces::WorkspaceState::Open {
                        "the agent is asked to commit first, and this applies when its turn ends"
                    } else {
                        "sill commits first, as you, under the session's title"
                    };
                    line = line.push(tooltip(
                        apply,
                        container(text(format!("{n} uncommitted file{}: {who}", if n == 1 { "" } else { "s" })).size(style::TINY)).padding(6).style(style::fill(P.surface)),
                        tooltip::Position::Top,
                    ));
                } else {
                    line = line.push(apply);
                }
                line = line.push(tooltip(
                    keys::press_maybe(
                        // the caret points up while its menu is open
                        button(text(if state.menu.as_deref() == Some(id.as_str()) { "▴" } else { "▾" }).size(style::SMALL))
                            .padding([3, 6])
                            .style(style::solid(P.accent)),
                        ev(Event::Menu(session.clone(), id.clone())),
                    ),
                    container(text("Other ways to apply").size(style::TINY)).padding(6).style(style::fill(P.surface)),
                    tooltip::Position::Top,
                ));
            }
            let discard = if state.armed.as_deref() == Some(id.as_str()) {
                format!("Discard {}?", if d.summary.files > 0 { format!("{} file{}", d.summary.files, if d.summary.files == 1 { "" } else { "s" }) } else { "it".into() })
            } else {
                "Discard".into()
            };
            line = line.push(quiet_button(discard, ev(Event::Discard(session.clone(), id.clone()))));
            shelf = shelf.push(line);
            for row in self.crosscheck_rows(view, d) {
                shelf = shelf.push(row);
            }
            if state.menu.as_deref() == Some(id.as_str()) && !rebase_stopped {
                shelf = shelf.push(
                    row![
                        text("Writes the changes into your checkout as they are, uncommitted, for you to try. Nothing is committed and the work stays here to apply; other sessions do not get it.")
                            .size(style::TINY)
                            .color(P.fg_dim)
                            .width(Fill)
                            .wrapping(text::Wrapping::WordOrGlyph),
                        quiet_button("Apply without commit", ev(Event::Copy(session.clone(), id.clone()))),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                );
            }
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
