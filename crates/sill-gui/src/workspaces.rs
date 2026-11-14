//! A session's workspaces in the window (docs/parallel-coding.md §12): what its jail has for each
//! pinned repository, the shelf above the prompt that offers Apply and Discard once the agent's
//! turn ends, and the pin chips' marks. State belongs to the session; the work is the daemon's
//! (`sill_client::workspaces`), read off the GUI thread.
use std::collections::BTreeMap;

use iced::Task;
use sill_client::workspaces::{self as ws, ApplyOutcome, Loaded};
use sill_proto::workspaces as api;

use crate::{App, Message, blocking};

/// One session's workspaces as the window knows them.
#[derive(Debug, Default)]
pub struct State {
    pub loaded: Option<Loaded>,
    /// The workspace whose Discard was clicked once: the next click discards.
    pub armed: Option<String>,
    /// The workspace an action is running for.
    pub running: Option<String>,
    /// What the last action on a workspace said: done, or why not.
    pub notices: BTreeMap<String, (String, bool)>,
    /// The workspace whose diff is open in the shelf, and the diff.
    pub diff: Option<(String, Result<String, String>)>,
}

impl State {
    /// A pinned folder that is this session's workspace.
    pub fn workspace_of(&self, path: &sill_proto::sessions::HostPath) -> Option<&api::Workspace> {
        self.loaded.as_ref()?.view.workspaces.iter().find(|w| w.source.bytes_base64 == path.bytes_base64)
    }

    /// A pinned repository the session keeps live although it works in workspaces, and why.
    pub fn live_reason(&self, path: &sill_proto::sessions::HostPath) -> Option<&api::LiveRepository> {
        self.loaded.as_ref()?.view.live.iter().find(|l| l.source.bytes_base64 == path.bytes_base64)
    }

    /// The workspaces the shelf offers: those with something to apply or a stopped operation.
    pub fn offered(&self) -> Vec<&api::Detail> {
        self.loaded.as_ref().map(|l| l.details.iter().filter(|d| ws::pending(d)).collect()).unwrap_or_default()
    }
}

/// Every session's workspaces with their state, and the discarded work still undoable.
pub type All = (api::Listed, Vec<api::Detail>);

#[derive(Debug, Clone)]
pub enum Event {
    /// The Changes page's workspaces: loaded, and the person's actions there.
    PageLoaded(Result<All, String>),
    PageApply(String),
    PageDiscard(String),
    PageUndo(String),
    PageDone(String, bool),
    Loaded(String, Result<Loaded, String>),
    Apply(String, String),
    Discard(String, String),
    Abort(String, String),
    /// Click elsewhere: a Discard waiting for its second click is not wanted.
    Disarm(String),
    /// An action ended: the session, the workspace, what to say and whether it went wrong.
    Done(String, String, String, bool),
    Diff(String, String),
    DiffLoaded(String, String, Result<String, String>),
}

impl App {
    /// Read `session`'s workspaces; refreshed with its files (a turn's end, the 15 s poll, opening).
    pub(crate) fn workspaces_load(&self, session: &str) -> Task<Message> {
        let Some(actions) = self.actions.clone() else { return Task::none() };
        let id = session.to_string();
        blocking(
            move || ws::load(&actions, &id).map_err(|e| ws::message(&e)),
            {
                let id = session.to_string();
                move |loaded| Message::Workspaces(Event::Loaded(id, loaded))
            },
        )
    }

    /// The Changes page's workspaces, read with its changes.
    pub(crate) fn workspaces_page_load(&self) -> Task<Message> {
        let Some(actions) = self.actions.clone() else { return Task::none() };
        blocking(move || ws::all(&actions).map_err(|e| ws::message(&e)), |all| Message::Workspaces(Event::PageLoaded(all)))
    }

    /// The revision the Changes page shows for a workspace.
    fn page_seen(&self, workspace: &str) -> Option<api::Revision> {
        let Some(Ok((_, details))) = self.changes.as_ref()?.workspaces.as_ref() else { return None };
        details.iter().find(|d| d.workspace.workspace_id == workspace).map(|d| d.revision.clone())
    }

