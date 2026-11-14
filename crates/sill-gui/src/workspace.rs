//! Session files, through public SFP only. A separate worker keeps scans off the conversation
//! connection. UI state belongs to a session, and replies carry request identities.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;

use iced::Task;
use iced::widget::{markdown, text_editor};
use sill_proto::{Client, Empty, admin, changes, discovery, pins, sessions::HostPath};

use crate::Message;

const MAX_TEXT: usize = 2 * 1024 * 1024;
const MAX_LINES: usize = 20_000;

pub fn key(path: &HostPath) -> &str {
    &path.bytes_base64
}
pub fn name(path: &HostPath) -> &str {
    path.display
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("/")
}

#[derive(Debug, Clone)]
pub struct Root {
    pub path: HostPath,
    pub global: bool,
    pub metadata: Option<admin::FileMetadata>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Request {
    Roots,
    Directory {
        path: HostPath,
        after: Option<HostPath>,
        pages: usize,
    },
    Inspect(HostPath),
    File {
        path: HostPath,
        repo: Option<HostPath>,
    },
    Repo(HostPath),
    Pin {
        path: HostPath,
        on: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub session: String,
    pub serial: u64,
    pub request: Request,
    pub result: Result<Data, String>,
}

#[derive(Debug, Clone)]
pub enum Data {
    Roots {
        roots: Vec<Root>,
        home: HostPath,
    },
    Directory(admin::FilesPage),
    Metadata(admin::FileMetadata),
    File(String),
    Repo {
        root: Option<HostPath>,
        head: String,
        entries: Vec<discovery::StatusEntry>,
        summary: String,
    },
    Pinned,
}

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
    Reply(Box<Reply>),
}

#[derive(Default)]
pub struct Directory {
    pub entries: Vec<admin::FileEntry>,
    pub next: Option<HostPath>,
    pub pending: Option<u64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Repo {
    pub root: Option<HostPath>,
    pub head: String,
    pub entries: Vec<discovery::StatusEntry>,
    pub summary: String,
    pub pending: Option<u64>,
    pub error: Option<String>,
}

pub struct FileView {
    pub path: HostPath,
    pub repo: Option<HostPath>,
    pub pending: Option<u64>,
    pub error: Option<String>,
    pub text: String,
    pub content: text_editor::Content,
    pub markdown: markdown::Content,
    pub source: bool,
}

#[derive(Default)]
pub struct State {
    pub roots: Vec<Root>,
    pub home: Option<HostPath>,
    pub roots_pending: Option<u64>,
    pub initialized: bool,
    pub directories: BTreeMap<String, Directory>,
    pub paths: BTreeMap<String, HostPath>,
    pub expanded: BTreeSet<String>,
    pub repos: BTreeMap<String, Repo>,
    pub open_files: Vec<HostPath>,
    pub viewer: Option<FileView>,
    pub path_input: String,
    pub inspect_pending: Option<u64>,
    pub pin_pending: Option<u64>,
    pub error: Option<String>,
    pub diff_mode: bool,
}

impl State {
    pub fn pinned(&self, path: &HostPath) -> bool {
        self.roots
            .iter()
            .any(|r| !r.global && key(&r.path) == key(path))
    }
}

pub struct Workspace {
    pub shown: bool,
    pub active: String,
    pub states: BTreeMap<String, State>,
    serial: u64,
    commands: Option<mpsc::Sender<(String, u64, Request)>>,
    demo: bool,
}

impl Workspace {
    pub fn new(demo: bool) -> (Self, Task<Message>) {
        let (commands, receiver) = mpsc::channel::<(String, u64, Request)>();
        let (updates, events) = async_channel::bounded(32);
        let task = if demo {
            Task::none()
        } else {
            std::thread::Builder::new()
                .name("sill-gui-files".into())
                .spawn(move || {
                    let mut client: Option<Client> = None;
                    while let Ok((session, serial, request)) = receiver.recv() {
                        if updates.is_closed() {
                            break;
                        }
                        let result = (|| {
                            if client.is_none() {
                                client = Some(Client::connect()?);
                            }
                            execute(client.as_ref().unwrap(), &session, &request)
                        })();
                        if result.as_ref().is_err_and(Client::is_closed) {
                            client = None;
                        }
                        let reply = Reply {
                            session,
                            serial,
                            request,
                            result: result.map_err(|e| format!("{e:#}")),
                        };
                        if updates.send_blocking(reply).is_err() {
                            break;
                        }
                    }
                })
                .expect("start file worker");
            let stream = iced::futures::stream::unfold(events, |events| async move {
                events.recv().await.ok().map(|reply| (reply, events))
            });
            Task::run(stream, |reply| {
                Message::Workspace(Action::Reply(Box::new(reply)))
            })
        };
        (
            Self {
                shown: false,
                active: String::new(),
                states: BTreeMap::new(),
                serial: 0,
                commands: (!demo).then_some(commands),
                demo,
            },
            task,
        )
    }

