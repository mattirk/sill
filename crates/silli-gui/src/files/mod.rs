//! Session files in the window. What belongs to a session — its pins, folders, open files, the
//! reader — is its own (`SessionView::files`, silli-client's `FilesState`, plus the rendering here).
//! What belongs to the window is whether the drawer is shown and the file worker. A reply only
//! ever changes the session it was asked for; only the session the person is looking at may show
//! the drawer.
use std::collections::VecDeque;

use iced::Task;
use iced::widget::text_editor;
use silli_client::Connector;
use silli_client::files::{self, Files, Reply, Request};
use silli_proto::sessions::HostPath;

use crate::{App, Message, SessionView, demo};

mod drawer_ui;
mod edits;
mod links;
mod reader_ui;
mod tree;
mod ui;

pub use edits::ReaderView;
pub(crate) use edits::reveal_caret;
#[cfg(test)]
pub(crate) use edits::source_columns;

#[derive(Debug, Clone)]
pub enum Action {
    ToggleDrawer,
    Link {
        session: String,
        source: HostPath,
        target: String,
    },
    Navigate {
        session: String,
        path: HostPath,
        fragment: String,
        history: bool,
    },
    Back(bool),
    AnchorOffset {
        session: String,
        key: String,
        y: f32,
    },
    PickFolder,
    FolderPicked {
        session: String,
        token: String,
        path: Option<std::path::PathBuf>,
    },
    /// A Mac folder made to appear in the window's own VM, ready to pin there (`local_vm`).
    MacFolderShared {
        session: String,
        token: String,
        result: Result<std::path::PathBuf, String>,
    },
    Refresh,
    Collapse,
    Toggle(HostPath),
    More(HostPath),
    Open {
        path: HostPath,
        repo: Option<HostPath>,
    },
    /// Open from the narrow drawer: the reader takes the content area.
    OpenAndHide {
        path: HostPath,
        repo: Option<HostPath>,
    },
    CloseViewer,
    RefreshFile,
    SaveFile,
    Saved {
        session: String,
        key: String,
        serial: u64,
        text: String,
        outcome: silli_client::actions::Outcome,
    },
    DiscardEdits,
    CancelDiscard,
    Source,
    Text(text_editor::Action),
    ScrollSourceTo(f32),
    History(bool),
    Path(String),
    BrowsePath,
    PinPath,
    Pin {
        path: HostPath,
        on: bool,
    },
    DiffMode(bool),
    DismissError,
    /// The pointer came over a tree row (its key), or left it.
    Hover(String),
    Unhover(String),
    Reply(Box<Reply>),
}

pub struct Drawer {
    pub shown: bool,
    /// The tree row under the pointer: its pin button shows on hover.
    pub hovered: Option<String>,
    pub pending_discard: Option<(String, Box<Message>)>,
    /// The originating session and view identity of the one native dialog in flight.
    pub folder_picker: Option<(String, String)>,
    serial: u64,
    /// Each session's followed repository as last seen, and whether it had changes.
    follow_seen: std::collections::BTreeMap<String, (String, bool)>,
    /// The session whose changes opened the drawer: it is put away again when they are gone,
    /// unless the person has shown or hidden it meanwhile.
    follow_opened: Option<String>,
    navigation: std::collections::BTreeMap<String, links::Navigation>,
    /// None in `--demo`: requests are answered with synthetic files at once.
    files: Option<Files>,
}

impl Drawer {
    /// A workspace session's repositories for the file worker to read in their workspaces
    /// (docs/parallel-coding.md §12); true when that changed, so the drawer reads again.
    pub(crate) fn set_aliases(&self, session: &str, pairs: Vec<silli_client::files::AliasPair>) -> bool {
        self.files.as_ref().is_some_and(|f| f.set_aliases(session, pairs))
    }

    /// The path a session's file is read from and saved to: its workspace copy in a workspace
    /// session, the path itself otherwise.
    pub(crate) fn workspace_path(&self, session: &str, path: &silli_proto::sessions::HostPath) -> silli_proto::sessions::HostPath {
        self.files.as_ref().map_or_else(|| path.clone(), |f| f.workspace_path(session, path))
    }

    pub fn new(connect: Option<Connector>) -> (Self, Task<Message>) {
        let Some(connect) = connect else {
            return (
                Self {
                    shown: false,
                    hovered: None,
                    pending_discard: None,
                    folder_picker: None,
                    navigation: Default::default(),
                    serial: 0,
                    follow_seen: Default::default(),
                    follow_opened: None,
                    files: None,
                },
                Task::none(),
            );
        };
        let (tx, rx) = async_channel::bounded(32);
        let files = Files::start(connect, move |reply| tx.send_blocking(reply).is_ok());
        let stream = iced::futures::stream::unfold(rx, |rx| async move { rx.recv().await.ok().map(|reply| (reply, rx)) });
        (
            Self {
                shown: false,
                hovered: None,
                pending_discard: None,
                folder_picker: None,
                navigation: Default::default(),
                serial: 0,
                follow_seen: Default::default(),
                follow_opened: None,
                files: Some(files),
            },
            Task::run(stream, |reply| Message::Files(Action::Reply(Box::new(reply)))),
        )
    }
}

