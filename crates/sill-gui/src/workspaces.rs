//! A session's workspaces in the window (docs/parallel-coding.md §12): what its jail has for each
//! pinned repository, the shelf above the prompt that offers Apply and Discard once the agent's
//! turn ends, and the pin chips' marks. State belongs to the session; the work is the daemon's
//! (`sill_client::workspaces`), read off the GUI thread.
use std::collections::BTreeMap;

use iced::Task;
use sill_client::workspaces::{self as ws, ApplyOutcome, BranchOutcome, How, Loaded};
use sill_proto::workspaces as api;

use crate::armed::Armed;
use crate::{App, Message, blocking};

/// One session's workspaces as the window knows them.
#[derive(Debug, Default)]
pub struct State {
    pub loaded: Option<Loaded>,
    /// The workspace whose Discard was clicked once, and the revision the shelf showed then: the
    /// next click discards when that is still what it shows.
    pub armed: Armed<(String, api::Revision)>,
    /// An Apply onto another branch than the workspace started from, clicked once: the workspace
    /// and the revision the shelf showed then (the checkout's branch is part of it). The next
    /// click applies when that is still what it shows.
    pub apply_armed: Armed<(String, api::Revision)>,
    /// An Apply onto another branch the person confirmed: the workspace and that branch. It
    /// lasts until the Apply ends, so the one after the agent's commit turn does not ask again.
    pub apply_onto: Option<(String, String)>,
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
    /// The workspace whose commit turn has just ended well: the Apply that follows asks for no
    /// second one, whatever the daemon says (2026-11-12, an Apply that asked after every turn).
    pub committed: Option<String>,
    /// The workspace whose Apply menu is open (the caret beside Apply).
    pub menu: Option<String>,
    /// The commit turn being waited for ([`State::committing`]) was asked for by Apply as
    /// branch: the branch, not an Apply, follows it.
    pub branch_after_commit: bool,
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
        (n > 0).then(|| {
            format!("{n} uncommitted file{} in your checkout {} not in this workspace", if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" })
        })
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
    /// Remove a workspace that cannot be read (asks for a second press).
    PageRemove(String),
    PageDone(String, bool),
    Loaded(String, Result<Loaded, String>),
    Apply(String, String),
    /// The caret beside Apply: show or hide the other ways to apply.
    Menu(String, String),
    /// Apply without commit: the workspace's state into the checkout, uncommitted, to try.
    Copy(String, String),
    /// Apply as branch: the workspace's commits as a new branch in the checkout, not checked out.
    Branch(String, String),
    /// An Apply found uncommitted changes: ask the session's agent to commit them; the Apply
    /// follows at its turn's end.
    Commit(String, String),
    /// Apply as branch found uncommitted changes: [`Event::Commit`], then the branch.
    CommitForBranch(String, String),
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
         Change nothing else. Every changed or new file must end up committed, ignored or removed: `git status` must be clean afterwards. After committing, reply with only the short commit message (the subject line). If committing fails, report the failure instead."
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
        blocking(move || ws::load(&actions, &id).map_err(|e| ws::message(&e)), {
            let id = session.to_string();
            move |loaded| Message::Workspaces(Event::Committed(id, loaded))
        })
    }

    /// Read `session`'s workspaces; refreshed with its files (a turn's end, the 15 s poll, opening).
    pub(crate) fn workspaces_load(&self, session: &str) -> Task<Message> {
        let Some(actions) = self.actions.clone() else { return Task::none() };
        let id = session.to_string();
        blocking(move || ws::load(&actions, &id).map_err(|e| ws::message(&e)), {
            let id = session.to_string();
            move |loaded| Message::Workspaces(Event::Loaded(id, loaded))
        })
    }