    pub fn state(&self) -> Option<&State> {
        self.states.get(&self.active)
    }

    pub fn select(&mut self, session: &str) {
        self.active = session.to_string();
        if !session.is_empty() {
            self.states.entry(session.to_string()).or_default();
            self.refresh();
        }
    }

    fn request(&mut self, mut request: Request) {
        if self.active.is_empty() {
            return;
        }
        self.serial += 1;
        let serial = self.serial;
        let state = self.states.get_mut(&self.active).unwrap();
        if let Request::Directory {
            path,
            after: None,
            pages,
        } = &mut request
        {
            *pages = state
                .directories
                .get(key(path))
                .map_or(1, |d| d.entries.len().div_ceil(128).max(1));
        }
        match &request {
            Request::Roots => {
                state.roots_pending = Some(serial);
            }
            Request::Directory { path, .. } => {
                state.paths.insert(key(path).to_string(), path.clone());
                let dir = state.directories.entry(key(path).to_string()).or_default();
                dir.pending = Some(serial);
                dir.error = None;
            }
            Request::File { .. } => {
                if let Some(file) = &mut state.viewer {
                    file.pending = Some(serial);
                    file.error = None;
                }
            }
            Request::Repo(path) => {
                state
                    .repos
                    .entry(key(path).to_string())
                    .or_default()
                    .pending = Some(serial);
            }
            Request::Inspect(_) => {
                state.inspect_pending = Some(serial);
            }
            Request::Pin { .. } => {
                state.pin_pending = Some(serial);
            }
        }
        if self.demo {
            let result = demo_data(&request);
            self.accept(Reply {
                session: self.active.clone(),
                serial,
                request,
                result,
            });
        } else if self
            .commands
            .as_ref()
            .is_some_and(|tx| tx.send((self.active.clone(), serial, request)).is_err())
        {
            let state = self.states.get_mut(&self.active).unwrap();
            state.error = Some("File connection stopped. Reopen the preview.".into());
            state.roots_pending = None;
            state.pin_pending = None;
        }
    }

    fn refresh(&mut self) {
        let Some(state) = self.state() else {
            return;
        };
        if state.roots_pending.is_some() {
            return;
        }
        let paths: Vec<_> = state
            .expanded
            .iter()
            .filter_map(|k| state.paths.get(k).cloned())
            .collect();
        self.request(Request::Roots);
        for path in paths {
            self.request(Request::Directory {
                path,
                after: None,
                pages: 1,
            });
        }
    }

    fn open(&mut self, path: HostPath, repo: Option<HostPath>) {
        let Some(state) = self.states.get_mut(&self.active) else {
            return;
        };
        if !state.open_files.iter().any(|p| key(p) == key(&path)) {
            state.open_files.push(path.clone());
        }
        let source =
            repo.is_none() && !matches!(path.display.rsplit('.').next(), Some("md" | "markdown"));
        state.viewer = Some(FileView {
            path: path.clone(),
            repo: repo.clone(),
            pending: None,
            error: None,
            text: String::new(),
            content: text_editor::Content::new(),
            markdown: markdown::Content::new(),
            source,
        });
        self.request(Request::File { path, repo });
    }