impl Drawer {
    /// Follow status transitions, so a manual choice lasts until the repository changes. Also
    /// the turn the repository took since it was last seen: `Some(true)` when changes appeared
    /// in a clean one, `Some(false)` when the last of them went.
    fn follow(&mut self, session: &str, files: &mut files::FilesState, enabled: bool) -> (Vec<Request>, Option<bool>) {
        if !enabled {
            self.follow_seen.remove(session);
            return (vec![], None);
        }
        // Iced has no separate scoped folder: follow the first session-pinned repository.
        let repo = files.roots.iter().filter(|r| !r.global).find_map(|r| files.repo_of(&r.path));
        let Some(repo) = repo.filter(|r| r.read && r.error.is_none()) else {
            self.follow_seen.remove(session);
            return (vec![], None);
        };
        let state = format!("{:?}:{:?}:{:?}:{:?}", repo.root, repo.head, repo.entries, repo.lines);
        let dirty = !repo.entries.is_empty();
        if self.follow_seen.get(session).map(|(seen, _)| seen) == Some(&state) {
            return (vec![], None);
        }
        let was = self.follow_seen.insert(session.into(), (state, dirty));
        // a repository first seen with changes is not one they appeared in
        let turn = was.filter(|(_, was)| *was != dirty).map(|_| dirty);
        let requests = if files.diff_mode != dirty { files.set_diff_mode(dirty) } else { vec![] };
        (requests, turn)
    }

    /// The person showed or hid the drawer themselves: it stays as they left it.
    pub fn chosen(&mut self) {
        self.follow_opened = None;
    }
}

impl App {
    pub fn sync_drawer_follow(&mut self) {
        let enabled = self.daemon_settings.as_ref().is_some_and(|s| s.drawer_follow_changes.unwrap_or(false));
        if !enabled {
            self.drawer.follow_opened = None;
        }
        // the setting or the snapshot changed, not a repository: no turn to act on
        let requests: Vec<_> = self.views.iter_mut().map(|(id, view)| (id.clone(), self.drawer.follow(id, &mut view.files, enabled).0)).collect();
        for (id, requests) in requests {
            self.files_send(&id, requests);
        }
    }

    /// Send `requests` for `session`'s files: each recorded as awaited, then to the worker (in the
    /// demo, answered at once).
    fn files_send(&mut self, session: &str, requests: Vec<Request>) {
        let mut queue: VecDeque<Request> = requests.into();
        while let Some(request) = queue.pop_front() {
            self.drawer.serial += 1;
            let serial = self.drawer.serial;
            let Some(view) = self.views.get_mut(session) else {
                return;
            };
            let request = view.files.prepare(serial, request);
            match &self.drawer.files {
                Some(worker) => {
                    if !worker.send(session, serial, request) {
                        view.files.abandon("File connection stopped. Reopen the preview.");
                    }
                }
                None => {
                    let reply = Reply { session: session.into(), serial, result: demo::files(&request), request };
                    let mut effects = view.files.accept(reply);
                    // the demo's repository never changes: no turn to act on
                    effects.follow.extend(
                        self.drawer.follow(session, &mut view.files, self.daemon_settings.as_ref().is_some_and(|s| s.drawer_follow_changes.unwrap_or(false))).0,
                    );
                    view.sync_reader();
                    queue.extend(effects.follow);
                    if effects.reveal && self.selected.as_deref() == Some(session) {
                        self.drawer.shown = true;
                    }
                }
            }
        }
    }

    pub fn files_refresh(&mut self, session: &str) -> Task<Message> {
        let requests = self
            .views
            .get_mut(session)
            .filter(|v| {
                // Do not supersede slow replies or pile up work on a remote daemon.
                v.files.directories.values().all(|d| d.pending.is_none()) && v.files.repos.values().all(|r| r.pending.is_none())
            })
            .map(|v| v.files.refresh())
            .unwrap_or_default();
        self.files_send(session, requests);
        // its workspaces with it: what the shelf offers, the chips' marks, what the drawer reads
        self.workspaces_load(session)
    }

