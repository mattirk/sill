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

/// The open file as the window draws it, rebuilt when the file's text changes.
pub struct ReaderView {
    pub key: String,
    pub revision: u64,
    pub content: crate::editor::Editor,
    pub source_columns: usize,
    pub markdown: Option<markdown::Content>,
    pub saved: String,
    pub dirty: bool,
    pub saving: Option<u64>,
    pub notice: Option<String>,
}

#[derive(Default)]
struct Navigation {
    back: Vec<HostPath>,
    forward: Vec<HostPath>,
    anchor: Option<(String, String)>,
}

pub struct Workspace {
    pub shown: bool,
    /// The tree row under the pointer: its pin button shows on hover.
    pub hovered: Option<String>,
    pub pending_discard: Option<(String, Box<Message>)>,
    /// The originating session and view identity of the one native dialog in flight.
    pub folder_picker: Option<(String, String)>,
    serial: u64,
    follow_seen: std::collections::BTreeMap<String, String>,
    navigation: std::collections::BTreeMap<String, Navigation>,
    /// None in `--demo`: requests are answered with synthetic files at once.
    files: Option<Files>,
}

/// Source Code Pro uses fixed-width cells; tabs advance to eight-column stops.
pub(crate) fn source_columns(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    text.lines()
        .map(|line| {
            line.split('\t')
                .enumerate()
                .fold(0, |columns, (index, part)| {
                    let columns = if index == 0 {
                        columns
                    } else {
                        (columns / 8 + 1) * 8
                    };
                    columns + part.width()
                })
        })
        .max()
        .unwrap_or(0)
}

/// Keep the caret visible inside the source's horizontal viewport during keyboard editing.
struct RevealSourceCaret {
    id: iced::advanced::widget::Id,
    x: f32,
}

impl iced::advanced::widget::operation::Operation for RevealSourceCaret {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::operation::Operation),
    ) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        bounds: iced::Rectangle,
        _content: iced::Rectangle,
        translation: iced::Vector,
        state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id != Some(&self.id) {
            return;
        }
        let margin = 12.0;
        let offset = if self.x < translation.x + margin {
            self.x - margin
        } else if self.x > translation.x + bounds.width - margin {
            self.x - bounds.width + margin
        } else {
            return;
        };
        state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
            x: Some(offset.max(0.0)),
            y: None,
        });
    }
}

impl Workspace {
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
                content: crate::editor::Editor::with_text(&file.text),
                source_columns: source_columns(&file.text),
                markdown: None,
                saved: file.text.clone(),
                dirty: false,
                saving: None,
                notice: None,
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
            reader.markdown = Some(markdown::Content::parse(&reader.content.text()));
        }
    }
}

impl Workspace {
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
    /// Unsaved buffers survive navigation; ask only before an action can discard them.
    pub fn guard_file_edits(&mut self, message: &Message) -> bool {
        let target = match message {
            Message::Quit
            | Message::CloseRequested
            | Message::Control(crate::control::Command::Quit) => self
                .views
                .iter()
                .find(|(_, v)| {
                    v.reader
                        .as_ref()
                        .is_some_and(|r| r.dirty || r.saving.is_some())
                })
                .map(|(id, _)| id.clone()),
            Message::CloseTab(id) => Some(id.clone()),
            Message::Workspace(Action::Navigate { session, path, .. })
                if self
                    .views
                    .get(session)
                    .and_then(|v| v.files.reader.as_ref())
                    .is_none_or(|f| files::key(&f.path) != files::key(path)) =>
            {
                Some(session.clone())
            }
            Message::Workspace(Action::Back(_)) => self.selected.clone(),
            Message::Workspace(
                Action::Open { .. }
                | Action::OpenAndHide { .. }
                | Action::CloseViewer
                | Action::RefreshFile
                | Action::BrowsePath
                | Action::PinPath,
            ) => self.selected.clone(),
            Message::Workspace(Action::Reply(reply))
                if matches!(reply.request, Request::Inspect(_)) =>
            {
                Some(reply.session.clone())
            }
            _ => None,
        };
        let Some(id) = target else {
            return false;
        };
        let Some(reader) = self.views.get_mut(&id).and_then(|v| v.reader.as_mut()) else {
            return false;
        };
        if !reader.dirty && reader.saving.is_none() {
            return false;
        }
        reader.notice = Some(
            if reader.saving.is_some() {
                "Saving… wait before closing or replacing this file."
            } else {
                "Unsaved changes. Save before continuing, or discard them."
            }
            .into(),
        );
        if reader.saving.is_none() {
            self.workspace.pending_discard = Some((id.clone(), Box::new(message.clone())));
        }
        if !matches!(message, Message::Workspace(Action::Reply(_))) {
            self.selected = Some(id);
            self.changes = None;
            self.settings_page = None;
            self.agents_page.shown = false;
        }
        true
    }