    pub fn update(&mut self, action: Action) {
        if let Action::Reply(reply) = action {
            self.accept(*reply);
            return;
        }
        if matches!(action, Action::ToggleDrawer) {
            self.shown = !self.shown;
            if self.shown {
                self.refresh();
            }
            return;
        }
        let Some(state) = self.states.get_mut(&self.active) else {
            return;
        };
        match action {
            Action::Refresh => {
                state.error = None;
                self.refresh();
            }
            Action::Collapse => state.expanded.clear(),
            Action::Toggle(path) => {
                let revealing = !self.shown;
                self.shown = true;
                state.inspect_pending = None;
                if revealing && state.expanded.contains(key(&path)) {
                    return;
                }
                if !state.expanded.remove(key(&path)) {
                    state.expanded.insert(key(&path).to_string());
                    self.request(Request::Directory {
                        path,
                        after: None,
                        pages: 1,
                    });
                }
            }
            Action::More(path) => {
                if let Some(dir) = state.directories.get(key(&path))
                    && dir.pending.is_none()
                    && let Some(after) = dir.next.clone()
                {
                    self.request(Request::Directory {
                        path,
                        after: Some(after),
                        pages: 1,
                    });
                }
            }
            Action::OpenAndHide { path, repo } => {
                state.inspect_pending = None;
                self.shown = false;
                self.open(path, repo);
            }
            Action::Open { path, repo } => {
                state.inspect_pending = None;
                self.open(path, repo);
            }
            Action::CloseViewer => {
                state.inspect_pending = None;
                state.viewer = None;
            }
            Action::CloseFile(id) => {
                state.open_files.retain(|p| key(p) != id);
                if state.viewer.as_ref().is_some_and(|f| key(&f.path) == id) {
                    state.viewer = None;
                }
            }
            Action::RefreshFile => {
                if let Some(file) = &state.viewer {
                    let request = Request::File {
                        path: file.path.clone(),
                        repo: file.repo.clone(),
                    };
                    self.request(request);
                }
            }
            Action::Source => {
                if let Some(file) = &mut state.viewer {
                    file.source = !file.source;
                }
            }
            Action::Text(action) => {
                if !matches!(action, text_editor::Action::Edit(_))
                    && let Some(file) = &mut state.viewer
                {
                    file.content.perform(action);
                }
            }
            Action::Path(value) => {
                state.inspect_pending = None;
                state.path_input = value;
            }
            Action::BrowsePath | Action::PinPath => {
                let pin = matches!(action, Action::PinPath);
                let path = HostPath::from_path(std::path::Path::new(state.path_input.trim()));
                if path.to_path().is_err() {
                    state.error = Some(
                        "Enter an absolute path on the daemon host (for example /home/me/project)."
                            .into(),
                    );
                } else if pin && state.pin_pending.is_none() {
                    self.request(Request::Pin { path, on: true });
                } else if !pin {
                    self.request(Request::Inspect(path));
                }
            }
            Action::Pin { path, on } => {
                if state.pin_pending.is_none() {
                    state.error = None;
                    self.request(Request::Pin { path, on });
                }
            }
            Action::DiffMode(on) => {
                state.diff_mode = on;
                if on {
                    self.load_repos();
                }
            }
            Action::DismissError => state.error = None,
            Action::ToggleDrawer | Action::Reply(_) => unreachable!(),
        }
    }

    fn load_repos(&mut self) {
        let paths: Vec<_> = self
            .state()
            .into_iter()
            .flat_map(|s| &s.roots)
            .filter(|r| r.metadata.as_ref().is_some_and(|m| m.directory))
            .map(|r| r.path.clone())
            .collect();
        for path in paths {
            self.request(Request::Repo(path));
        }
    }