    pub fn files_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Link { session, source, target } => self.follow_link(session, source, target),
            Action::Back(forward) => self.navigate_back(forward),
            Action::Navigate { session, path, fragment, history } => self.navigate(session, path, fragment, history),
            Action::AnchorOffset { session, key, y } => self.anchor_offset(session, key, y),
            Action::SaveFile => self.save_file(),
            Action::Saved { session, key, serial, text, outcome } => self.file_saved(session, key, serial, text, outcome),
            Action::CancelDiscard => self.cancel_discard(),
            Action::DiscardEdits => self.discard_edits(),
            Action::Reply(reply) => self.files_reply(*reply),
            Action::Hover(key) => {
                self.drawer.hovered = Some(key);
                Task::none()
            }
            Action::Unhover(key) => {
                if self.drawer.hovered.as_deref() == Some(key.as_str()) {
                    self.drawer.hovered = None;
                }
                Task::none()
            }
            Action::PickFolder => self.pick_folder(),
            Action::FolderPicked { session, token, path } => self.folder_picked(session, token, path),
            Action::MacFolderShared { session, token, result } => self.mac_folder_shared(session, token, result),
            Action::ToggleDrawer => self.toggle_drawer(),
            action => self.selected_files_action(action),
        }
    }

    fn files_reply(&mut self, reply: Reply) -> Task<Message> {
        let session = reply.session.clone();
        let active = self.selected.as_deref() == Some(session.as_str());
        let Some(view) = self.views.get_mut(&session) else {
            return Task::none();
        };
        let mut effects = view.files.accept(reply);
        let (follow, turn) =
            self.drawer.follow(&session, &mut view.files, self.daemon_settings.as_ref().is_some_and(|s| s.drawer_follow_changes.unwrap_or(false)));
        effects.follow.extend(follow);
        view.sync_reader();
        // a background session's reply updates its own state, never the window
        if effects.reveal && active {
            self.drawer.shown = true;
        }
        self.files_send(&session, effects.follow);
        // a pin change rebuilds a running jail in the daemon (docs/pins.md): nothing to do here,
        // the session's notice says when it is done
        Task::batch([self.follow_drawer(&session, turn), self.reader_anchor(&session)])
    }

    /// "Drawer follows changes": the drawer comes out when changes appear in the shown session's
    /// repository, from Full too, and goes back where it came from when they are gone. A drawer
    /// already out stays; the Shelf is left alone (its window would have to grow); a background
    /// session never moves the window.
    fn follow_drawer(&mut self, session: &str, turn: Option<bool>) -> Task<Message> {
        use crate::layout::Mode;
        let Some(dirty) = turn else {
            return Task::none();
        };
        let shown_session = self.selected.as_deref() == Some(session);
        if dirty {
            if !shown_session || self.drawer.shown || self.backend_shown() {
                return Task::none();
            }
            if self.layout.plain {
                self.drawer.shown = true;
                self.drawer.follow_opened = Some(session.into());
                return self.files_refresh(session);
            }
            if self.layout.mode != Mode::Full {
                return Task::none();
            }
            self.drawer.follow_opened = Some(session.into());
            return self.set_mode(Mode::Drawer);
        }
        self.follow_return(session)
    }

    /// Put the drawer back where `session`'s changes found it, once they are gone. While another
    /// session is shown the return waits: it is made when the person comes back to this one
    /// (`choose`), unless they have shown or hidden the drawer themselves by then.
    pub(crate) fn follow_return(&mut self, session: &str) -> Task<Message> {
        use crate::layout::Mode;
        if self.drawer.follow_opened.as_deref() != Some(session)
            || self.selected.as_deref() != Some(session)
            || self.drawer.follow_seen.get(session).is_some_and(|(_, dirty)| *dirty)
        {
            return Task::none();
        }
        self.drawer.follow_opened = None;
        if !self.drawer.shown {
            return Task::none();
        }
        if self.layout.plain {
            self.drawer.shown = false;
            if let Some(view) = self.views.get_mut(session) {
                view.files.inspect_pending = None;
            }
            return self.restore_scroll();
        }
        if self.layout.mode == Mode::Drawer {
            return self.set_mode(Mode::Full);
        }
        Task::none()
    }

    fn toggle_drawer(&mut self) -> Task<Message> {
        // in Drawer mode, putting the drawer away is Full: the same window without it
        // (Ctrl+Shift+F brings it back); elsewhere a quick look over the conversation
        if self.layout.mode == crate::layout::Mode::Drawer && !self.layout.plain {
            return self.update(Message::Layout(crate::layout::Action::Mode(crate::layout::Mode::Full)));
        }
        self.drawer.chosen();
        self.drawer.shown = !self.drawer.shown;
        let Some(id) = self.selected.clone() else {
            return Task::none();
        };
        if self.drawer.shown {
            return self.files_refresh(&id);
        }
        // closing the drawer cancels a folder reveal still on its way
        if let Some(view) = self.views.get_mut(&id) {
            view.files.inspect_pending = None;
        }
        if self.current().is_some_and(SessionView::conversation_shown) {
            return self.restore_scroll();
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests;