    /// Read what an Apply would bring of one file of a workspace, for the Changes page's row.
    pub(crate) fn workspace_file_diff(&self, workspace: String, path: sill_proto::sessions::HostPath) -> Task<Message> {
        let key = crate::changes_ui::workspace_key(&workspace, &path);
        let Some(actions) = self.actions.clone() else {
            return Task::done(Message::Changes(crate::changes_ui::Event::DiffLoaded(key, Ok(crate::demo::changes_diff()))));
        };
        blocking(
            move || ws::diff(&actions, &workspace, Some(&path)).map_err(|e| ws::message(&e)),
            move |diff| Message::Changes(crate::changes_ui::Event::DiffLoaded(key, diff)),
        )
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
                    details.iter().find_map(|d| {
                        d.files.iter().find(|f| crate::changes_ui::workspace_key(&d.workspace.workspace_id, &f.path) == open).map(|f| (d, f.path.clone()))
                    })
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
                let Some(detail) = self.changes_listing.details().iter().find(|d| d.workspace.workspace_id == workspace) else { return Task::none() };
                if let Some((from, now)) = ws::other_branch(detail) {
                    let onto = (workspace.clone(), now.to_string());
                    let asked = (workspace.clone(), detail.revision.clone());
                    let Some(page) = &mut self.changes else { return Task::none() };
                    if page.workspace_apply_onto.as_ref() != Some(&onto) {
                        // one Apply is in question at a time
                        page.workspace_apply_onto = None;
                        if !page.workspace_apply_armed.press(asked) {
                            page.said = Some(ws::other_branch_question(from, now));
                            return Task::none();
                        }
                        page.workspace_apply_onto = Some(onto);
                    }
                }
                let (Some(actions), Some(seen)) = (self.actions.clone(), self.page_seen(&workspace)) else { return Task::none() };
                blocking(
                    move || match ws::apply(&actions, &workspace, &seen, How::Commits) {
                        Ok(ApplyOutcome::Applied) => Event::PageDone("Applied to your checkout.".to_string(), false),
                        Ok(ApplyOutcome::Conflicts(paths)) => {
                            Event::PageDone(format!("The rebase stopped on {}: resolve it in the session, or abort it there.", paths.join(", ")), true)
                        }
                        Ok(ApplyOutcome::Uncommitted) => Event::PageCommit(workspace),
                        Err(e) => Event::PageDone(ws::message(&e), true),
                    },
                    Message::Workspaces,
                )
            }
            Event::PageCommit(workspace) => {
                // the agent commits in its session: asked there when the session is open here
                let session = self.changes_listing.details().iter().find(|d| d.workspace.workspace_id == workspace).map(|d| d.workspace.session_id.clone());
                let open = session.filter(|s| {
                    self.views.get(s).is_some_and(|v| !v.conversation.busy() && v.workspaces.loaded.as_ref().is_some_and(|l| l.detail(&workspace).is_some()))
                });
                let said = match &open {
                    Some(_) => "It has uncommitted changes: its session's agent is asked to commit them, and they are applied when its turn ends.",
                    None => "It has uncommitted changes the agent commits first: open its session and apply there.",
                };
                if let Some(page) = &mut self.changes {
                    page.said = Some(said.to_string());
                }
                // the person confirmed another branch here: the session's Apply after the commit needs no second ask
                let confirmed = self
                    .changes
                    .as_mut()
                    .and_then(|p| {
                        p.workspace_apply_armed.clear();
                        p.workspace_apply_onto.take()
                    })
                    .filter(|(w, _)| *w == workspace);
                if let (Some(session), Some(confirmed)) = (&open, confirmed)
                    && let Some(view) = self.views.get_mut(session)
                {
                    view.workspaces.apply_armed.clear();
                    view.workspaces.apply_onto = Some(confirmed);
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
                // the second press confirms what the page showed at the first: a workspace that
                // changed between them is asked about again
                let Some(seen) = self.page_seen(&workspace) else { return Task::none() };
                let Some(page) = &mut self.changes else { return Task::none() };
                if !page.workspace_armed.press((workspace.clone(), seen.clone())) {
                    return Task::none();
                }
                let Some(actions) = self.actions.clone() else { return Task::none() };
                blocking(
                    move || match ws::discard(&actions, &workspace, &seen) {
                        Ok(_) => ("Discarded. Undo below for 7 days.".to_string(), false),
                        Err(e) => (ws::message(&e), true),
                    },
                    |(said, bad)| Message::Workspaces(Event::PageDone(said, bad)),
                )
            }
            Event::PageRemove(workspace) => {
                if !self.changes.as_ref().is_some_and(|p| p.fresh_workspaces) {
                    return Task::none();
                }
                let Some(page) = &mut self.changes else { return Task::none() };
                if !page.workspace_remove_armed.press(workspace.clone()) {
                    return Task::none();
                }
                let Some(actions) = self.actions.clone() else { return Task::none() };
                blocking(
                    move || match ws::remove(&actions, &workspace) {
                        Ok(_) => ("Removed the workspace and its folder.".to_string(), false),
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
                    page.workspace_apply_armed.clear();
                    page.workspace_apply_onto = None;
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
                    loaded
                        .detail(id)
                        .is_some_and(|new| view.workspaces.loaded.as_ref().and_then(|old| old.detail(id)).is_some_and(|old| old.revision == new.revision))
                });
                if view.workspaces.diff.as_ref().is_some_and(|(id, _)| loaded.detail(id).is_none_or(|d| !ws::pending(d))) {
                    view.workspaces.diff = None;
                }
                // each workspace's review, read with it
                let reviews: Vec<String> = loaded.details.iter().map(|d| d.workspace.workspace_id.clone()).collect();
                view.workspaces.loaded = Some(loaded);
                let mut tasks: Vec<Task<Message>> = reviews.into_iter().map(|id| self.crosscheck_load(id)).collect();
                // the drawer reads the agent's copies: what it showed was the person's checkout
                if self.drawer.set_aliases(&session, aliases) {
                    tasks.push(self.files_refresh(&session));
                }
                Task::batch(tasks)
            }
            Event::Disarm(session) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.workspaces.armed.clear();
                    view.workspaces.apply_armed.clear();
                    view.workspaces.apply_onto = None;
                    view.workspaces.menu = None;
                }
                Task::none()
            }
            Event::Apply(session, workspace) => {
                // one commit turn for an Apply: the one that follows it never asks for another
                let after_commit = self.views.get_mut(&session).and_then(|v| v.workspaces.committed.take()).is_some_and(|w| w == workspace);
                if self.ask_other_branch(&session, &workspace) {
                    return Task::none();
                }
                let Some((actions, seen)) = self.workspace_action(&session, &workspace) else { return Task::none() };
                blocking(
                    move || match ws::apply(&actions, &workspace, &seen, How::Commits) {
                        Ok(ApplyOutcome::Applied) => Event::Done(session, workspace, "Applied to your checkout.".to_string(), false),
                        Ok(ApplyOutcome::Conflicts(paths)) => Event::Done(
                            session,
                            workspace,
                            format!("The rebase onto your checkout stopped on {}: Resolve asks the agent, or Abort.", paths.join(", ")),
                            true,
                        ),
                        Ok(ApplyOutcome::Uncommitted) if after_commit => Event::Done(
                            session,
                            workspace,
                            "Sill still finds uncommitted changes after the agent's commit, so nothing was applied: look at the workspace, then apply."
                                .to_string(),
                            true,
                        ),
                        Ok(ApplyOutcome::Uncommitted) => Event::Commit(session, workspace),
                        Err(e) => Event::Done(session, workspace, ws::message(&e), true),
                    },
                    Message::Workspaces,
                )
            }
            Event::Menu(session, workspace) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.workspaces.menu = if view.workspaces.menu.as_deref() == Some(workspace.as_str()) { None } else { Some(workspace) };
                }
                Task::none()
            }
            Event::Copy(session, workspace) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.workspaces.menu = None;
                }
                let Some((actions, seen)) = self.workspace_action(&session, &workspace) else { return Task::none() };
                blocking(
                    move || {
                        match ws::apply(&actions, &workspace, &seen, How::Uncommitted) {
                        Ok(ApplyOutcome::Applied) => Event::Done(session, workspace, "Written into your checkout, uncommitted. The work is still here to apply.".to_string(), false),
                        Ok(ApplyOutcome::Conflicts(paths)) => Event::Done(session, workspace, format!("The rebase onto your checkout stopped on {}: Resolve asks the agent, or Abort.", paths.join(", ")), true),
                        // the checkout moved on: the rebase onto it takes commits
                        Ok(ApplyOutcome::Uncommitted) => {
                            Event::Done(session, workspace, "Your checkout has commits the workspace does not, and the rebase onto them needs its changes committed: ask the agent to commit, or use Apply.".to_string(), true)
                        }
                        Err(e) => Event::Done(session, workspace, ws::message(&e), true),
                    }
                    },
                    Message::Workspaces,
                )
            }
            Event::Branch(session, workspace) => {
                let after_commit = self
                    .views
                    .get_mut(&session)
                    .and_then(|v| {
                        v.workspaces.menu = None;
                        v.workspaces.committed.take()
                    })
                    .is_some_and(|w| w == workspace);
                let Some((actions, seen)) = self.workspace_action(&session, &workspace) else { return Task::none() };
                blocking(
                    move || match ws::branch(&actions, &workspace, &seen) {
                        Ok(BranchOutcome::Made(name)) => Event::Done(
                            session,
                            workspace,
                            format!("Branch {name} is in your checkout, not checked out; your checkout is unchanged. The work is still here to apply."),
                            false,
                        ),
                        Ok(BranchOutcome::Uncommitted) if after_commit => Event::Done(
                            session,
                            workspace,
                            "Sill still finds uncommitted changes after the agent's commit, so no branch was made: look at the workspace, then try again."
                                .to_string(),
                            true,
                        ),
                        Ok(BranchOutcome::Uncommitted) => Event::CommitForBranch(session, workspace),
                        Err(e) => Event::Done(session, workspace, ws::message(&e), true),
                    },
                    Message::Workspaces,
                )
            }
            Event::CommitForBranch(session, workspace) => {
                let task = self.workspaces_event(Event::Commit(session.clone(), workspace.clone()));
                // the branch follows only the commit turn asked for here, if one was
                if let Some(view) = self.views.get_mut(&session)
                    && view.workspaces.committing.as_ref().is_some_and(|(w, _)| *w == workspace)
                {
                    view.workspaces.branch_after_commit = true;
                }
                task
            }
            Event::Commit(session, workspace) => {
                let Some(view) = self.views.get_mut(&session) else { return Task::none() };
                view.workspaces.running = None;
                // an Apply follows unless Apply as branch asked ([`Event::CommitForBranch`])
                view.workspaces.branch_after_commit = false;
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
                // what follows this turn, decided once: a failed or changed turn leaves nothing behind
                let then_branch = std::mem::take(&mut view.workspaces.branch_after_commit);
                let Ok(loaded) = loaded else {
                    view.workspaces.notices.insert(workspace, ("The workspace could not be read after the agent's turn: look again, then apply.".into(), true));
                    return Task::none();
                };
                let now = loaded.detail(&workspace).map(|d| (d.summary.uncommitted.is_empty(), d.state_tree == seen));
                view.workspaces.loaded = Some(loaded);
                let said = match now {
                    Some((true, true)) => {
                        view.workspaces.committed = Some(workspace.clone());
                        if then_branch {
                            return Task::done(Message::Workspaces(Event::Branch(session, workspace)));
                        }
                        return Task::done(Message::Workspaces(Event::Apply(session, workspace)));
                    }
                    Some((false, _)) => "The agent left changes uncommitted, so nothing was applied.",
                    Some((true, false)) => "The work changed while the agent committed it: look again, then apply.",
                    None => return Task::none(),
                };
                view.workspaces.notices.insert(workspace, (said.into(), true));
                Task::none()
            }
            Event::Discard(session, workspace) => {
                let Some(view) = self.views.get_mut(&session) else { return Task::none() };
                // the second click confirms what the shelf showed at the first: a workspace that
                // changed between them is asked about again
                let Some(seen) = view.workspaces.loaded.as_ref().and_then(|l| l.detail(&workspace)).map(|d| d.revision.clone()) else { return Task::none() };
                if !view.workspaces.armed.press((workspace.clone(), seen)) {
                    return Task::none();
                }
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
                let Some(stopped) = view
                    .workspaces
                    .offered()
                    .into_iter()
                    .find(|d| d.workspace.workspace_id == workspace)
                    .and_then(|d| d.workspace.operation.as_ref())
                    .filter(|o| o.state == "conflict")
                else {
                    return Task::none();
                };
                self.submit(&session, resolve_prompt(&stopped.detail), vec![])
            }
            Event::Done(session, workspace, said, bad) => {
                if let Some(view) = self.views.get_mut(&session) {
                    view.workspaces.running = None;
                    if view.workspaces.apply_armed.get().is_some_and(|(w, _)| *w == workspace) {
                        view.workspaces.apply_armed.clear();
                    }
                    if view.workspaces.apply_onto.as_ref().is_some_and(|(w, _)| *w == workspace) {
                        view.workspaces.apply_onto = None;
                    }
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

    /// Whether an Apply of `workspace` must first ask: the checkout is on another branch than the
    /// workspace started from and the person has not clicked again for that branch. Asking arms
    /// the second click and says why.
    fn ask_other_branch(&mut self, session: &str, workspace: &str) -> bool {
        let Some(view) = self.views.get_mut(session) else { return false };
        let state = &mut view.workspaces;
        if state.running.is_some() {
            return false;
        }
        let Some(detail) = state.loaded.as_ref().and_then(|l| l.detail(workspace)) else { return false };
        let Some((from, now)) = ws::other_branch(detail) else { return false };
        let onto = (workspace.to_string(), now.to_string());
        if state.apply_onto.as_ref() == Some(&onto) {
            return false;
        }
        // one Apply is in question at a time
        state.apply_onto = None;
        if state.apply_armed.press((workspace.to_string(), detail.revision.clone())) {
            state.apply_onto = Some(onto);
            return false;
        }
        let question = ws::other_branch_question(from, now);
        state.notices.insert(workspace.to_string(), (question, true));
        true
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
