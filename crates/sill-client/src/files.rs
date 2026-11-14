//! Session files through public SFP only: pins, directories, repositories, file text and diffs
//! (docs/pins.md, docs/protocol.md). A worker of its own keeps scans off the conversation and action
//! connections. [`FilesState`] is one session's drawer and reader, without a toolkit: requests carry
//! a serial, and a reply is applied only if it is still the one that state waits for — so a late
//! reply can neither replace a newer selection nor act on another session.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;

use sill_proto::{Client, Empty, admin, changes, discovery, pins, sessions::HostPath};

use crate::Connector;

pub const MAX_TEXT: usize = 2 * 1024 * 1024;
pub const MAX_LINES: usize = 20_000;
/// Directory and status pages are asked for this many entries at a time.
const PAGE: usize = 128;
/// Past this, text is shown in a source editor rather than as rendered rows.
pub const RENDER_LIMIT: usize = 256 * 1024;

/// A path's identity: its exact bytes.
pub fn key(path: &HostPath) -> &str {
    &path.bytes_base64
}

/// The last component, for display.
pub fn name(path: &HostPath) -> &str {
    path.display
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("/")
}

/// `path` below `base`, for display; the whole path when it is not below it.
pub fn relative(path: &HostPath, base: &HostPath) -> String {
    let base = base.display.trim_end_matches('/');
    path.display
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .map_or_else(|| path.display.clone(), str::to_owned)
}

pub fn is_markdown(path: &HostPath) -> bool {
    matches!(path.display.rsplit('.').next(), Some("md" | "markdown"))
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
    /// Line counts of a pinned repository's changed files (the first [`COUNT_CAP`]), for the Diff
    /// view; `entries` are the status they are counted for.
    Counts {
        pin: HostPath,
        entries: Vec<discovery::StatusEntry>,
    },
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
        /// Lines added and removed in the working tree, as git counts them.
        lines: Option<(String, String)>,
    },
    /// Each counted file's lines added and removed; `None` when it has no counts (binary, too
    /// large, unchanged lines).
    Counts(Vec<(String, Option<(u64, u64)>)>),
    Pinned,
}

/// The Diff view counts the lines of this many changed files per repository, as the shelf does.
pub const COUNT_CAP: usize = 150;
/// A new file larger than this gets no line count.
const NEW_FILE_MAX: u64 = 1_000_000;

/// The file worker: one connection, requests in order, each reply tagged as it was asked.
/// Dropping it lets the worker finish its current request and leave.
pub struct Files {
    requests: mpsc::Sender<(String, u64, Request)>,
}

impl Files {
    pub fn start(connect: Connector, sink: impl Fn(Reply) -> bool + Send + 'static) -> Self {
        let (requests, receiver) = mpsc::channel::<(String, u64, Request)>();
        std::thread::Builder::new()
            .name("sill-files".into())
            .spawn(move || {
                let mut client: Option<Client> = None;
                while let Ok((session, serial, request)) = receiver.recv() {
                    let result = (|| {
                        if client.as_ref().is_none_or(|c| !c.is_open()) {
                            client = Some(connect()?);
                        }
                        execute(client.as_ref().unwrap(), &session, &request)
                    })();
                    if result.as_ref().is_err_and(crate::connection_lost) {
                        client = None;
                    }
                    let reply = Reply {
                        session,
                        serial,
                        request,
                        result: result.map_err(|e| format!("{e:#}")),
                    };
                    if !sink(reply) {
                        break;
                    }
                }
            })
            .expect("start the file worker");
        Self { requests }
    }

    /// False when the worker has gone.
    pub fn send(&self, session: &str, serial: u64, request: Request) -> bool {
        self.requests
            .send((session.to_string(), serial, request))
            .is_ok()
    }
}

#[derive(Debug, Default)]
pub struct Directory {
    pub entries: Vec<admin::FileEntry>,
    pub next: Option<HostPath>,
    pub pending: Option<u64>,
    pub error: Option<String>,
    /// A listing has come back once: a reload keeps showing it, without "Loading…", until the
    /// new one is there (the 15 s refresh flickered the tree otherwise).
    pub listed: bool,
    /// What is awaited is the listing again, not its first or next page.
    pub reloading: bool,
}