    fn save_file(&mut self) -> Task<Message> {
        let Some(id) = self.selected.clone() else {
            return Task::none();
        };
        let Some(view) = self.views.get_mut(&id) else {
            return Task::none();
        };
        let (Some(file), Some(reader)) = (&view.files.reader, &mut view.reader) else {
            return Task::none();
        };
        if file.repo.is_some()
            || file.pending.is_some()
            || file.error.is_some()
            || !reader.dirty
            || reader.saving.is_some()
        {
            return Task::none();
        }
        reader.content.break_group();
        let text = reader.content.text();
        if text.len() > sill_proto::admin::FILE_CHUNK_BYTES {
            reader.notice =
                Some("Save supports files up to 1 MiB; your edits are still here.".into());
            return Task::none();
        }
        let Some(actions) = self.actions.clone() else {
            reader.notice = Some("The demo cannot save files; your edits are still here.".into());
            return Task::none();
        };
        self.workspace.serial += 1;
        let serial = self.workspace.serial;
        reader.saving = Some(serial);
        reader.notice = None;
        let key = reader.key.clone();
        let params = serde_json::to_value(sill_proto::admin::FileWrite {
            path: file.path.clone(),
            bytes_base64: sill_proto::admin::encode_bytes(text.as_bytes()),
        })
        .unwrap();
        crate::blocking(
            move || (text, actions.call("_sill/files.write", params)),
            move |(text, outcome)| {
                Message::Workspace(Action::Saved {
                    session: id.clone(),
                    key: key.clone(),
                    serial,
                    text,
                    outcome,
                })
            },
        )
    }

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
                    self.workspace.follow(id, &mut view.files, enabled),
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
                    let mut effects = view.files.accept(reply);
                    effects.follow.extend(
                        self.workspace.follow(
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

    pub fn workspace_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Link {
                session,
                source,
                target,
            } => {
                if !self
                    .views
                    .get(&session)
                    .and_then(|v| v.files.reader.as_ref())
                    .is_some_and(|f| files::key(&f.path) == files::key(&source))
                {
                    return Task::none();
                }
                match resolve_link(&source, &target) {
                    Ok(LinkTarget::Web(url)) => {
                        crate::terminal::open_link(url);
                        Task::none()
                    }
                    Ok(LinkTarget::File(path, fragment)) => {
                        self.update(Message::Workspace(Action::Navigate {
                            session,
                            path,
                            fragment,
                            history: true,
                        }))
                    }
                    Err(error) => {
                        if let Some(view) = self.views.get_mut(&session) {
                            if let Some(reader) = &mut view.reader {
                                reader.notice = Some(error);
                            }
                        }
                        Task::none()
                    }
                }
            }
            Action::Back(forward) => {
                let Some(session) = self.selected.clone() else {
                    return Task::none();
                };
                let nav = self
                    .workspace
                    .navigation
                    .entry(session.clone())
                    .or_default();
                let path = if forward {
                    nav.forward.pop()
                } else {
                    nav.back.pop()
                };
                let Some(path) = path else {
                    return Task::none();
                };
                if let Some(file) = self
                    .views
                    .get(&session)
                    .and_then(|v| v.files.reader.as_ref())
                {
                    if forward {
                        nav.back.push(file.path.clone());
                    } else {
                        nav.forward.push(file.path.clone());
                    }
                }
                self.workspace_action(Action::Navigate {
                    session,
                    path,
                    fragment: String::new(),
                    history: false,
                })
            }
            Action::Navigate {
                session,
                path,
                fragment,
                history,
            } => {
                let Some(view) = self.views.get_mut(&session) else {
                    return Task::none();
                };
                let same = view
                    .files
                    .reader
                    .as_ref()
                    .is_some_and(|f| files::key(&f.path) == files::key(&path));
                let nav = self
                    .workspace
                    .navigation
                    .entry(session.clone())
                    .or_default();
                nav.anchor = Some((files::key(&path).into(), fragment));
                if !same {
                    if history {
                        if let Some(file) = &view.files.reader {
                            nav.back.push(file.path.clone());
                        }
                        nav.forward.clear();
                    }
                    let request = view.files.open(path, None);
                    view.sync_reader();
                    self.files_send(&session, vec![request]);
                    self.reader_anchor(&session)
                } else {
                    self.reader_anchor(&session)
                }
            }
            Action::AnchorOffset { session, key, y } => {
                if self.selected.as_deref() == Some(&session)
                    && self
                        .views
                        .get(&session)
                        .and_then(|v| v.reader.as_ref())
                        .is_some_and(|r| r.key == key)
                {
                    iced::widget::operation::scroll_to(
                        "markdown-reader",
                        iced::widget::scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(y),
                        },
                    )
                } else {
                    Task::none()
                }
            }
            Action::SaveFile => self.save_file(),
            Action::Saved {
                session,
                key,
                serial,
                text,
                outcome,
            } => {
                let Some(view) = self.views.get_mut(&session) else {
                    return Task::none();
                };
                let Some(reader) = view
                    .reader
                    .as_mut()
                    .filter(|r| r.key == key && r.saving == Some(serial))
                else {
                    return Task::none();
                };
                reader.saving = None;
                if outcome.is_done() {
                    reader.content.break_group();
                    reader.saved = text.clone();
                    reader.dirty = reader.content.text() != text;
                    reader.markdown = None;
                    reader.notice = Some("Saved".into());
                    if let Some(file) = &mut view.files.reader {
                        file.text = text;
                    }
                    if self
                        .workspace
                        .pending_discard
                        .as_ref()
                        .is_some_and(|(id, _)| id == &session)
                    {
                        self.workspace.pending_discard = None;
                    }
                    return self.files_refresh(&session);
                }
                reader.notice = Some(match outcome {
                    sill_client::actions::Outcome::Uncertain(why) => format!(
                        "Save outcome unknown: {why}. Edits kept; check the file before retrying."
                    ),
                    _ => format!(
                        "Save failed: {}. Edits kept.",
                        outcome.problem().unwrap_or("unknown error")
                    ),
                });
                Task::none()
            }
            Action::CancelDiscard => {
                self.workspace.pending_discard = None;
                if let Some(reader) = self.current_mut().and_then(|v| v.reader.as_mut()) {
                    reader.notice = None;
                }
                Task::none()
            }
            Action::DiscardEdits => {
                let Some((id, next)) = self.workspace.pending_discard.take() else {
                    return Task::none();
                };
                if let Some(reader) = self.views.get_mut(&id).and_then(|v| v.reader.as_mut()) {
                    if reader.saving.is_some() {
                        return Task::none();
                    }
                    reader.content = crate::editor::Editor::with_text(&reader.saved);
                    reader.source_columns = source_columns(&reader.saved);
                    reader.dirty = false;
                    reader.notice = None;
                    reader.markdown = None;
                }
                self.update(*next)
            }
            Action::Reply(reply) => {
                let session = reply.session.clone();
                let active = self.selected.as_deref() == Some(session.as_str());
                let Some(view) = self.views.get_mut(&session) else {
                    return Task::none();
                };
                let mut effects = view.files.accept(*reply);
                effects.follow.extend(
                    self.workspace.follow(
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
                    self.workspace.shown = true;
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
            Action::PickFolder => {
                if self.workspace.folder_picker.is_some() {
                    return Task::none();
                }
                let Some(session) = self.selected.clone() else {
                    return Task::none();
                };
                let Some(view) = self.views.get(&session) else {
                    return Task::none();
                };
                if view.files.pin_pending.is_some() {
                    return Task::none();
                }
                let token = view.media_token.clone();
                self.workspace.folder_picker = Some((session.clone(), token.clone()));
                Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .set_title("Pin a folder")
                            .pick_folder()
                            .await
                            .map(|folder| folder.path().to_path_buf())
                    },
                    move |path| {
                        Message::Workspace(Action::FolderPicked {
                            session,
                            token,
                            path,
                        })
                    },
                )
            }
            Action::FolderPicked {
                session,
                token,
                path,
            } => {
                if self.workspace.folder_picker.as_ref() != Some(&(session.clone(), token.clone()))
                {
                    return Task::none();
                }
                self.workspace.folder_picker = None;
                let Some(view) = self
                    .views
                    .get_mut(&session)
                    .filter(|v| v.media_token == token)
                else {
                    return Task::none();
                };
                let requests = path
                    .and_then(|path| view.files.pin(HostPath::from_path(&path), true))
                    .into_iter()
                    .collect();
                self.files_send(&session, requests);
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
                    Action::RefreshFile => view.files.reload().into_iter().collect(),
                    Action::Source => {
                        view.files.toggle_source();
                        vec![]
                    }
                    action @ (Action::Text(_) | Action::History(_)) => {
                        let editing = matches!(
                            action,
                            Action::Text(text_editor::Action::Edit(_)) | Action::History(_)
                        );
                        let reveal = matches!(
                            action,
                            Action::Text(
                                text_editor::Action::Edit(_)
                                    | text_editor::Action::Move(_)
                                    | text_editor::Action::Select(_)
                            ) | Action::History(_)
                        );
                        let editable = view.files.reader.as_ref().is_some_and(|f| {
                            f.source && f.repo.is_none() && f.pending.is_none() && f.error.is_none()
                        });
                        if (!editing || editable)
                            && let Some(reader) = &mut view.reader
                        {
                            match action {
                                Action::Text(action) => reader.content.perform(action),
                                Action::History(redo) => reader.content.history(redo),
                                _ => unreachable!(),
                            }
                            if editing {
                                let text = reader.content.text();
                                reader.source_columns = source_columns(&text);
                                reader.dirty = text != reader.saved;
                                reader.markdown = None;
                                reader.notice = None;
                            }
                            if reveal {
                                let cursor = reader.content.cursor().position;
                                let columns = reader.content.line(cursor.line).map_or(0, |line| {
                                    source_columns(
                                        line.text.get(..cursor.column).unwrap_or(&line.text),
                                    )
                                });
                                let digits = reader.content.line_count().max(1).ilog10() + 1;
                                return iced_runtime::task::effect(iced_runtime::Action::widget(
                                    RevealSourceCaret {
                                        id: iced::advanced::widget::Id::from(format!(
                                            "source-horizontal-{}",
                                            reader.key
                                        )),
                                        x: 12.0
                                            + (columns as f32 + digits.max(2) as f32 + 2.0)
                                                * crate::style::MONO
                                                * 0.6,
                                    },
                                ));
                            }
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
                    Action::Link { .. }
                    | Action::Navigate { .. }
                    | Action::Back(_)
                    | Action::AnchorOffset { .. }
                    | Action::SaveFile
                    | Action::Saved { .. }
                    | Action::DiscardEdits
                    | Action::CancelDiscard
                    | Action::PickFolder
                    | Action::FolderPicked { .. }
                    | Action::ToggleDrawer
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

#[derive(Debug)]
enum LinkTarget {
    Web(String),
    File(HostPath, String),
}

fn resolve_link(source: &HostPath, target: &str) -> Result<LinkTarget, String> {
    if target.starts_with("https://") || target.starts_with("http://") {
        return Ok(LinkTarget::Web(target.into()));
    }
    if target.starts_with("//") || target.split('/').next().unwrap_or("").contains(':') {
        return Err(format!("Unsupported link: {target}"));
    }
    let (path, fragment) = target.split_once('#').unwrap_or((target, ""));
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    use std::os::unix::ffi::OsStringExt;
    let decoded = percent_encoding::percent_decode_str(path).collect::<Vec<_>>();
    if decoded.contains(&0) {
        return Err("Link contains a NUL byte".into());
    }
    let source = source.to_path().map_err(|e| e.to_string())?;
    let path = if path.is_empty() {
        source.clone()
    } else {
        source
            .parent()
            .ok_or("Document has no parent directory")?
            .join(std::ffi::OsString::from_vec(decoded))
    };
    // Normalize lexically: the daemon resolves and reads host paths, never this GUI process.
    let mut normalized = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            part => normalized.push(part.as_os_str()),
        }
    }
    let fragment = percent_encoding::percent_decode_str(fragment)
        .decode_utf8()
        .map_err(|_| "Heading link is not UTF-8")?
        .into_owned();
    Ok(LinkTarget::File(HostPath::from_path(&normalized), fragment))
}

fn heading_index(items: &[markdown::Item], fragment: &str) -> Option<usize> {
    let mut used = std::collections::BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        if let markdown::Item::Heading(_, text) = item {
            let text = text
                .spans(crate::ui::markdown_settings().style)
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>();
            let slug: String = text
                .to_lowercase()
                .chars()
                .filter_map(|c| {
                    if c.is_alphanumeric() || c == '_' || c == '-' {
                        Some(c)
                    } else if c.is_whitespace() {
                        Some('-')
                    } else {
                        None
                    }
                })
                .collect();
            let mut anchor = slug.clone();
            let mut suffix = 0;
            while used.contains(&anchor) {
                suffix += 1;
                anchor = format!("{slug}-{suffix}");
            }
            used.insert(anchor.clone());
            if anchor == fragment {
                return Some(index);
            }
        }
    }
    None
}

impl App {
    pub(crate) fn reader_anchor(&mut self, session: &str) -> Task<Message> {
        if self.selected.as_deref() != Some(session) {
            return Task::none();
        }
        let Some(nav) = self.workspace.navigation.get_mut(session) else {
            return Task::none();
        };
        let Some((key, fragment)) = nav.anchor.as_ref() else {
            return Task::none();
        };
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        let Some(file) = &view.files.reader else {
            return Task::none();
        };
        if files::key(&file.path) != key || file.pending.is_some() {
            return Task::none();
        }
        let Some(reader) = &mut view.reader else {
            return Task::none();
        };
        let Some(markdown) = &reader.markdown else {
            return Task::none();
        };
        let key = key.clone();
        let fragment = fragment.clone();
        nav.anchor = None;
        if fragment.is_empty() {
            return iced::widget::operation::scroll_to(
                "markdown-reader",
                iced::widget::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(0.0),
                },
            );
        }
        let Some(index) = heading_index(markdown.items(), &fragment) else {
            reader.notice = Some(format!("Heading not found: #{fragment}"));
            return Task::none();
        };
        iced_runtime::task::widget(FindHeading {
            target: iced::advanced::widget::Id::from(format!("markdown-heading-{index}")),
            session: session.into(),
            key,
            origin: 0.0,
            y: None,
        })
    }
}

struct FindHeading {
    target: iced::advanced::widget::Id,
    session: String,
    key: String,
    origin: f32,
    y: Option<f32>,
}
impl iced::advanced::widget::operation::Operation<Message> for FindHeading {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::operation::Operation<Message>),
    ) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        _bounds: iced::Rectangle,
        content: iced::Rectangle,
        _translation: iced::Vector,
        _state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id == Some(&iced::advanced::widget::Id::from("markdown-reader")) {
            self.origin = content.y;
        }
    }
    fn container(&mut self, id: Option<&iced::advanced::widget::Id>, bounds: iced::Rectangle) {
        if id == Some(&self.target) {
            self.y = Some((bounds.y - self.origin).max(0.0));
        }
    }
    fn finish(&self) -> iced::advanced::widget::operation::Outcome<Message> {
        match self.y {
            Some(y) => iced::advanced::widget::operation::Outcome::Some(Message::Workspace(
                Action::AnchorOffset {
                    session: self.session.clone(),
                    key: self.key.clone(),
                    y,
                },
            )),
            None => iced::advanced::widget::operation::Outcome::None,
        }
    }
}