    fn accept(&mut self, reply: Reply) {
        let Some(state) = self.states.get_mut(&reply.session) else {
            return;
        };
        let current = match &reply.request {
            Request::Roots => state.roots_pending,
            Request::Directory { path, .. } => {
                state.directories.get(key(path)).and_then(|d| d.pending)
            }
            Request::Repo(path) => state.repos.get(key(path)).and_then(|r| r.pending),
            Request::File { .. } => state.viewer.as_ref().and_then(|f| f.pending),
            Request::Inspect(_) => state.inspect_pending,
            Request::Pin { .. } => state.pin_pending,
        };
        if current != Some(reply.serial) {
            return;
        }
        let mut follow = Vec::new();
        match reply.request {
            Request::Roots => {
                state.roots_pending = None;
                match reply.result {
                    Ok(Data::Roots { roots, home }) => {
                        for root in &roots {
                            if root.metadata.as_ref().is_some_and(|m| m.directory)
                                && !root.global
                                && !state.initialized
                                && state.expanded.is_empty()
                            {
                                state
                                    .paths
                                    .insert(key(&root.path).to_string(), root.path.clone());
                                state.expanded.insert(key(&root.path).to_string());
                                follow.push(Request::Directory {
                                    path: root.path.clone(),
                                    after: None,
                                    pages: 1,
                                });
                            }
                        }
                        state.roots = roots;
                        state.home.get_or_insert(home);
                        state.initialized = true;
                        if state.diff_mode {
                            follow.extend(
                                state
                                    .roots
                                    .iter()
                                    .filter(|r| r.metadata.as_ref().is_some_and(|m| m.directory))
                                    .map(|r| Request::Repo(r.path.clone())),
                            );
                        }
                    }
                    Err(error) => state.error = Some(error),
                    _ => {}
                }
            }
            Request::Directory { path, after, .. } => {
                let dir = state.directories.get_mut(key(&path)).unwrap();
                dir.pending = None;
                match reply.result {
                    Ok(Data::Directory(page)) => {
                        if after.is_none() {
                            dir.entries.clear();
                        }
                        for entry in page.entries {
                            if let Some(old) = dir
                                .entries
                                .iter_mut()
                                .find(|e| key(&e.path) == key(&entry.path))
                            {
                                *old = entry;
                            } else {
                                dir.entries.push(entry);
                            }
                        }
                        dir.entries.sort_by(|a, b| {
                            b.metadata
                                .directory
                                .cmp(&a.metadata.directory)
                                .then_with(|| name(&a.path).cmp(name(&b.path)))
                        });
                        dir.next = page.next;
                    }
                    Err(error) => dir.error = Some(error),
                    _ => {}
                }
            }
            Request::Repo(path) => {
                let repo = state.repos.get_mut(key(&path)).unwrap();
                repo.pending = None;
                match reply.result {
                    Ok(Data::Repo {
                        root,
                        head,
                        entries,
                        summary,
                    }) => {
                        repo.root = root;
                        repo.head = head;
                        repo.entries = entries;
                        repo.summary = summary;
                        repo.error = None;
                    }
                    Err(error) => repo.error = Some(error),
                    _ => {}
                }
            }
            Request::File { .. } => {
                let file = state.viewer.as_mut().unwrap();
                file.pending = None;
                match reply.result {
                    Ok(Data::File(text)) => {
                        file.content = text_editor::Content::with_text(&text);
                        // Large text stays in the editor's viewport instead of thousands of Markdown widgets.
                        if text.len() > 256 * 1024
                            || (file.repo.is_some() && text.lines().count() > 2000)
                        {
                            file.source = true;
                        } else {
                            file.markdown = markdown::Content::parse(&text);
                        }
                        file.text = text;
                    }
                    Err(error) => file.error = Some(error),
                    _ => {}
                }
            }
            Request::Inspect(path) => {
                state.inspect_pending = None;
                match reply.result {
                    Ok(Data::Metadata(meta)) if meta.directory => {
                        state.paths.insert(key(&path).to_string(), path.clone());
                        state.expanded.insert(key(&path).to_string());
                        // Browsing a path adds a temporary root; pinning remains an explicit action.
                        state.home = Some(path.clone());
                        self.shown = true;
                        follow.push(Request::Directory {
                            path,
                            after: None,
                            pages: 1,
                        });
                    }
                    Ok(Data::Metadata(_)) => {
                        if self.active == reply.session {
                            self.open(path, None);
                        }
                    }
                    Err(error) => state.error = Some(error),
                    _ => {}
                }
            }
            Request::Pin { path, on } => {
                state.pin_pending = None;
                match reply.result {
                    Ok(Data::Pinned) => {
                        if on {
                            state.path_input.clear();
                        } else {
                            state.expanded.remove(key(&path));
                        }
                        follow.push(Request::Roots);
                    }
                    Err(error) => {
                        state.error = Some(format!(
                            "Pin change: {error}. Refresh to check the current pins before trying again."
                        ))
                    }
                    _ => {}
                }
            }
        }
        // A response for a background tab may update its cache, but may never act on another tab.
        if self.active == reply.session {
            for request in follow {
                self.request(request);
            }
        }
    }
}

pub fn execute(client: &Client, session: &str, request: &Request) -> anyhow::Result<Data> {
    Ok(match request {
        Request::Roots => {
            let mut roots: Vec<Root> = Vec::new();
            for global in [false, true] {
                let mut after = None;
                loop {
                    let page: pins::Page = client.call(
                        "_sill/pins.list",
                        pins::List {
                            session_id: (!global).then(|| session.to_string()),
                            after: after.clone(),
                            limit: Some(128),
                        },
                    )?;
                    for path in page.pins {
                        if roots.iter().any(|r| key(&r.path) == key(&path)) {
                            continue;
                        }
                        let metadata = client.call(
                            "_sill/files.metadata",
                            admin::FileKey { path: path.clone() },
                        );
                        let (metadata, error) = match metadata {
                            Ok(m) => (Some(m), None),
                            Err(e) => (None, Some(format!("{e:#}"))),
                        };
                        roots.push(Root {
                            path,
                            global,
                            metadata,
                            error,
                        });
                    }
                    anyhow::ensure!(roots.len() <= 4096, "too many pins to display");
                    if page.next.is_none() {
                        break;
                    }
                    anyhow::ensure!(
                        page.next.as_ref().map(key) != after.as_ref().map(key),
                        "pin listing did not advance"
                    );
                    after = page.next;
                }
            }
            Data::Roots {
                roots,
                home: client.call("_sill/files.home", Empty::default())?,
            }
        }
        Request::Directory { path, after, pages } => {
            let mut cursor = after.clone();
            let mut entries = Vec::new();
            for _ in 0..*pages {
                let page: admin::FilesPage = client.call(
                    "_sill/files.list",
                    admin::FilesList {
                        path: path.clone(),
                        after: cursor.clone(),
                        limit: Some(128),
                    },
                )?;
                anyhow::ensure!(
                    page.next.is_none() || page.next.as_ref().map(key) != cursor.as_ref().map(key),
                    "directory listing did not advance"
                );
                entries.extend(page.entries);
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
            Data::Directory(admin::FilesPage {
                entries,
                next: cursor,
            })
        }
        Request::Inspect(path) => Data::Metadata(client.call(
            "_sill/files.metadata",
            admin::FileKey { path: path.clone() },
        )?),
        Request::Pin { path, on } => {
            let _: serde_json::Value = client.call(
                if *on {
                    "_sill/sessions.pin"
                } else {
                    "_sill/sessions.unpin"
                },
                pins::SessionPin {
                    session_id: session.into(),
                    path: path.clone(),
                },
            )?;
            Data::Pinned
        }
        Request::File { path, repo } => Data::File(read_text(client, path, repo.as_ref())?),
        Request::Repo(path) => {
            let root: discovery::Root = client.call(
                "_sill/repos.root",
                discovery::PathKey { path: path.clone() },
            )?;
            let mut head = String::new();
            let mut entries = Vec::new();
            let mut summary = String::new();
            if let Some(repo) = &root.path {
                let h: discovery::HeadResult = client.call(
                    "_sill/repos.head",
                    discovery::PathKey { path: repo.clone() },
                )?;
                head = h.head.map_or_else(String::new, |h| h.label);
                let s: discovery::SummaryResult = client.call(
                    "_sill/repos.summary",
                    discovery::PathKey { path: repo.clone() },
                )?;
                if let Some(s) = s.summary {
                    summary = format!("+{} −{}  ?{}", s.insertions, s.deletions, s.untracked);
                }
                let mut after = None;
                loop {
                    let page: discovery::StatusPage = client.call(
                        "_sill/repos.status",
                        discovery::Status {
                            repo: repo.clone(),
                            after: after.clone(),
                            limit: Some(128),
                        },
                    )?;
                    entries.extend(page.entries);
                    anyhow::ensure!(entries.len() <= 10_000, "too many changed files to display");
                    if page.next.is_none() {
                        break;
                    }
                    anyhow::ensure!(
                        page.next.as_ref().map(key) != after.as_ref().map(key),
                        "repository status did not advance"
                    );
                    after = page.next;
                }
            }
            Data::Repo {
                root: root.path,
                head,
                entries,
                summary,
            }
        }
    })
}

fn read_text(client: &Client, path: &HostPath, repo: Option<&HostPath>) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    let mut offset = "0".to_string();
    let mut revision = None;
    loop {
        let (chunk, next, fingerprint) = if let Some(repo) = repo {
            let page: changes::DiffPage = client.call(
                "_sill/changes.diff",
                changes::Diff {
                    target: changes::Target::Live {
                        repo: repo.clone(),
                        path: path.clone(),
                    },
                    offset: Some(offset.clone()),
                    fingerprint: revision.clone(),
                },
            )?;
            (page.text.into_bytes(), page.next_offset, page.fingerprint)
        } else {
            let page: admin::FileBytes = client.call(
                "_sill/files.read",
                admin::FileRead {
                    path: path.clone(),
                    offset: offset.clone(),
                    length: Some(256 * 1024),
                    revision: revision.clone(),
                },
            )?;
            (
                admin::decode_bytes(&page.bytes_base64)?,
                (!page.eof).then_some(page.next_offset),
                page.revision,
            )
        };
        if let Some(expected) = &revision {
            anyhow::ensure!(
                expected == &fingerprint,
                "File changed while reading. Refresh to read it again."
            );
        }
        bytes.extend(chunk);
        anyhow::ensure!(
            bytes.len() <= MAX_TEXT,
            "This file exceeds the 2 MiB preview limit."
        );
        if let Some(next) = next {
            anyhow::ensure!(
                next.parse::<u64>()? > offset.parse::<u64>()?,
                "file read did not advance"
            );
            offset = next;
            revision = Some(fingerprint);
        } else {
            break;
        }
    }
    anyhow::ensure!(
        !bytes.contains(&0),
        "Binary file — text preview is unavailable."
    );
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("Binary or non-UTF-8 file — text preview is unavailable."))?;
    anyhow::ensure!(
        text.lines().count() <= MAX_LINES,
        "This file exceeds the 20,000-line preview limit."
    );
    if repo.is_some() && text.is_empty() {
        return Ok("No text diff available (the file may be binary or unchanged).".into());
    }
    Ok(text)
}

fn demo_path(path: &str) -> HostPath {
    HostPath::from_path(std::path::Path::new(path))
}
fn demo_metadata(directory: bool) -> admin::FileMetadata {
    admin::FileMetadata {
        file: !directory,
        directory,
        symlink: false,
        target: None,
        length: "150".into(),
        readonly: false,
    }
}

fn demo_data(request: &Request) -> Result<Data, String> {
    Ok(match request {
        Request::Roots => Data::Roots {
            roots: vec![Root {
                path: demo_path("/home/demo/project"),
                global: false,
                metadata: Some(demo_metadata(true)),
                error: None,
            }],
            home: demo_path("/home/demo"),
        },
        Request::Directory { path, .. } => {
            let entries = match path.display.as_str() {
                "/home/demo" => vec![("project", true)],
                "/home/demo/project" => vec![
                    ("src", true),
                    ("docs", true),
                    ("README.md", false),
                    ("Cargo.toml", false),
                    (".gitignore", false),
                ],
                "/home/demo/project/src" => vec![("main.rs", false)],
                "/home/demo/project/docs" => vec![("guide.md", false)],
                _ => vec![],
            };
            Data::Directory(admin::FilesPage {
                entries: entries
                    .into_iter()
                    .map(|(name, directory)| admin::FileEntry {
                        path: demo_path(&format!("{}/{name}", path.display)),
                        metadata: demo_metadata(directory),
                    })
                    .collect(),
                next: None,
            })
        }
        Request::Inspect(path) => Data::Metadata(demo_metadata(!name(path).contains('.'))),
        Request::File {
            path,
            repo: Some(_),
        } => Data::File(format!(
            "diff --git a/{0} b/{0}\n--- a/{0}\n+++ b/{0}\n@@ -1,3 +1,4 @@\n # Sill\n-A conversation preview.\n+A place to work with your agents.\n+Files and diffs stay with each session.\n",
            name(path)
        )),
        Request::File { path, .. } => Data::File(if path.display.ends_with(".rs") {
            "fn main() {\n    println!(\"Hello from Sill\");\n}\n".into()
        } else {
            "# A workspace for your session\n\nBrowse pinned folders, read files and review **what changed** without leaving the conversation.\n\n## Staying oriented\n\n- Expanded folders stay open across refreshes.\n- Each session remembers its own files.\n- Close the viewer to return to the conversation.\n\n```rust\nfn main() {\n    println!(\"Hello from Sill\");\n}\n```\n\nThis is a synthetic file in the demo.\n".into()
        }),
        Request::Repo(path) => Data::Repo {
            root: Some(path.clone()),
            head: "main".into(),
            entries: vec![
                discovery::StatusEntry {
                    path: demo_path("/home/demo/project/README.md"),
                    status: "M".into(),
                },
                discovery::StatusEntry {
                    path: demo_path("/home/demo/project/src/main.rs"),
                    status: "?".into(),
                },
            ],
            summary: "+12 −3  ?1".into(),
        },
        Request::Pin { .. } => {
            return Err("Demo pins are examples; use a real session to change pins.".into());
        }
    })
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
