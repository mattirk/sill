//! Session files in the window. What belongs to a session — its pins, folders, open files, the
//! reader — is its own (`SessionView::files`, sill-client's `FilesState`, plus the rendering here).
//! What belongs to the window is whether the drawer is shown and the file worker. A reply only
//! ever changes the session it was asked for; only the session the person is looking at may show
//! the drawer.
use std::collections::VecDeque;

use iced::Task;
use iced::widget::{markdown, text_editor};
use sill_client::Connector;
use sill_client::files::{self, Files, Reply, Request};
use sill_proto::sessions::HostPath;

use crate::{App, Message, SessionView, demo};

#[derive(Debug, Clone)]
pub enum Action {
    ToggleDrawer,
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
    CloseFile(String),
    RefreshFile,
    Source,
    Text(text_editor::Action),
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

/// The open file as the window draws it, rebuilt when the file's text changes.
pub struct ReaderView {
    pub key: String,
    pub revision: u64,
    pub content: text_editor::Content,
    pub markdown: Option<markdown::Content>,
}

pub struct Workspace {
    pub shown: bool,
    /// The tree row under the pointer: its pin button shows, as the shelf's does on hover.
    pub hovered: Option<String>,
    serial: u64,
    /// None in `--demo`: requests are answered with synthetic files at once.
    files: Option<Files>,
}

impl Workspace {
    pub fn new(connect: Option<Connector>) -> (Self, Task<Message>) {
        let Some(connect) = connect else {
            return (
                Self {
                    shown: false,
                    hovered: None,
                    serial: 0,
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
                serial: 0,
                files: Some(files),
            },
            Task::run(stream, |reply| {
                Message::Workspace(Action::Reply(Box::new(reply)))
            }),
        )
    }
}

impl SessionView {
    /// The reader's rendering for the file now open; parsed Markdown only when it is shown.
    pub fn sync_reader(&mut self) {
        let Some(file) = &self.files.reader else {
            self.reader = None;
            return;
        };
        let key = files::key(&file.path);
        if self
            .reader
            .as_ref()
            .is_none_or(|r| r.key != key || r.revision != file.revision)
        {
            self.reader = Some(ReaderView {
                key: key.into(),
                revision: file.revision,
                content: text_editor::Content::with_text(&file.text),
                markdown: None,
            });
        }
        let reader = self.reader.as_mut().unwrap();
        if !file.source
            && file.repo.is_none()
            && file.renderable()
            && reader.markdown.is_none()
            && file.pending.is_none()
            && file.error.is_none()
        {
            reader.markdown = Some(markdown::Content::parse(&file.text));
        }
    }
}

impl App {
    /// Send `requests` for `session`'s files: each recorded as awaited, then to the worker (in the
    /// demo, answered at once).
    fn files_send(&mut self, session: &str, requests: Vec<Request>) {
        let mut queue: VecDeque<Request> = requests.into();
        while let Some(request) = queue.pop_front() {
            self.workspace.serial += 1;
            let serial = self.workspace.serial;
            let Some(view) = self.views.get_mut(session) else {
                return;
            };
            let request = view.files.prepare(serial, request);
            match &self.workspace.files {
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
                    let effects = view.files.accept(reply);
                    view.sync_reader();
                    queue.extend(effects.follow);
                    if effects.reveal && self.selected.as_deref() == Some(session) {
                        self.workspace.shown = true;
                    }
                }
            }
        }
    }

    pub fn files_refresh(&mut self, session: &str) -> Task<Message> {
        let requests = self
            .views
            .get_mut(session)
            .map(|v| v.files.refresh())
            .unwrap_or_default();
        self.files_send(session, requests);
        Task::none()
    }

    pub fn workspace_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Reply(reply) => {
                let session = reply.session.clone();
                let active = self.selected.as_deref() == Some(session.as_str());
                let Some(view) = self.views.get_mut(&session) else {
                    return Task::none();
                };
                let effects = view.files.accept(*reply);
                view.sync_reader();
                // a background session's reply updates its own state, never the window
                if effects.reveal && active {
                    self.workspace.shown = true;
                }
                self.files_send(&session, effects.follow);
                Task::none()
            }
            Action::Hover(key) => {
                self.workspace.hovered = Some(key);
                Task::none()
            }
            Action::Unhover(key) => {
                if self.workspace.hovered.as_deref() == Some(key.as_str()) {
                    self.workspace.hovered = None;
                }
                Task::none()
            }
            Action::ToggleDrawer => {
                // in Drawer mode, putting the drawer away is Full: the same window without it
                // (Ctrl+Shift+F brings it back); elsewhere a quick look over the conversation
                if self.layout.mode == crate::layout::Mode::Drawer {
                    return self.update(Message::Layout(crate::layout::Action::Mode(
                        crate::layout::Mode::Full,
                    )));
                }
                self.workspace.shown = !self.workspace.shown;
                let Some(id) = self.selected.clone() else {
                    return Task::none();
                };
                if self.workspace.shown {
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
            action => {
                let Some(id) = self.selected.clone() else {
                    return Task::none();
                };
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                let mut restore = false;
                let requests: Vec<Request> = match action {
                    Action::Refresh => {
                        view.files.error = None;
                        view.files.refresh()
                    }
                    Action::Collapse => {
                        view.files.expanded.clear();
                        vec![]
                    }
                    Action::Toggle(path) => {
                        // a chip shows the drawer at its folder, without closing it
                        let revealing = !self.workspace.shown;
                        self.workspace.shown = true;
                        if revealing && view.files.expanded.contains(files::key(&path)) {
                            vec![]
                        } else {
                            view.files.toggle(path).into_iter().collect()
                        }
                    }
                    Action::More(path) => view.files.more(path).into_iter().collect(),
                    Action::Open { path, repo } => vec![view.files.open(path, repo)],
                    Action::OpenAndHide { path, repo } => {
                        self.workspace.shown = false;
                        vec![view.files.open(path, repo)]
                    }
                    Action::CloseViewer => {
                        view.files.close_reader();
                        restore = true;
                        vec![]
                    }
                    Action::CloseFile(key) => {
                        let had = view.files.reader.is_some();
                        view.files.close_file(&key);
                        restore = had && view.files.reader.is_none();
                        vec![]
                    }
                    Action::RefreshFile => view.files.reload().into_iter().collect(),
                    Action::Source => {
                        view.files.toggle_source();
                        vec![]
                    }
                    Action::Text(action) => {
                        if !matches!(action, text_editor::Action::Edit(_))
                            && let Some(reader) = &mut view.reader
                        {
                            reader.content.perform(action);
                        }
                        vec![]
                    }
                    Action::Path(value) => {
                        view.files.set_path(value);
                        vec![]
                    }
                    Action::BrowsePath => view.files.typed_path(false).into_iter().collect(),
                    Action::PinPath => view.files.typed_path(true).into_iter().collect(),
                    Action::Pin { path, on } => view.files.pin(path, on).into_iter().collect(),
                    Action::DiffMode(on) => view.files.set_diff_mode(on),
                    Action::DismissError => {
                        view.files.error = None;
                        vec![]
                    }
                    Action::ToggleDrawer
                    | Action::Reply(_)
                    | Action::Hover(_)
                    | Action::Unhover(_) => unreachable!(),
                };
                view.sync_reader();
                self.files_send(&id, requests);
                if restore && self.current().is_some_and(SessionView::conversation_shown) {
                    return self.restore_scroll();
                }
                Task::none()
            }
        }
    }
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