#[derive(Debug, Default)]
pub struct Repo {
    pub root: Option<HostPath>,
    pub head: String,
    pub entries: Vec<discovery::StatusEntry>,
    pub summary: String,
    /// Lines added and removed in the working tree.
    pub lines: Option<(String, String)>,
    /// What git says of each changed path, and of each folder with a change below it.
    pub changes: BTreeMap<std::path::PathBuf, Change>,
    /// Lines added and removed per changed file (by key), for the Diff view.
    pub counts: BTreeMap<String, Option<(u64, u64)>>,
    /// The status the counts were asked for: asked again only when it changes.
    pub counted: Option<Vec<(String, String)>>,
    pub counts_pending: Option<u64>,
    pub pending: Option<u64>,
    pub error: Option<String>,
    /// Read once (a repository or not): a reread keeps showing what was there.
    pub read: bool,
}

/// The file open in the reader, without its rendering.
#[derive(Debug)]
pub struct Reader {
    pub path: HostPath,
    /// A repository: the file's diff in it rather than its text.
    pub repo: Option<HostPath>,
    pub pending: Option<u64>,
    pub error: Option<String>,
    pub text: String,
    /// Show the text as source rather than rendered (Markdown, numbered diff rows).
    pub source: bool,
    /// Counts each new text, so a frontend knows when to rebuild its rendering.
    pub revision: u64,
}

impl Reader {
    /// Rendering is offered: Markdown, or a diff small enough for rows.
    pub fn renderable(&self) -> bool {
        self.text.len() <= RENDER_LIMIT
            && (self.repo.is_none() || self.text.lines().count() <= 2000)
            && (self.repo.is_some() || is_markdown(&self.path))
    }
}

/// What applying a reply asks of the frontend, besides the state it already changed.
#[derive(Debug, Default)]
pub struct Effects {
    /// Requests that follow from it, for the same session.
    pub follow: Vec<Request>,
    /// The person asked to browse a folder and it is there: show the drawer — if this session is
    /// still the one they are looking at.
    pub reveal: bool,
}

#[derive(Debug, Default)]
pub struct FilesState {
    pub roots: Vec<Root>,
    pub home: Option<HostPath>,
    pub roots_pending: Option<u64>,
    pub initialized: bool,
    pub directories: BTreeMap<String, Directory>,
    pub paths: BTreeMap<String, HostPath>,
    pub expanded: BTreeSet<String>,
    pub repos: BTreeMap<String, Repo>,
    pub reader: Option<Reader>,
    pub path_input: String,
    pub inspect_pending: Option<u64>,
    pub pin_pending: Option<u64>,
    pub error: Option<String>,
    pub diff_mode: bool,
    revisions: u64,
}

/// What git says of a path in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Modified, added, renamed…
    Changed,
    /// Untracked.
    New,
    Deleted,
    /// A folder with changes somewhere below it.
    Below,
}

/// What a set of counts was read for: each file and its status.
fn signature(entries: &[discovery::StatusEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|e| (key(&e.path).to_string(), e.status.clone()))
        .collect()
}

/// One changed file's lines added and removed: git's numstat for a tracked file, the line count
/// of a new one (`shelf/diffstat.rs`); none for binary or very large files.
fn count(client: &Client, entry: &discovery::StatusEntry) -> anyhow::Result<Option<(u64, u64)>> {
    if entry.status.contains('?') {
        let meta: admin::FileMetadata = client.call(
            "_sill/files.metadata",
            admin::FileKey {
                path: entry.path.clone(),
            },
        )?;
        if !meta.file || meta.length.parse::<u64>().unwrap_or(u64::MAX) > NEW_FILE_MAX {
            return Ok(None);
        }
        let page: admin::FileBytes = client.call(
            "_sill/files.read",
            admin::FileRead {
                path: entry.path.clone(),
                offset: "0".into(),
                length: Some(NEW_FILE_MAX as u32),
                revision: None,
            },
        )?;
        let bytes = admin::decode_bytes(&page.bytes_base64)?;
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return Ok(None);
        }
        let lines = bytes.split(|b| *b == b'\n').count() as u64
            - u64::from(bytes.ends_with(b"\n") || bytes.is_empty());
        return Ok(Some((lines, 0)));
    }
    let s: discovery::SummaryResult = client.call(
        "_sill/repos.summary",
        discovery::PathKey {
            path: entry.path.clone(),
        },
    )?;
    Ok(s.summary.and_then(|s| {
        let (added, removed) = (s.insertions.parse().ok()?, s.deletions.parse().ok()?);
        // numstat has no counts for a binary file
        (added > 0 || removed > 0).then_some((added, removed))
    }))
}

