//! Session files in the window. What belongs to a session — its pins, folders, open files, the
//! reader — is its own (`SessionView::files`, sill-client's `FilesState`, plus the rendering here).
//! What belongs to the window is whether the drawer is shown and the file worker. A reply only
//! ever changes the session it was asked for; only the session the person is looking at may show
//! the drawer.
use std::collections::VecDeque;

use iced::Task;
use iced::widget::text_editor;
use sill_client::Connector;
use sill_client::files::{self, Files, Reply, Request};
use sill_proto::sessions::HostPath;

use crate::{App, Message, SessionView, demo};

mod edits;
mod links;
mod tree;
mod drawer_ui;
mod reader_ui;
mod ui;

pub use edits::ReaderView;
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
        outcome: sill_client::actions::Outcome,
    },
    DiscardEdits,
    CancelDiscard,
    Source,
    Text(text_editor::Action),
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
    follow_seen: std::collections::BTreeMap<String, String>,
    navigation: std::collections::BTreeMap<String, links::Navigation>,
    /// None in `--demo`: requests are answered with synthetic files at once.
    files: Option<Files>,
}

impl Drawer {
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
                    files: None,
                },
                Task::none(),
            );
        };
        let (tx, rx) = async_channel::bounded(32);
        let files = Files::start(connect, move |reply| tx.send_blocking(reply).is_ok());
        let stream = iced::futures::stream::unfold(rx, |rx| async move {
            rx.recv().await.ok().map(|reply| (reply, rx))
        });
        (
            Self {
                shown: false,
                hovered: None,
                pending_discard: None,
                folder_picker: None,
                navigation: Default::default(),
                serial: 0,
                follow_seen: Default::default(),
                files: Some(files),
            },
            Task::run(stream, |reply| {
                Message::Files(Action::Reply(Box::new(reply)))
            }),
        )
    }
}

impl Drawer {
    /// Follow status transitions, so a manual choice lasts until the repository changes.
    fn follow(
        &mut self,
        session: &str,
        files: &mut files::FilesState,
        enabled: bool,
    ) -> Vec<Request> {
        if !enabled {
            self.follow_seen.remove(session);
            return vec![];
        }
        // Iced has no separate scoped folder: follow the first session-pinned repository.
        let repo = files
            .roots
            .iter()
            .filter(|r| !r.global)
            .find_map(|r| files.repo_of(&r.path));
        let Some(repo) = repo.filter(|r| r.read && r.error.is_none()) else {
            self.follow_seen.remove(session);
            return vec![];
        };
        let state = format!(
            "{:?}:{:?}:{:?}:{:?}",
            repo.root, repo.head, repo.entries, repo.lines
        );
        let dirty = !repo.entries.is_empty();
        if self.follow_seen.get(session) == Some(&state) {
            return vec![];
        }
        self.follow_seen.insert(session.into(), state);
        if files.diff_mode != dirty {
            files.set_diff_mode(dirty)
        } else {
            vec![]
        }
    }
}

impl App {
    pub fn sync_drawer_follow(&mut self) {
        let enabled = self
            .daemon_settings
            .as_ref()
            .is_some_and(|s| s.drawer_follow_changes.unwrap_or(false));
        let requests: Vec<_> = self
            .views
            .iter_mut()
            .map(|(id, view)| {
                (
                    id.clone(),
                    self.drawer.follow(id, &mut view.files, enabled),
                )
            })
            .collect();
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
                        view.files
                            .abandon("File connection stopped. Reopen the preview.");
                    }
                }
                None => {
                    let reply = Reply {
                        session: session.into(),
                        serial,
                        result: demo::files(&request),
                        request,
                    };
                    let mut effects = view.files.accept(reply);
                    effects.follow.extend(
                        self.drawer.follow(
                            session,
                            &mut view.files,
                            self.daemon_settings
                                .as_ref()
                                .is_some_and(|s| s.drawer_follow_changes.unwrap_or(false)),
                        ),
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
                v.files.directories.values().all(|d| d.pending.is_none())
                    && v.files
                        .repos
                        .values()
                        .all(|r| r.pending.is_none() && r.counts_pending.is_none())
            })
            .map(|v| v.files.refresh())
            .unwrap_or_default();
        self.files_send(session, requests);
        Task::none()
    }

    pub fn files_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Link {
                session,
                source,
                target,
            } => self.follow_link(session, source, target),
            Action::Back(forward) => self.navigate_back(forward),
            Action::Navigate {
                session,
                path,
                fragment,
                history,
            } => self.navigate(session, path, fragment, history),
            Action::AnchorOffset { session, key, y } => self.anchor_offset(session, key, y),
            Action::SaveFile => self.save_file(),
            Action::Saved {
                session,
                key,
                serial,
                text,
                outcome,
            } => self.file_saved(session, key, serial, text, outcome),
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
            Action::FolderPicked {
                session,
                token,
                path,
            } => self.folder_picked(session, token, path),
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
        effects.follow.extend(
            self.drawer.follow(
                &session,
                &mut view.files,
                self.daemon_settings
                    .as_ref()
                    .is_some_and(|s| s.drawer_follow_changes.unwrap_or(false)),
            ),
        );
        view.sync_reader();
        // a background session's reply updates its own state, never the window
        if effects.reveal && active {
            self.drawer.shown = true;
        }
        self.files_send(&session, effects.follow);
        // pins are binds made at launch: a running jail is restarted to get the change
        // (the agent resumes with its history, as after a permission change)
        if effects.pins_changed && self.tabs.iter().any(|t| t.id == session && t.running) {
            return self.update(Message::Perms(crate::permissions::Event::Restart(
                session,
                crate::permissions::Change::RestartPins,
            )));
        }
        self.reader_anchor(&session)
    }

    fn toggle_drawer(&mut self) -> Task<Message> {
        // in Drawer mode, putting the drawer away is Full: the same window without it
        // (Ctrl+Shift+F brings it back); elsewhere a quick look over the conversation
        if self.layout.mode == crate::layout::Mode::Drawer {
            return self.update(Message::Layout(crate::layout::Action::Mode(
                crate::layout::Mode::Full,
            )));
        }
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
