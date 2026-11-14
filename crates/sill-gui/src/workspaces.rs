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
    /// An Apply waiting for the agent to commit the workspace's uncommitted changes: the
    /// workspace and its state (`Detail::state_tree`) when the person clicked. It applies at the
    /// turn's end when that state is all committed and nothing else changed.
    pub committing: Option<(String, String)>,
}

impl State {
    /// A pinned folder that is this session's workspace.
    pub fn workspace_of(&self, path: &sill_proto::sessions::HostPath) -> Option<&api::Workspace> {
        self.loaded.as_ref()?.view.workspaces.iter().find(|w| w.source.bytes_base64 == path.bytes_base64)
    }

    /// What to say when the person's checkout of a pinned repository holds uncommitted work of
    /// their own that this session's workspace does not have (docs/parallel-coding.md §5).
    pub fn checkout_only(&self, path: &sill_proto::sessions::HostPath) -> Option<String> {
        let d = self.loaded.as_ref()?.details.iter().find(|d| d.workspace.source.bytes_base64 == path.bytes_base64)?;
        let n = d.checkout_only;
        (n > 0).then(|| format!("{n} uncommitted file{} in your checkout {} not in this workspace", if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" }))
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
    /// An Apply found uncommitted changes: ask the session's agent to commit them; the Apply
    /// follows at its turn's end.
    Commit(String, String),
    /// The turn after that prompt ended: the session's workspaces read afresh.
    Committed(String, Result<Loaded, String>),
    /// The Changes page's Apply found uncommitted changes in this workspace.
    PageCommit(String),
    Discard(String, String),
    Abort(String, String),
    /// A stopped rebase: ask the session's agent to resolve it and finish the rebase.
    Resolve(String, String),
    /// Click elsewhere: a Discard waiting for its second click is not wanted.
    Disarm(String),
    /// An action ended: the session, the workspace, what to say and whether it went wrong.
    Done(String, String, String, bool),
    Diff(String, String),
    DiffLoaded(String, String, Result<String, String>),
}

/// What Resolve sends the agent: the rebase stopped in its own copy, where only it can finish it.
/// `detail` is the daemon's line with the destination and the conflicted files.
fn resolve_prompt(detail: &str) -> String {
    format!(
        "{detail}\n\nThe rebase is stopped in your workspace. Resolve the conflicts keeping what both sides intended, \
         stage the files and run `git rebase --continue` until the rebase is finished. Do not abort it."
    )
}

/// What Apply sends the agent when its workspace holds uncommitted changes: Apply brings commits
/// only, and the message is the agent's to write.
fn commit_prompt(repository: &str) -> String {
    format!(
        "Commit the uncommitted changes in {repository} now, with a commit message of your own that says what they do, so that I can apply them to my checkout. \
         Change nothing else. Every changed or new file must end up committed, ignored or removed: `git status` must be clean afterwards."
    )
}

impl App {
    /// After a turn's end: when an Apply waits for the agent's commit, read the session's
    /// workspaces afresh and decide it ([`Event::Committed`]).
    pub(crate) fn workspaces_turn_ended(&self, session: &str) -> Task<Message> {
        if !self.views.get(session).is_some_and(|v| v.workspaces.committing.is_some()) {
            return Task::none();
        }
        let Some(actions) = self.actions.clone() else { return Task::none() };
        let id = session.to_string();
        blocking(
            move || ws::load(&actions, &id).map_err(|e| ws::message(&e)),
            {
                let id = session.to_string();
                move |loaded| Message::Workspaces(Event::Committed(id, loaded))
            },
        )
    }

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

    /// Read what an Apply would bring of one file of a workspace, for the Changes page's row.
    pub(crate) fn workspace_file_diff(&self, workspace: String, path: sill_proto::sessions::HostPath) -> Task<Message> {
        let key = crate::changes_ui::workspace_key(&workspace, &path);
        let Some(actions) = self.actions.clone() else {
            return Task::done(Message::Changes(crate::changes_ui::Event::DiffLoaded(key, Ok(crate::demo::changes_diff()))));
        };
        blocking(move || ws::diff(&actions, &workspace, Some(&path)).map_err(|e| ws::message(&e)), move |diff| Message::Changes(crate::changes_ui::Event::DiffLoaded(key, diff)))
    }

    /// The Changes page's workspaces, read with its changes.
    pub(crate) fn workspaces_page_load(&self) -> Task<Message> {
        let Some(actions) = self.actions.clone() else { return Task::none() };
        blocking(move || ws::all(&actions).map_err(|e| ws::message(&e)), |all| Message::Workspaces(Event::PageLoaded(all)))
    }

    /// The revision the Changes page shows for a workspace.
    fn page_seen(&self, workspace: &str) -> Option<api::Revision> {
        self.changes_listing.details().iter().find(|d| d.workspace.workspace_id == workspace).map(|d| d.revision.clone())
    }

    pub(crate) fn workspaces_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::PageLoaded(all) => {
                // an open diff of a file that went closes; one whose workspace moved is read again
                let before: BTreeMap<String, api::Revision> =
                    self.changes_listing.details().iter().map(|d| (d.workspace.workspace_id.clone(), d.revision.clone())).collect();
                self.changes_listing.workspaces = Some(all);
                let Some(page) = &mut self.changes else { return Task::none() };
                page.fresh_workspaces = true;
                let details = self.changes_listing.details();
                let listed = |open: &str| {
                    details.iter().find_map(|d| d.files.iter().find(|f| crate::changes_ui::workspace_key(&d.workspace.workspace_id, &f.path) == open).map(|f| (d, f.path.clone())))
                };
                page.open.retain(|k, _| crate::changes_ui::workspace_of(k).is_none() || listed(k).is_some());
                let moved: Vec<(String, sill_proto::sessions::HostPath)> = page
                    .open
                    .keys()
                    .filter_map(|k| listed(k))
                    .filter(|(d, _)| before.get(&d.workspace.workspace_id) != Some(&d.revision))
                    .map(|(d, path)| (d.workspace.workspace_id.clone(), path))
                    .collect();
                Task::batch(moved.into_iter().map(|(id, path)| self.workspace_file_diff(id, path)))
            }
            Event::PageApply(workspace) => {
                // the listing from before the page was opened is shown, not acted on
                if !self.changes.as_ref().is_some_and(|p| p.fresh_workspaces) {
                    return Task::none();
                }
                let (Some(actions), Some(seen)) = (self.actions.clone(), self.page_seen(&workspace)) else { return Task::none() };
                blocking(
                    move || match ws::apply(&actions, &workspace, &seen) {
                        Ok(ApplyOutcome::Applied) => Event::PageDone("Applied to your checkout.".to_string(), false),
                        Ok(ApplyOutcome::Conflicts(paths)) => Event::PageDone(format!("The rebase stopped on {}: resolve it in the session, or abort it there.", paths.join(", ")), true),
                        Ok(ApplyOutcome::Uncommitted) => Event::PageCommit(workspace),
                        Err(e) => Event::PageDone(ws::message(&e), true),
                    },
                    Message::Workspaces,
                )
            }
            Event::PageCommit(workspace) => {
                // the agent commits in its session: asked there when the session is open here
                let session = self.changes_listing.details().iter().find(|d| d.workspace.workspace_id == workspace).map(|d| d.workspace.session_id.clone());
                let open = session.filter(|s| self.views.get(s).is_some_and(|v| !v.conversation.busy() && v.workspaces.loaded.as_ref().is_some_and(|l| l.detail(&workspace).is_some())));
                let said = match &open {
                    Some(_) => "It has uncommitted changes: its session's agent is asked to commit them, and they are applied when its turn ends.",
                    None => "It has uncommitted changes the agent commits first: open its session and apply there.",
                };
                if let Some(page) = &mut self.changes {
                    page.said = Some(said.to_string());
                }
                match open {
                    Some(session) => Task::done(Message::Workspaces(Event::Commit(session, workspace))),
                    None => Task::none(),
                }
            }
            Event::PageDiscard(workspace) => {
                // the listing from before the page was opened is shown, not acted on
                if !self.changes.as_ref().is_some_and(|p| p.fresh_workspaces) {
                    return Task::none();
                }
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
                // the listing from before the page was opened is shown, not acted on
                if !self.changes.as_ref().is_some_and(|p| p.fresh_workspaces) {
                    return Task::none();
                }
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
                        Ok(ApplyOutcome::Applied) => Event::Done(session, workspace, "Applied to your checkout.".to_string(), false),
                        Ok(ApplyOutcome::Conflicts(paths)) => Event::Done(session, workspace, format!("The rebase onto your checkout stopped on {}: Resolve asks the agent, or Abort.", paths.join(", ")), true),
                        Ok(ApplyOutcome::Uncommitted) => Event::Commit(session, workspace),
                        Err(e) => Event::Done(session, workspace, ws::message(&e), true),
                    },
                    Message::Workspaces,
                )
            }
            Event::Commit(session, workspace) => {
                let Some(view) = self.views.get_mut(&session) else { return Task::none() };
                view.workspaces.running = None;
                let Some(d) = view.workspaces.loaded.as_ref().and_then(|l| l.detail(&workspace)) else { return Task::none() };
                if view.conversation.busy() {
                    view.workspaces.notices.insert(workspace, ("The agent is working; apply after its turn.".into(), true));
                    return Task::none();
                }
                let prompt = commit_prompt(&d.workspace.source.display);
                view.workspaces.committing = Some((workspace, d.state_tree.clone()));
                // the conversation folds the prompt and its turn to one line (`folds`)
                self.submit_for(&session, prompt, vec![], Some("commit".into()))
            }
            Event::Committed(session, loaded) => {
                let Some(view) = self.views.get_mut(&session) else { return Task::none() };
                let Some((workspace, seen)) = view.workspaces.committing.take() else { return Task::none() };
                let Ok(loaded) = loaded else {
                    view.workspaces.notices.insert(workspace, ("The workspace could not be read after the agent's turn: look again, then apply.".into(), true));
                    return Task::none();
                };
                let now = loaded.detail(&workspace).map(|d| (d.summary.uncommitted.is_empty(), d.state_tree == seen));
                view.workspaces.loaded = Some(loaded);
                let said = match now {
                    Some((true, true)) => return Task::done(Message::Workspaces(Event::Apply(session, workspace))),
                    Some((false, _)) => "The agent left changes uncommitted, so nothing was applied.",
                    Some((true, false)) => "The work changed while the agent committed it: look again, then apply.",
                    None => return Task::none(),
                };
                view.workspaces.notices.insert(workspace, (said.into(), true));
                Task::none()
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
            Event::Resolve(session, workspace) => {
                let Some(view) = self.views.get(&session) else { return Task::none() };
                if view.workspaces.running.is_some() || view.conversation.busy() {
                    return Task::none();
                }
                let Some(stopped) = view.workspaces.offered().into_iter().find(|d| d.workspace.workspace_id == workspace).and_then(|d| d.workspace.operation.as_ref()).filter(|o| o.state == "conflict") else {
                    return Task::none();
                };
                self.submit(&session, resolve_prompt(&stopped.detail), vec![])
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
                        move || ws::diff(&actions, &workspace, None).map_err(|e| ws::message(&e))
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