/// A repository's status by path: each entry's own state, and `Below` for every folder between
/// it and the repository's root.
fn index(
    root: Option<&HostPath>,
    entries: &[discovery::StatusEntry],
) -> BTreeMap<std::path::PathBuf, Change> {
    let mut changes = BTreeMap::new();
    let Some(root) = root.and_then(|r| r.to_path().ok()) else {
        return changes;
    };
    for entry in entries {
        let Ok(path) = entry.path.to_path() else {
            continue;
        };
        let change = if entry.status.contains('?') {
            Change::New
        } else if entry.status.contains('D') {
            Change::Deleted
        } else {
            Change::Changed
        };
        let mut folder = path.parent();
        changes.insert(path.clone(), change);
        while let Some(dir) = folder {
            if dir == root || !dir.starts_with(&root) || changes.contains_key(dir) {
                break;
            }
            changes.insert(dir.to_path_buf(), Change::Below);
            folder = dir.parent();
        }
    }
    changes
}

impl FilesState {
    /// The path's git state, from the status of the pinned repository it is in.
    pub fn change(&self, path: &HostPath) -> Option<Change> {
        let path = path.to_path().ok()?;
        self.repos
            .values()
            .find_map(|r| r.changes.get(&path).copied())
    }

    /// The repository a pinned folder is in: its head and line totals.
    pub fn repo_of(&self, pin: &HostPath) -> Option<&Repo> {
        self.repos.get(key(pin)).filter(|r| r.root.is_some())
    }

    /// Pinned to this session (not only globally).
    pub fn pinned(&self, path: &HostPath) -> bool {
        self.roots
            .iter()
            .any(|r| !r.global && key(&r.path) == key(path))
    }

    /// Record `request` as the one awaited, under `serial`; what to send.
    pub fn prepare(&mut self, serial: u64, mut request: Request) -> Request {
        if let Request::Directory {
            path,
            after: None,
            pages,
        } = &mut request
        {
            // a refresh reloads as many pages as were loaded, so "load more" survives it
            *pages = self
                .directories
                .get(key(path))
                .map_or(1, |d| d.entries.len().div_ceil(PAGE).max(1));
        }
        match &request {
            Request::Roots => self.roots_pending = Some(serial),
            Request::Directory { path, after, .. } => {
                self.paths.insert(key(path).to_string(), path.clone());
                let dir = self.directories.entry(key(path).to_string()).or_default();
                dir.pending = Some(serial);
                dir.reloading = after.is_none() && dir.listed;
                dir.error = None;
            }
            Request::File { .. } => {
                if let Some(file) = &mut self.reader {
                    file.pending = Some(serial);
                    file.error = None;
                }
            }
            Request::Repo(path) => {
                self.repos.entry(key(path).to_string()).or_default().pending = Some(serial);
            }
            Request::Counts { pin, entries } => {
                let repo = self.repos.entry(key(pin).to_string()).or_default();
                repo.counts_pending = Some(serial);
                repo.counted = Some(signature(entries));
            }
            Request::Inspect(_) => self.inspect_pending = Some(serial),
            Request::Pin { .. } => self.pin_pending = Some(serial),
        }
        request
    }

    /// The worker is gone: nothing awaited will come.
    pub fn abandon(&mut self, why: &str) {
        self.error = Some(why.into());
        self.roots_pending = None;
        self.pin_pending = None;
        self.inspect_pending = None;
    }

    /// Pins and every expanded folder again; nothing while pins are still loading.
    pub fn refresh(&mut self) -> Vec<Request> {
        if self.roots_pending.is_some() {
            return vec![];
        }
        let mut requests = vec![Request::Roots];
        requests.extend(
            self.expanded
                .iter()
                .filter_map(|k| self.paths.get(k).cloned())
                .map(|path| Request::Directory {
                    path,
                    after: None,
                    pages: 1,
                }),
        );
        requests
    }

    /// Open or close a folder; the listing to load when it opens.
    pub fn toggle(&mut self, path: HostPath) -> Option<Request> {
        self.inspect_pending = None;
        if self.expanded.remove(key(&path)) {
            return None;
        }
        self.expanded.insert(key(&path).to_string());
        Some(Request::Directory {
            path,
            after: None,
            pages: 1,
        })
    }