    pub(crate) fn workspaces_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::PageLoaded(all) => {
                if let Some(page) = &mut self.changes {
                    page.workspaces = Some(all);
                }
                Task::none()
            }
            Event::PageApply(workspace) => {
                let (Some(actions), Some(seen)) = (self.actions.clone(), self.page_seen(&workspace)) else { return Task::none() };
                blocking(
                    move || match ws::apply(&actions, &workspace, &seen) {
                        Ok(ApplyOutcome::Applied) => ("Applied to your checkout.".to_string(), false),
                        Ok(ApplyOutcome::Rebased) => ("Your checkout had moved: the work is rebased onto it. Look again, then Apply.".into(), false),
                        Ok(ApplyOutcome::Conflicts(paths)) => (format!("The rebase stopped on {}: resolve it in the session, or abort it there.", paths.join(", ")), true),
                        Err(e) => (ws::message(&e), true),
                    },
                    |(said, bad)| Message::Workspaces(Event::PageDone(said, bad)),
                )
            }
            Event::PageDiscard(workspace) => {
                let Some(page) = &mut self.changes else { return Task::none() };
                if page.workspace_armed.as_deref() != Some(workspace.as_str()) {
                    page.workspace_armed = Some(workspace);
                    return Task::none();
                }
                page.workspace_armed = None;
                let (Some(actions), Some(seen)) = (self.actions.clone(), self.page_seen(&workspace)) else { return Task::none() };
                blocking(
                    move || match ws::discard(&actions, &workspace, &seen) {
                        Ok(_) => ("Discarded. Undo below for 7 days.".to_string(), false),
                        Err(e) => (ws::message(&e), true),
                    },
                    |(said, bad)| Message::Workspaces(Event::PageDone(said, bad)),
                )
            }
            Event::PageUndo(discard) => {
                let Some(actions) = self.actions.clone() else { return Task::none() };
                blocking(
                    move || match ws::undo(&actions, &discard) {
                        Ok(_) => ("The discarded work is back.".to_string(), false),
                        Err(e) => (ws::message(&e), true),
                    },
                    |(said, bad)| Message::Workspaces(Event::PageDone(said, bad)),
                )
            }
            Event::PageDone(said, _bad) => {
                if let Some(page) = &mut self.changes {
                    page.said = Some(said);
                }
                let mut tasks = vec![self.changes_refresh_if_shown()];
                if let Some(id) = self.selected.clone() {
                    tasks.push(self.files_refresh(&id));
                }
                Task::batch(tasks)
            }
            Event::Loaded(session, loaded) => {
                let Some(view) = self.views.get_mut(&session) else { return Task::none() };
                let Ok(loaded) = loaded else { return Task::none() };
                let aliases = loaded.aliases();
                // a workspace that went: its notice and diff too
                view.workspaces.notices.retain(|id, _| {
                    loaded.detail(id).is_some_and(|new| {
                        view.workspaces.loaded.as_ref().and_then(|old| old.detail(id))
                            .is_some_and(|old| old.revision == new.revision)
                    })
                });
                if view.workspaces.diff.as_ref().is_some_and(|(id, _)| loaded.detail(id).is_none_or(|d| !ws::pending(d))) {
                    view.workspaces.diff = None;
                }
                view.workspaces.loaded = Some(loaded);
                // the drawer reads the agent's copies: what it showed was the person's checkout
                if self.drawer.set_aliases(&session, aliases) {
                    return self.files_refresh(&session);
                }
                Task::none()
            }
            Event::Disarm(session) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.workspaces.armed = None;
                }
                Task::none()
            }
            Event::Apply(session, workspace) => {
                let Some((actions, seen)) = self.workspace_action(&session, &workspace) else { return Task::none() };
                blocking(
                    move || match ws::apply(&actions, &workspace, &seen) {
                        Ok(ApplyOutcome::Applied) => (workspace, "Applied to your checkout.".to_string(), false),
                        Ok(ApplyOutcome::Rebased) => (workspace, "Your checkout had moved: the work is rebased onto it. Look again, then Apply.".into(), false),
                        Ok(ApplyOutcome::Conflicts(paths)) => (workspace, format!("The rebase onto your checkout stopped on {}: resolve it in the session, or Abort.", paths.join(", ")), true),
                        Err(e) => (workspace, ws::message(&e), true),
                    },
                    move |(w, said, bad)| Message::Workspaces(Event::Done(session, w, said, bad)),
                )
            }
            Event::Discard(session, workspace) => {
                let Some(view) = self.views.get_mut(&session) else { return Task::none() };
                if view.workspaces.armed.as_deref() != Some(workspace.as_str()) {
                    view.workspaces.armed = Some(workspace);
                    return Task::none();
                }
                view.workspaces.armed = None;
                let Some((actions, seen)) = self.workspace_action(&session, &workspace) else { return Task::none() };
                blocking(
                    move || match ws::discard(&actions, &workspace, &seen) {
                        Ok(_) => (workspace, "Discarded. Undo on Changes for 7 days.".to_string(), false),
                        Err(e) => (workspace, ws::message(&e), true),
                    },
                    move |(w, said, bad)| Message::Workspaces(Event::Done(session, w, said, bad)),
                )
            }
            Event::Abort(session, workspace) => {
                let Some((actions, _)) = self.workspace_action(&session, &workspace) else { return Task::none() };
                blocking(
                    move || match ws::abort(&actions, &workspace) {
                        Ok(_) => (workspace, "The rebase is aborted; the work is as it was.".to_string(), false),
                        Err(e) => (workspace, ws::message(&e), true),
                    },
                    move |(w, said, bad)| Message::Workspaces(Event::Done(session, w, said, bad)),
                )
            }
            Event::Done(session, workspace, said, bad) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.workspaces.running = None;
                    view.workspaces.notices.insert(workspace, (said, bad));
                    view.workspaces.diff = None;
                }
                Task::batch([self.files_refresh(&session), self.changes_refresh_if_shown()])
            }
            Event::Diff(session, workspace) => {
                let Some(view) = self.views.get_mut(&session) else { return Task::none() };
                if view.workspaces.diff.as_ref().is_some_and(|(id, _)| *id == workspace) {
                    view.workspaces.diff = None;
                    return Task::none();
                }
                view.workspaces.diff = Some((workspace.clone(), Ok(String::new())));
                let Some(actions) = self.actions.clone() else { return Task::none() };
                blocking(
                    {
                        let workspace = workspace.clone();
                        move || ws::diff(&actions, &workspace).map_err(|e| ws::message(&e))
                    },
                    move |diff| Message::Workspaces(Event::DiffLoaded(session, workspace, diff)),
                )
            }
            Event::DiffLoaded(session, workspace, diff) => {
                if let Some(view) = self.views.get_mut(&session)
                    && view.workspaces.diff.as_ref().is_some_and(|(id, _)| *id == workspace)
                {
                    view.workspaces.diff = Some((workspace, diff));
                }
                Task::none()
            }
        }
    }

    /// The client and the revision the person saw, marking the action as running; None when one
    /// already runs or the workspace is not known.
    fn workspace_action(&mut self, session: &str, workspace: &str) -> Option<(std::sync::Arc<sill_client::actions::Actions>, api::Revision)> {
        let actions = self.actions.clone()?;
        let view = self.views.get_mut(session)?;
        if view.workspaces.running.is_some() {
            return None;
        }
        let seen = view.workspaces.loaded.as_ref()?.detail(workspace)?.revision.clone();
        view.workspaces.running = Some(workspace.to_string());
        view.workspaces.notices.remove(workspace);
        Some((actions, seen))
    }
}

#[cfg(test)]
#[path = "workspaces_tests.rs"]
mod tests;