    pub fn more(&self, path: HostPath) -> Option<Request> {
        let dir = self.directories.get(key(&path))?;
        let after = dir.next.clone().filter(|_| dir.pending.is_none())?;
        Some(Request::Directory {
            path,
            after: Some(after),
            pages: 1,
        })
    }

    /// Show a file (or its diff in `repo`) in the reader; its text to load.
    pub fn open(&mut self, path: HostPath, repo: Option<HostPath>) -> Request {
        self.inspect_pending = None;
        let source = repo.is_none() && !is_markdown(&path);
        self.reader = Some(Reader {
            path: path.clone(),
            repo: repo.clone(),
            pending: None,
            error: None,
            text: String::new(),
            source,
            revision: 0,
        });
        Request::File { path, repo }
    }

    pub fn close_reader(&mut self) {
        self.inspect_pending = None;
        self.reader = None;
    }

    pub fn reload(&self) -> Option<Request> {
        let file = self.reader.as_ref()?;
        Some(Request::File {
            path: file.path.clone(),
            repo: file.repo.clone(),
        })
    }

    pub fn toggle_source(&mut self) {
        if let Some(file) = &mut self.reader {
            file.source = !file.source;
        }
    }

    pub fn set_path(&mut self, value: String) {
        self.inspect_pending = None;
        self.path_input = value;
    }

    /// Browse, or pin, the absolute path typed below the drawer.
    pub fn typed_path(&mut self, pin: bool) -> Option<Request> {
        let path = HostPath::from_path(std::path::Path::new(self.path_input.trim()));
        if path.to_path().is_err() {
            self.error = Some(
                "Enter an absolute path on the daemon host (for example /home/me/project).".into(),
            );
            return None;
        }
        if pin {
            return self
                .pin_pending
                .is_none()
                .then_some(Request::Pin { path, on: true });
        }
        Some(Request::Inspect(path))
    }

    pub fn pin(&mut self, path: HostPath, on: bool) -> Option<Request> {
        if self.pin_pending.is_some() {
            return None;
        }
        self.error = None;
        Some(Request::Pin { path, on })
    }

    /// Files or Diff; in Diff, each pinned folder's repository to read.
    pub fn set_diff_mode(&mut self, on: bool) -> Vec<Request> {
        self.diff_mode = on;
        if !on {
            return vec![];
        }
        self.directory_roots().map(Request::Repo).collect()
    }

    /// The counts to read for the Diff view of `pin`'s repository, unless they are read already
    /// for its current status.
    fn counts_for(&self, pin: &HostPath) -> Option<Request> {
        let repo = self.repos.get(key(pin))?;
        repo.root.as_ref()?;
        let entries: Vec<_> = repo.entries.iter().take(COUNT_CAP).cloned().collect();
        if entries.is_empty() || repo.counted.as_ref() == Some(&signature(&entries)) {
            return None;
        }
        Some(Request::Counts {
            pin: pin.clone(),
            entries,
        })
    }

    fn directory_roots(&self) -> impl Iterator<Item = HostPath> + '_ {
        self.roots
            .iter()
            .filter(|r| r.metadata.as_ref().is_some_and(|m| m.directory))
            .map(|r| r.path.clone())
    }

    /// Apply a reply addressed to this state, if it is the one awaited.
    pub fn accept(&mut self, reply: Reply) -> Effects {
        let awaited = match &reply.request {
            Request::Roots => self.roots_pending,
            Request::Directory { path, .. } => {
                self.directories.get(key(path)).and_then(|d| d.pending)
            }
            Request::Repo(path) => self.repos.get(key(path)).and_then(|r| r.pending),
            Request::Counts { pin, .. } => self.repos.get(key(pin)).and_then(|r| r.counts_pending),
            Request::File { .. } => self.reader.as_ref().and_then(|f| f.pending),
            Request::Inspect(_) => self.inspect_pending,
            Request::Pin { .. } => self.pin_pending,
        };
        let mut effects = Effects::default();
        if awaited != Some(reply.serial) {
            return effects;
        }
        match reply.request {
            Request::Roots => {
                self.roots_pending = None;
                match reply.result {
                    Ok(Data::Roots { roots, home }) => {
                        // the first session folder opens by itself, once
                        for root in &roots {
                            if root.metadata.as_ref().is_some_and(|m| m.directory)
                                && !root.global
                                && !self.initialized
                                && self.expanded.is_empty()
                            {
                                self.paths
                                    .insert(key(&root.path).to_string(), root.path.clone());
                                self.expanded.insert(key(&root.path).to_string());
                                effects.follow.push(Request::Directory {
                                    path: root.path.clone(),
                                    after: None,
                                    pages: 1,
                                });
                            }
                        }
                        self.roots = roots;
                        self.home.get_or_insert(home);
                        self.initialized = true;
                        // the tree and the chips show what changed, not only the Diff view
                        effects
                            .follow
                            .extend(self.directory_roots().map(Request::Repo));
                    }
                    Err(error) => self.error = Some(error),
                    _ => {}
                }
            }
            Request::Directory { path, after, .. } => {
                let dir = self.directories.entry(key(&path).to_string()).or_default();
                dir.pending = None;
                dir.reloading = false;
                dir.listed = true;
                match reply.result {
                    Ok(Data::Directory(page)) => {
                        if after.is_none() {
                            dir.entries.clear();
                        }
                        for entry in page.entries {
                            match dir
                                .entries
                                .iter_mut()
                                .find(|e| key(&e.path) == key(&entry.path))
                            {
                                Some(old) => *old = entry,
                                None => dir.entries.push(entry),
                            }
                        }
                        // as the shelf lists a folder: folders first, hidden names last, case
                        // ignored
                        dir.entries.sort_by(|a, b| {
                            let (a_name, b_name) = (name(&a.path), name(&b.path));
                            b.metadata
                                .directory
                                .cmp(&a.metadata.directory)
                                .then_with(|| a_name.starts_with('.').cmp(&b_name.starts_with('.')))
                                .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
                                .then_with(|| a_name.cmp(b_name))
                        });
                        dir.next = page.next;
                    }
                    Err(error) => dir.error = Some(error),
                    _ => {}
                }
            }
            Request::Repo(path) => {
                let repo = self.repos.entry(key(&path).to_string()).or_default();
                repo.pending = None;
                repo.read = true;
                match reply.result {
                    Ok(Data::Repo {
                        root,
                        head,
                        entries,
                        summary,
                        lines,
                    }) => {
                        repo.root = root;
                        repo.head = head;
                        repo.entries = entries;
                        repo.summary = summary;
                        repo.lines = lines;
                        repo.changes = index(repo.root.as_ref(), &repo.entries);
                        repo.error = None;
                    }
                    Err(error) => repo.error = Some(error),
                    _ => {}
                }
                // the Diff view's counts, when what changed is not what was counted
                if self.diff_mode {
                    effects.follow.extend(self.counts_for(&path));
                }
            }
            Request::Counts { pin, .. } => {
                let repo = self.repos.entry(key(&pin).to_string()).or_default();
                repo.counts_pending = None;
                match reply.result {
                    Ok(Data::Counts(counts)) => repo.counts = counts.into_iter().collect(),
                    // no counts is not an error worth a banner: the rows show none
                    Err(_) => repo.counted = None,
                    _ => {}
                }
            }
            Request::File { .. } => {
                self.revisions += 1;
                let revision = self.revisions;
                let Some(file) = self.reader.as_mut() else {
                    return effects;
                };
                file.pending = None;
                match reply.result {
                    Ok(Data::File(text)) => {
                        // large text stays in a source editor's viewport, not thousands of rows
                        if text.len() > RENDER_LIMIT
                            || (file.repo.is_some() && text.lines().count() > 2000)
                        {
                            file.source = true;
                        }
                        file.text = text;
                        file.revision = revision;
                    }
                    Err(error) => file.error = Some(error),
                    _ => {}
                }
            }
            Request::Inspect(path) => {
                self.inspect_pending = None;
                match reply.result {
                    Ok(Data::Metadata(meta)) if meta.directory => {
                        self.paths.insert(key(&path).to_string(), path.clone());
                        self.expanded.insert(key(&path).to_string());
                        // browsing a path makes it the drawer's Browse root; pinning stays explicit
                        self.home = Some(path.clone());
                        effects.reveal = true;
                        effects.follow.push(Request::Directory {
                            path,
                            after: None,
                            pages: 1,
                        });
                    }
                    Ok(Data::Metadata(_)) => effects.follow.push(self.open(path, None)),
                    Err(error) => self.error = Some(error),
                    _ => {}
                }
            }
            Request::Pin { path, on } => {
                self.pin_pending = None;
                match reply.result {
                    Ok(Data::Pinned) => {
                        if on {
                            self.path_input.clear();
                        } else {
                            self.expanded.remove(key(&path));
                        }
                        effects.follow.push(Request::Roots);
                    }
                    Err(error) => {
                        self.error = Some(format!(
                            "Pin change: {error}. Refresh to check the current pins before trying again."
                        ))
                    }
                    _ => {}
                }
            }
        }
        effects
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
                            limit: Some(PAGE as u16),
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
                        limit: Some(PAGE as u16),
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
        Request::Counts { entries, .. } => Data::Counts(
            entries
                .iter()
                .map(|e| (key(&e.path).to_string(), count(client, e).ok().flatten()))
                .collect(),
        ),
        Request::Repo(path) => {
            // the daemon looks for `.git` from the path's parent up (`changes::repo_root`, made for
            // files), so a folder that is itself a repository is asked about through a child, as
            // the shelf does (`shelf/tree.rs`)
            let inside = path
                .to_path()
                .map(|p| HostPath::from_path(&p.join("x")))
                .unwrap_or_else(|_| path.clone());
            let root: discovery::Root =
                client.call("_sill/repos.root", discovery::PathKey { path: inside })?;
            let mut head = String::new();
            let mut entries = Vec::new();
            let mut summary = String::new();
            let mut lines = None;
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
                    lines = Some((s.insertions, s.deletions));
                }
                let mut after = None;
                loop {
                    let page: discovery::StatusPage = client.call(
                        "_sill/repos.status",
                        discovery::Status {
                            repo: repo.clone(),
                            after: after.clone(),
                            limit: Some(PAGE as u16),
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
                lines,
            }
        }
    })
}

/// A file's text, or its live diff in `repo`, read page by page against one revision: a file that
/// changes meanwhile is an error, never a mix of two versions. Bounded; binary is refused.
pub fn read_text(
    client: &Client,
    path: &HostPath,
    repo: Option<&HostPath>,
) -> anyhow::Result<String> {
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
                    length: Some(RENDER_LIMIT as u32),
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

#[cfg(all(test, unix))]
#[path = "files_tests.rs"]
mod protocol_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> HostPath {
        HostPath {
            bytes_base64: s.into(),
            display: s.into(),
        }
    }

    fn meta(directory: bool) -> admin::FileMetadata {
        admin::FileMetadata {
            file: !directory,
            directory,
            symlink: false,
            target: None,
            length: "1".into(),
            readonly: false,
        }
    }

    fn reply(serial: u64, request: Request, result: Result<Data, String>) -> Reply {
        Reply {
            session: "a".into(),
            serial,
            request,
            result,
        }
    }

    #[test]
    fn a_browsed_folder_asks_to_be_revealed_and_a_stale_reply_changes_nothing() {
        let mut state = FilesState::default();
        state.set_path("/home/me/project".into());
        let request = state.typed_path(false).unwrap();
        let request = state.prepare(1, request);
        let Request::Inspect(asked) = request.clone() else {
            panic!()
        };
        let effects = state.accept(reply(1, request.clone(), Ok(Data::Metadata(meta(true)))));
        assert!(effects.reveal);
        assert!(state.expanded.contains(key(&asked)));
        assert!(matches!(
            effects.follow.as_slice(),
            [Request::Directory { .. }]
        ));
        // the person typed again meanwhile: the old answer is not awaited any more
        state.prepare(2, Request::Inspect(asked.clone()));
        state.set_path("/elsewhere".into());
        let late = state.accept(reply(2, request, Ok(Data::Metadata(meta(true)))));
        assert!(!late.reveal && late.follow.is_empty());
    }

    #[test]
    fn a_refresh_reloads_the_pages_that_were_loaded() {
        let mut state = FilesState::default();
        let folder = path("/p");
        let request = state.toggle(folder.clone()).unwrap();
        let request = state.prepare(1, request);
        let entries = (0..200)
            .map(|i| admin::FileEntry {
                path: path(&format!("/p/{i:03}")),
                metadata: meta(false),
            })
            .collect();
        state.accept(reply(
            1,
            request,
            Ok(Data::Directory(admin::FilesPage {
                entries,
                next: Some(path("/p/199")),
            })),
        ));
        let again = state.refresh();
        let Request::Directory { .. } = &again[1] else {
            panic!("{again:?}")
        };
        let prepared = state.prepare(2, again[1].clone());
        assert!(
            matches!(prepared, Request::Directory { pages: 2, .. }),
            "{prepared:?}"
        );
        assert_eq!(relative(&path("/p/x/y"), &path("/p/")), "x/y");
    }
}
