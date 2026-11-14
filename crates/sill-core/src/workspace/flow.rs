//! The session flow of §12 (docs/parallel-coding.md): Apply as a repeatable checkpoint that brings
//! a workspace's commits and uncommitted changes into the person's checkout, Discard back to what
//! was last applied (kept as a snapshot for Undo).
//!
//! A **snapshot** is a commit of a clone's whole state — its HEAD plus every tracked change and
//! every untracked file that is not ignored — made with a private index, so the clone's own index
//! and files are untouched; it costs what is dirty, not the repository's size. The **mirror**
//! ([`mirror`]) is the person's side: the clone's state with the agent's paths as the checkout
//! has them, read fresh from the checkout at every look; what Apply would bring and what Discard
//! resets to. A look makes trees only, no commits, and what it leaves behind is pruned when the
//! folder changes hands ([`prune`]). `refs/sill/applied` in the clone is the snapshot of the last
//! Apply (at creation, of the base), kept to tell the agent's earlier Apply from the person's own
//! edit of a dirty file. All Git here runs through the hardened view ([`Git`]); nothing in
//! either repository's config runs.
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Stop, ok, ok_raw, read_tree_stop, run};
use crate::changes::Git;

/// The snapshot of what the person already has from this workspace.
pub const APPLIED: &str = "refs/sill/applied";

pub(super) const IDENT: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "sill"),
    ("GIT_AUTHOR_EMAIL", "sill@localhost"),
    ("GIT_COMMITTER_NAME", "sill"),
    ("GIT_COMMITTER_EMAIL", "sill@localhost"),
];

pub(super) fn rev(git: &Git, what: &str) -> Result<String> {
    let mut c = git.cmd();
    c.args(["rev-parse", "--verify", "-q", what]);
    ok(c, what)
}

fn try_rev(git: &Git, what: &str) -> Result<Option<String>> {
    let mut c = git.cmd();
    c.args(["rev-parse", "--verify", "-q", what]);
    let out = run(c)?;
    Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// A commit of the clone's whole state on top of its HEAD (see the module docs).
pub fn snapshot(clone: &Path, message: &str) -> Result<String> {
    let git = Git::open(clone)?;
    let tree = worktree_tree(&git, clone)?;
    let mut c = git.cmd_write();
    for (k, v) in IDENT {
        c.env(k, v);
    }
    c.args(["commit-tree", &tree, "-p", "HEAD", "-m", message]);
    ok(c, "snapshot: commit")
}

/// The tree of the clone's whole state now, no commit made: for a look.
pub fn state_tree(clone: &Path) -> Result<String> {
    worktree_tree(&Git::open(clone)?, clone)
}

/// Drop the objects of the clone's own store that nothing reaches any more — the trees and blobs
/// its looks left — keeping the last hour's. For when nothing else runs in the clone: the folder
/// changing hands, a jail starting. The borrowed store is never touched.
pub fn prune(clone: &Path) -> Result<()> {
    let git = Git::open(clone)?;
    let mut c = git.cmd_write();
    c.args(["prune", "--expire=1.hour.ago"]);
    ok(c, "prune").map(drop)
}

/// A tree of the working tree as `git add -A` would stage it, the index untouched: HEAD's tree
/// with every path `status` lists — tracked changes, deletions, untracked files that are not
/// ignored — read from the files; what `status` does not list is as HEAD has it. Costs what is
/// dirty, not the repository's size.
fn worktree_tree(git: &Git, root: &Path) -> Result<String> {
    let mut paths: Vec<String> = dirty(git)?.into_iter().map(|(_, _, p)| p).collect();
    paths.sort();
    paths.dedup();
    // a directory listed is a repository nested without being a submodule: kept as HEAD has it
    paths.retain(|p| !std::fs::symlink_metadata(root.join(p)).is_ok_and(|m| m.is_dir()));
    let entries = worktree_entries(git, root, &paths)?;
    build_tree(git, "HEAD", &entries)
}

/// A tree entry as git stores one: mode and blob; None where there is no file.
pub(super) type Have = Option<(String, String)>;

/// The files at `paths` below `root` as git would store them, their blobs written into `store`'s
/// own object store, read with `root`'s attributes so they are stored as the person's own commits
/// would store them; a link's target is the blob, never followed; nothing (None) where there is
/// no file or link.
///
/// `root` is a tree an agent may be writing (its workspace), and the paths come from it, so no
/// file is read by its path: each is opened below `root` with no link followed at any component
/// and git hashes the opened descriptor. A path below a link — a directory the agent replaced
/// by a link to anywhere on the host — is gone, as git itself counts it, and what the link points
/// at is never read (2026-11-10; before, `hash-object` followed it and wrote the file it reached
/// into the workspace's store). Files no attribute converts — nearly all — are hashed by one
/// `hash-object` per 128; one with `text`, `eol`, `ident`, `filter` or an encoding set is hashed
/// on its own with its path named.
pub(super) fn worktree_entries(
    store: &Git,
    root: &Path,
    paths: &[String],
) -> Result<Vec<(String, Have)>> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawMode, ResolveFlags, fstat, openat2, readlinkat, statat};
    use rustix::io::Errno;
    use std::os::fd::{AsRawFd, OwnedFd};
    let mut out: Vec<(String, Have)> = Vec::with_capacity(paths.len());
    let mut files = Vec::new();
    // a link or a file where a directory was, or nothing: the path is gone
    let gone = |e: &Errno| matches!(*e, Errno::NOENT | Errno::NOTDIR | Errno::LOOP | Errno::XDEV);
    let parent = |p: &str| -> Result<Option<(OwnedFd, std::ffi::OsString)>> {
        let rel = Path::new(p);
        let name = rel.file_name().with_context(|| format!("{p}: no file name"))?.to_os_string();
        match crate::safe_fs::open_dir_beneath(root, rel.parent().unwrap_or(Path::new("")), false) {
            Ok(dir) => Ok(Some((dir, name))),
            Err(e) if e.chain().any(|e| e.downcast_ref::<Errno>().is_some_and(gone)) => Ok(None),
            Err(e) => Err(e.context(format!("read {}", root.join(p).display()))),
        }
    };
    for (i, p) in paths.iter().enumerate() {
        let entry = match parent(p)? {
            None => None,
            Some((dir, name)) => match statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(e) if gone(&e) => None,
                Err(e) => return Err(e).with_context(|| format!("read {}", root.join(p).display())),
                Ok(st) => match FileType::from_raw_mode(st.st_mode as RawMode) {
                    FileType::Symlink => {
                        let target = readlinkat(&dir, &name, Vec::new()).with_context(|| format!("read the link {}", root.join(p).display()))?;
                        Some(("120000".to_string(), hash_stdin(store, target.as_bytes())?))
                    }
                    FileType::RegularFile => {
                        files.push(i);
                        Some((String::new(), String::new()))
                    }
                    _ => None,
                },
            },
        };
        out.push((p.clone(), entry));
    }
    let converted = converted_paths(store, root, &files.iter().map(|&i| paths[i].as_str()).collect::<Vec<_>>())?;
    for chunk in files.chunks(128) {
        // opened now, no link followed; a file that went or changed kind since is gone
        let mut open: Vec<(usize, std::sync::Arc<OwnedFd>)> = Vec::new();
        for &i in chunk {
            out[i].1 = None;
            let Some((dir, name)) = parent(&paths[i])? else { continue };
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let fd = match openat2(&dir, &name, flags, Mode::empty(), ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS) {
                Ok(fd) => fd,
                Err(e) if gone(&e) => continue,
                Err(e) => return Err(e).with_context(|| format!("read {}", root.join(&paths[i]).display())),
            };
            let st = fstat(&fd)?;
            if FileType::from_raw_mode(st.st_mode as RawMode) != FileType::RegularFile {
                continue;
            }
            out[i].1 = Some(((if st.st_mode & 0o111 != 0 { "100755" } else { "100644" }).to_string(), String::new()));
            // clear of the descriptors a child gets as its own
            open.push((i, std::sync::Arc::new(rustix::io::fcntl_dupfd_cloexec(&fd, 64)?)));
        }
        let hash = |named: Option<&str>, fds: &[std::sync::Arc<OwnedFd>]| -> Result<Vec<String>> {
            use std::os::unix::process::CommandExt;
            let mut c = store.cmd_write_over(root);
            c.args(["hash-object", "-w"]);
            match named {
                Some(path) => c.arg(format!("--path={path}")),
                None => c.arg("--no-filters"),
            };
            c.arg("--");
            for fd in fds {
                c.arg(format!("/dev/fd/{}", fd.as_raw_fd()));
            }
            let inherited = fds.to_vec();
            // SAFETY: fcntl is async-signal-safe; the descriptors stay owned by the closure.
            unsafe {
                c.pre_exec(move || {
                    for fd in &inherited {
                        rustix::io::fcntl_setfd(fd.as_ref(), rustix::io::FdFlags::empty())?;
                    }
                    Ok(())
                });
            }
            let ids: Vec<String> = ok(c, "hash-object")?.lines().map(str::to_string).collect();
            if ids.len() != fds.len() {
                bail!("hash-object: {} ids for {} files", ids.len(), fds.len());
            }
            Ok(ids)
        };
        let (plain, special): (Vec<_>, Vec<_>) = open.into_iter().partition(|(i, _)| !converted.contains(paths[*i].as_str()));
        let ids = if plain.is_empty() { vec![] } else { hash(None, &plain.iter().map(|(_, fd)| fd.clone()).collect::<Vec<_>>())? };
        for ((i, _), id) in plain.iter().zip(ids) {
            out[*i].1.as_mut().expect("opened").1 = id;
        }
        for (i, fd) in special {
            out[i].1.as_mut().expect("opened").1 = hash(Some(&paths[i]), &[fd])?.remove(0);
        }
    }
    Ok(out)
}

/// Of `paths` below `root`, those an attribute makes git convert on the way in (`text`, `eol`,
/// `ident`, a filter, an encoding): one `check-attr` for all. The view sets no `core.autocrlf`,
/// so a path with none of them is stored byte for byte.
fn converted_paths<'a>(store: &Git, root: &Path, paths: &[&'a str]) -> Result<std::collections::BTreeSet<&'a str>> {
    use std::io::Write;
    let mut found = std::collections::BTreeSet::new();
    if paths.is_empty() {
        return Ok(found);
    }
    let mut c = store.cmd_write_over(root);
    c.args(["check-attr", "-z", "--stdin", "text", "eol", "crlf", "ident", "filter", "working-tree-encoding"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn()?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0])).collect();
    // git answers as it reads: written from a thread so neither side waits on a full pipe
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !out.status.success() {
        bail!("check-attr: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.split('\0').collect();
    let known: std::collections::BTreeSet<&'a str> = paths.iter().copied().collect();
    for f in fields.chunks(3).filter(|f| f.len() == 3) {
        if !matches!(f[2], "unspecified" | "unset")
            && let Some(p) = known.get(f[0])
        {
            found.insert(*p);
        }
    }
    Ok(found)
}

/// `bytes` as a blob in `store`.
fn hash_stdin(store: &Git, bytes: &[u8]) -> Result<String> {
    use std::io::Write;
    let mut c = store.cmd_write();
    c.args(["hash-object", "-w", "--stdin"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn()?;
    child.stdin.take().context("no stdin")?.write_all(bytes)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("hash-object: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The files and links of `commit`'s tree at `paths`: mode and blob of each that is there (a
/// directory or a gitlink at a path is no file). One `ls-tree` for all.
fn tree_entries(git: &Git, commit: &str, paths: &[String]) -> Result<BTreeMap<String, (String, String)>> {
    let mut out = BTreeMap::new();
    for chunk in paths.chunks(256) {
        let mut c = git.cmd();
        c.args(["ls-tree", "-z", commit, "--"]);
        for p in chunk {
            c.arg(p);
        }
        for e in ok_raw(c, "ls-tree")?.split('\0').filter(|e| !e.is_empty()) {
            let Some((meta, path)) = e.split_once('\t') else { continue };
            let f: Vec<&str> = meta.split(' ').collect();
            if f.len() >= 3 && f[1] == "blob" {
                out.insert(path.to_string(), (f[0].to_string(), f[2].to_string()));
            }
        }
    }
    Ok(out)
}

static INDEXES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The tree of `base` with `entries` put in place of its own at their paths (None: the path
/// removed), made through a private index: the repository's index is untouched.
pub(super) fn build_tree(git: &Git, base: &str, entries: &[(String, Have)]) -> Result<String> {
    let n = INDEXES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = git.dirs().0.join(format!("sill-index-{}-{n}", std::process::id()));
    let _ = std::fs::remove_file(&index);
    let with = |args: &[&str]| {
        let mut c = git.cmd_write();
        c.env("GIT_INDEX_FILE", &index);
        c.args(args);
        c
    };
    let made = (|| {
        ok(with(&["read-tree", base]), "read the tree")?;
        if !entries.is_empty() {
            use std::io::Write;
            // a removal names the null id, as long as this repository's ids are
            let width = match entries.iter().find_map(|(_, e)| e.as_ref().map(|(_, b)| b.len())) {
                Some(w) => w,
                None => rev(git, base)?.len(),
            };
            let none = "0".repeat(width);
            let mut c = with(&["update-index", "-z", "--index-info"]);
            c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let mut child = c.spawn()?;
            {
                let mut stdin = child.stdin.take().context("no stdin")?;
                for (path, entry) in entries {
                    match entry {
                        Some((mode, blob)) => write!(stdin, "{mode} {blob}\t{path}\0")?,
                        None => write!(stdin, "0 {none}\t{path}\0")?,
                    }
                }
            }
            let out = child.wait_with_output()?;
            if !out.status.success() {
                bail!("update-index: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
        }
        ok(with(&["write-tree"]), "write the tree")
    })();
    let _ = std::fs::remove_file(&index);
    made
}

/// Record `snapshot` as what the person has (`refs/sill/applied`).
pub fn set_applied(clone: &Path, snapshot: &str) -> Result<()> {
    let git = Git::open(clone)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", APPLIED, snapshot]);
    ok(c, "applied ref").map(drop)
}

/// The clone's `refs/sill/applied`.
pub fn applied(clone: &Path) -> Result<String> {
    rev(&Git::open(clone)?, APPLIED)
}

/// The person's side of a workspace, as it is now (§12): the workspace's state with every path of
/// the agent's work — changed since where the workspace's own commits start, or applied and still
/// uncommitted in the checkout — replaced by what the person's checkout has there. What Apply
/// would bring is the difference from it to the workspace: nothing where the checkout already has
/// the workspace's content, however it got there; a revert where the checkout has the agent's
/// earlier Apply and the workspace no longer does. Discard resets the workspace to it. Made fresh
/// from the checkout at every look, so no record of what the person has can go stale under them
/// (2026-11-09; the recorded last Apply had, twice). `refs/sill/applied` stays only to tell the
/// agent's earlier Apply from the person's own edit of a dirty file.
pub struct Mirror {
    /// The tree of the person's side, in the clone's own store.
    pub tree: String,
    /// Where the workspace's own commits start: the checkout's tip when the workspace has it,
    /// else their common ancestor (a rebase is due). `rev-list upstream..branch` is the
    /// workspace's own commits.
    pub upstream: String,
    /// The tree of the workspace's state now.
    pub now: String,
    /// Whether the workspace differs from the person's side at all: anything to apply.
    pub changed: bool,
    /// The person's own uncommitted work the workspace does not have (§5: a workspace starts from
    /// HEAD): paths dirty in the checkout whose content there is neither the workspace's nor the
    /// agent's earlier Apply. Sorted.
    pub checkout_only: Vec<String>,
    /// The agent's paths: those whose content in `tree` is the checkout's (above). Sorted.
    pub paths: Vec<String>,
}

pub fn mirror(source: &Path, clone: &Path, workspace_branch: &str) -> Result<Mirror> {
    let ws = Git::open(clone)?;
    let src = Git::open(source)?;
    let tip = rev(&ws, &format!("refs/heads/{workspace_branch}"))?;
    let (_, dest) = super::head_of(&src)?;
    let upstream = if super::descends_in(&ws, &dest, &tip)? {
        dest
    } else {
        let mut c = ws.cmd();
        c.args(["merge-base", &dest, &tip]);
        let out = run(c)?;
        if out.status.success() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { rev(&ws, "refs/sill/base")? }
    };
    let now = worktree_tree(&ws, clone)?;
    let applied = rev(&ws, APPLIED)?;
    // the agent's paths: changed since upstream…
    let mut paths: Vec<String> = diff_tree(&ws, &upstream, &now)?.into_iter().filter(|e| e.mode != "160000").map(|e| e.path).collect();
    // …or dirty in the checkout, unstaged, holding what the agent's earlier Apply wrote there
    let dirty = dirty(&src)?;
    let dirty_paths: Vec<String> = dirty.iter().map(|(_, _, p)| p.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let unstaged: Vec<String> =
        dirty.into_iter().filter(|(x, _, p)| (*x == ' ' || *x == '?') && !paths.contains(p)).map(|(_, _, p)| p).collect();
    // every dirty path of the checkout is read, in the one batch: the agent's are told from the
    // person's own below
    let read: Vec<String> = paths.iter().chain(dirty_paths.iter().filter(|p| !paths.contains(p))).cloned().collect();
    let mut host: BTreeMap<String, Have> = worktree_entries(&ws, source, &read)?.into_iter().collect();
    let was = tree_entries(&ws, &applied, &dirty_paths)?;
    let here = tree_entries(&ws, &now, &read)?;
    let checkout_only: Vec<String> =
        dirty_paths.into_iter().filter(|p| !same_blob(host.get(p), here.get(p)) && !same_blob(host.get(p), was.get(p))).collect();
    for p in unstaged {
        if same_blob(host.get(&p), was.get(&p)) {
            paths.push(p);
        }
    }
    paths.sort();
    paths.dedup();
    // what the checkout has on them, where that differs from the workspace
    let entries: Vec<(String, Have)> = paths
        .iter()
        .cloned()
        .filter_map(|p| {
            let theirs = host.remove(&p).flatten();
            (theirs.as_ref() != here.get(&p)).then_some((p, theirs))
        })
        .collect();
    let tree = build_tree(&ws, &now, &entries)?;
    Ok(Mirror { changed: tree != now, tree, upstream, now, checkout_only, paths })
}

/// Whether a working tree's entry holds the blob a tree has at the path: the same content, or
/// nothing on both sides (a deletion the agent applied is the agent's too).
fn same_blob(theirs: Option<&Have>, tree: Option<&(String, String)>) -> bool {
    theirs.and_then(|h| h.as_ref()).map(|(_, b)| b) == tree.map(|(_, b)| b)
}

/// Whether the workspace has anything to bring: its state against the person's side ([`mirror`]).
pub fn unapplied(source: &Path, clone: &Path, workspace_branch: &str) -> Result<bool> {
    Ok(mirror(source, clone, workspace_branch)?.changed)
}

/// One entry of `git diff-tree -r -z`: new mode and blob (None when deleted) and the path.
pub(super) struct Entry {
    pub(super) mode: String,
    pub(super) blob: Option<String>,
    pub(super) path: String,
}

pub(super) fn diff_tree(git: &Git, from: &str, to: &str) -> Result<Vec<Entry>> {
    let mut c = git.cmd();
    c.args(["diff-tree", "-r", "-z", "--no-renames", from, to]);
    let out = ok_raw(c, "diff-tree")?;
    let mut parts = out.split('\0');
    let mut entries = Vec::new();
    while let Some(head) = parts.next() {
        let Some(head) = head.strip_prefix(':') else { continue };
        let Some(path) = parts.next() else { break };
        let f: Vec<&str> = head.split(' ').collect();
        if f.len() < 5 {
            continue;
        }
        entries.push(Entry {
            mode: f[1].to_string(),
            blob: (!f[4].starts_with('D')).then(|| f[3].to_string()),
            path: path.to_string(),
        });
    }
    Ok(entries)
}

/// A working tree's dirty entries: (index status, worktree status, path), untracked as `??`,
/// a rename's both paths.
pub(super) fn dirty(git: &Git) -> Result<Vec<(char, char, String)>> {
    let mut c = git.cmd();
    c.args(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=all"]);
    let out = ok_raw(c, "status")?;
    let mut parts = out.split('\0');
    let mut entries = Vec::new();
    while let Some(e) = parts.next() {
        if e.len() < 4 {
            continue;
        }
        let mut chars = e.chars();
        let (x, y) = (chars.next().unwrap(), chars.next().unwrap());
        let path = e[3..].to_string();
        if (x == 'R' || x == 'C')
            && let Some(from) = parts.next()
        {
            entries.push((x, y, from.to_string()));
        }
        entries.push((x, y, path));
    }
    Ok(entries)
}

/// Write the tree entry `blob` (mode `mode`, read from `git`) to `path` below `root`, never
/// following a symlink on the way; a gitlink is skipped.
pub(super) fn write_entry(git: &Git, root: &Path, path: &str, mode: &str, blob: &str) -> Result<()> {
    let rel = Path::new(path);
    if mode == "160000" {
        return Ok(());
    }
    let mut c = git.cmd();
    c.args(["cat-file", "blob", blob]);
    let out = run(c)?;
    if !out.status.success() {
        bail!("read {path}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if mode == "120000" {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().context("no file name")?;
        let dir = crate::safe_fs::open_dir_beneath(root, parent, true)?;
        let _ = rustix::fs::unlinkat(&dir, name, rustix::fs::AtFlags::empty());
        rustix::fs::symlinkat(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&out.stdout), &dir, name)
            .with_context(|| format!("link {path}"))?;
        return Ok(());
    }
    if let Ok(meta) = std::fs::symlink_metadata(root.join(rel))
        && meta.file_type().is_symlink()
    {
        crate::safe_fs::remove_file_beneath(root, rel)?;
    }
    crate::safe_fs::write_file_beneath(root, rel, &out.stdout, if mode == "100755" { 0o755 } else { 0o644 })
}

pub(super) fn remove_entry(root: &Path, path: &str) -> Result<()> {
    match std::fs::symlink_metadata(root.join(path)) {
        Ok(_) => crate::safe_fs::remove_file_beneath(root, Path::new(path)),
        Err(_) => Ok(()),
    }
}

/// What an Apply did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The checkout's branch before and after the fast-forward (equal when nothing was committed).
    pub before: String,
    pub tip: String,
    /// The snapshot now applied: the next `refs/sill/applied`.
    pub snapshot: String,
}

/// Apply (§12): bring the clone's commits (fast-forward from `expected`) and its uncommitted
/// changes into the person's checkout `source`. A dirty file there whose content is what the last
/// Apply (`applied`) wrote counts as the agent's and is updated; any other dirty file the result
/// touches is the person's and stops it, nothing written. The caller records the returned
/// snapshot as `refs/sill/applied`.
pub fn apply(
    source: &Path,
    clone: &Path,
    id: &str,
    branch: &str,
    expected: &str,
    workspace_branch: &str,
    applied: &str,
) -> Result<std::result::Result<Applied, Stop>> {
    let src = Git::open(source)?;
    let head = std::fs::read_to_string(src.dirs().0.join("HEAD"))?;
    if head.trim() != format!("ref: refs/heads/{branch}") || rev(&src, &format!("refs/heads/{branch}"))? != expected {
        return Ok(Err(Stop::SourceMoved));
    }
    let ws = Git::open(clone)?;
    let tip = rev(&ws, &format!("refs/heads/{workspace_branch}"))?;
    if !super::descends(clone, expected, &tip)? {
        return Ok(Err(Stop::NeedsPrepare));
    }
    let snap = snapshot(clone, "sill: applied")?;
    // the paths the result touches: since the branch tip, and since the last Apply (a revert of
    // an applied uncommitted edit changes nothing against the tip, but the person's checkout)
    let changed: std::collections::BTreeSet<String> =
        diff_tree(&ws, expected, &snap)?.into_iter().chain(diff_tree(&ws, applied, &snap)?).map(|e| e.path).collect();
    // the person's own dirty files against the agent's earlier Apply
    let (mut ours, mut overlap, mut untracked) = (Vec::new(), Vec::new(), Vec::new());
    let entries = dirty(&src)?;
    let paths: Vec<String> = entries.iter().map(|(_, _, p)| p.clone()).collect();
    let theirs: BTreeMap<String, Have> = worktree_entries(&ws, source, &paths)?.into_iter().collect();
    let was = tree_entries(&ws, applied, &paths)?;
    for (x, _y, path) in entries {
        let staged = x != ' ' && x != '?';
        let is_ours = !staged && same_blob(theirs.get(&path), was.get(&path));
        if is_ours {
            ours.push(path);
        } else if changed.contains(&path) {
            if x == '?' { untracked.push(path) } else { overlap.push(path) }
        }
    }
    // a file of the person's that their `status` does not list — an ignored one — where the
    // uncommitted part would write (the workspace un-ignored the path, or staged it by force):
    // what is on disk decides, not what status says. Theirs unless it already holds what would
    // be written or what the last Apply wrote. A path tracked at either tip is `read-tree`'s.
    let uncommitted = diff_tree(&ws, &tip, &snap)?;
    {
        let listed: std::collections::BTreeSet<&str> = paths.iter().map(String::as_str).collect();
        let there: Vec<String> = uncommitted
            .iter()
            .filter(|e| e.blob.is_some() && !listed.contains(e.path.as_str()) && std::fs::symlink_metadata(source.join(&e.path)).is_ok())
            .map(|e| e.path.clone())
            .collect();
        if !there.is_empty() {
            let (old, new) = (tree_entries(&src, expected, &there)?, tree_entries(&ws, &tip, &there)?);
            let rest: Vec<String> = there.into_iter().filter(|p| !old.contains_key(p) && !new.contains_key(p)).collect();
            let (want, was) = (tree_entries(&ws, &snap, &rest)?, tree_entries(&ws, applied, &rest)?);
            for (p, have) in worktree_entries(&ws, source, &rest)? {
                let holds = |tree: &BTreeMap<String, (String, String)>| have.as_ref().is_some_and(|(_, b)| tree.get(&p).is_some_and(|(_, t)| t == b));
                if !holds(&want) && !holds(&was) {
                    untracked.push(p);
                }
            }
        }
    }
    if !overlap.is_empty() {
        return Ok(Err(Stop::Overlap(overlap)));
    }
    if !untracked.is_empty() {
        return Ok(Err(Stop::Untracked(untracked)));
    }
    if tip != expected {
        // the commits into the person's own store first, from the clone's view into the source's
        let incoming = super::anchor_ref(id, "incoming");
        let mut c = src.cmd_write();
        c.args(["fetch", "-q", "--no-tags", "--no-write-fetch-head"])
            .arg(ws.view_dir())
            .arg(format!("+refs/heads/{workspace_branch}:{incoming}"));
        ok(c, "import")?;
    }
    // the agent's earlier uncommitted changes go back to the branch's state; the result writes
    // them again as they are now
    let rollback = |paths: &[String]| {
        for p in paths {
            match try_rev(&ws, &format!("{applied}:{p}")).ok().flatten() {
                Some(blob) => {
                    let mode = tree_mode(&ws, applied, p).unwrap_or_else(|| "100644".into());
                    let _ = write_entry(&ws, source, p, &mode, &blob);
                }
                None => {
                    let _ = remove_entry(source, p);
                }
            }
        }
    };
    for p in &ours {
        match try_rev(&src, &format!("{expected}:{p}"))? {
            Some(blob) => write_entry(&src, source, p, &tree_mode(&src, expected, p).unwrap_or_else(|| "100644".into()), &blob)?,
            None => remove_entry(source, p)?,
        }
    }
    if tip != expected {
        let mut c = src.cmd_write();
        c.args(["update-index", "-q", "--refresh"]);
        let _ = run(c);
        let mut c = src.cmd_write();
        c.args(["read-tree", "-m", "-u", "-n", expected, &tip]);
        let dry = run(c)?;
        if !dry.status.success() {
            rollback(&ours);
            return Ok(Err(read_tree_stop(&String::from_utf8_lossy(&dry.stderr))));
        }
        let message = format!("sill: apply workspace {id}");
        let mut c = src.cmd_write();
        c.args(["update-ref", "-m", &message]).arg(format!("refs/heads/{branch}")).arg(&tip).arg(expected);
        if !run(c)?.status.success() {
            rollback(&ours);
            return Ok(Err(Stop::SourceMoved));
        }
        let mut c = src.cmd_write();
        c.args(["read-tree", "-m", "-u", expected, &tip]);
        let moved = run(c)?;
        if !moved.status.success() {
            let mut c = src.cmd_write();
            c.args(["update-ref", "-m", "sill: apply undone"]).arg(format!("refs/heads/{branch}")).arg(expected).arg(&tip);
            ok(c, "put the branch back")?;
            rollback(&ours);
            return Ok(Err(read_tree_stop(&String::from_utf8_lossy(&moved.stderr))));
        }
    }
    // the uncommitted part, written into the working tree as it is in the snapshot
    for e in uncommitted {
        match &e.blob {
            Some(blob) => write_entry(&ws, source, &e.path, &e.mode, blob)?,
            None => remove_entry(source, &e.path)?,
        }
    }
    Ok(Ok(Applied { before: expected.to_string(), tip, snapshot: snap }))
}

/// The commit a rebase carries a workspace's uncommitted work in ([`commit_uncommitted`]).
const WIP: &str = "refs/sill/wip";
const WIP_SUBJECT: &str = "sill: uncommitted work";

/// Before a rebase: the clone's uncommitted work — tracked changes and untracked files that are
/// not ignored — as a commit at its branch's tip, so the rebase carries it like any other commit
/// and a conflict in it stops the rebase, for Resolve or Abort. False when there is none.
/// [`restore_uncommitted`] takes the commit off again once the rebase is over (2026-11-10;
/// before, `rebase --autostash`: a conflict when the stash went back left git's markers in the
/// files with the rebase reported clean, and the Apply that followed wrote them into the
/// person's checkout).
pub fn commit_uncommitted(clone: &Path, workspace_branch: &str) -> Result<bool> {
    let git = Git::open(clone)?;
    if dirty(&git)?.is_empty() {
        return Ok(false);
    }
    let wip = snapshot(clone, WIP_SUBJECT)?;
    for name in [WIP.to_string(), format!("refs/heads/{workspace_branch}")] {
        let mut c = git.cmd_write();
        c.args(["update-ref", "-m", WIP_SUBJECT, &name, &wip]);
        ok(c, "keep the uncommitted work")?;
    }
    let mut c = git.cmd_write();
    c.args(["read-tree", &wip]);
    ok(c, "index the uncommitted work")?;
    Ok(true)
}

/// Whether a rebase's commit of uncommitted work is still to take off ([`restore_uncommitted`]).
pub fn uncommitted_pending(clone: &Path) -> bool {
    clone.join(".git").join(WIP).exists()
        || std::fs::read_to_string(clone.join(".git/packed-refs")).is_ok_and(|t| t.lines().any(|l| l.ends_with(WIP)))
}

/// After the rebase — gone through, aborted, or finished in the workspace: the commit
/// [`commit_uncommitted`] made, now at the branch's tip, comes off and its changes are in the
/// files, uncommitted, as before. Nothing where none was made. A tip that is not that commit
/// (the agent built on it) stays as it is.
pub fn restore_uncommitted(clone: &Path, workspace_branch: &str) -> Result<()> {
    let git = Git::open(clone)?;
    if try_rev(&git, WIP)?.is_none() {
        return Ok(());
    }
    let branch = format!("refs/heads/{workspace_branch}");
    let mut c = git.cmd();
    c.args(["log", "-1", "--format=%ae%n%s", &branch]);
    if ok(c, "the tip")? == format!("{}\n{WIP_SUBJECT}", IDENT[1].1) {
        let parent = rev(&git, &format!("{branch}^"))?;
        let mut c = git.cmd_write();
        c.args(["update-ref", "-m", "sill: uncommitted again", &branch, &parent]);
        ok(c, "take the commit off")?;
        let mut c = git.cmd_write();
        c.args(["read-tree", &parent]);
        ok(c, "reset the index")?;
    }
    let mut c = git.cmd_write();
    c.args(["update-ref", "-d", WIP]);
    ok(c, "drop the kept commit").map(drop)
}

/// The mode of `path` in `commit`'s tree.
fn tree_mode(git: &Git, commit: &str, path: &str) -> Option<String> {
    let mut c = git.cmd();
    c.args(["ls-tree", commit, "--", path]);
    let out = ok(c, "ls-tree").ok()?;
    out.split_whitespace().next().map(str::to_string)
}

/// Put the clone into the state of snapshot `snap` (§12 Undo): its branch at the snapshot's
/// parent, the files as in the snapshot ([`reset_to`]).
pub fn reset_to_snapshot(clone: &Path, workspace_branch: &str, snap: &str) -> Result<()> {
    let at = rev(&Git::open(clone)?, &format!("{snap}^"))?;
    reset_to(clone, workspace_branch, snap, &at)
}

/// Put the clone into the state of tree-ish `state` with its branch at commit `at` (§12 Discard,
/// Undo): the files as in `state`, its differences from `at` uncommitted, other untracked files
/// gone, ignored files kept.
pub fn reset_to(clone: &Path, workspace_branch: &str, state: &str, at: &str) -> Result<()> {
    let gitdir = clone.join(".git");
    std::fs::write(gitdir.join("HEAD"), format!("ref: refs/heads/{workspace_branch}\n"))?;
    let git = Git::open(clone)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill: reset"]).arg(format!("refs/heads/{workspace_branch}")).arg(at);
    ok(c, "reset the branch")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", "--reset", "-u", state]);
    ok(c, "reset the files")?;
    let mut c = git.cmd_write();
    c.args(["clean", "-fdq"]);
    ok(c, "remove untracked files")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", at]);
    ok(c, "reset the index")?;
    Ok(())
}

/// Discard (§12): keep the clone's whole state as a snapshot in the source's own store
/// (`refs/sill/workspaces/<id>/discarded/<n>`, for Undo even after the folder is reused), then
/// reset the clone to the person's side ([`mirror`]): their checkout's content on the agent's
/// paths. The branch goes to their tip whenever nothing of the workspace's own would be lost
/// there — after a rebase, or when their tip has moved on past the workspace's (their commits
/// since come with it, so the workspace is clean against its own commit, not a mix of the old
/// commit and their newer files on the agent's paths, 2026-11-10); only when the two have
/// diverged does it stay at their common ancestor. Returns the discarded snapshot.
pub fn discard(
    source: &Path,
    clone: &Path,
    id: &str,
    n: &str,
    workspace_branch: &str,
) -> Result<String> {
    discard_retained(source, clone, id, n, workspace_branch, |_| Ok(()))
}

/// Persist the Undo record after retaining the snapshot, before any destructive reset.
pub(super) fn discard_retained(
    source: &Path,
    clone: &Path,
    id: &str,
    n: &str,
    workspace_branch: &str,
    retain: impl FnOnce(&str) -> Result<()>,
) -> Result<String> {
    if clone.join(".git/rebase-merge").exists() || clone.join(".git/rebase-apply").exists() {
        bail!("a rebase is stopped in the workspace");
    }
    let snap = snapshot(clone, "sill: discarded")?;
    let ws = Git::open(clone)?;
    let mut c = ws.cmd_write();
    c.args(["update-ref", "refs/sill/discarding", &snap]);
    ok(c, "discarding ref")?;
    let src = Git::open(source)?;
    let mut c = src.cmd_write();
    c.args(["fetch", "-q", "--no-tags", "--no-write-fetch-head"])
        .arg(ws.view_dir())
        .arg(format!(
            "+refs/sill/discarding:{}",
            super::anchor_ref(id, &format!("discarded/{n}"))
        ));
    ok(c, "keep the discarded work")?;
    drop(ws);
    let m = mirror(source, clone, workspace_branch)?;
    let (tree, at) = discard_target(source, clone, workspace_branch, &m)?;
    retain(&snap)?;
    reset_to(clone, workspace_branch, &tree, &at)?;
    let ws = Git::open(clone)?;
    let mut c = ws.cmd_write();
    c.args(["update-ref", "-d", "refs/sill/discarding"]);
    let _ = run(c);
    Ok(snap)
}

/// Where Discard puts the clone: the tree and the commit for its branch. When the workspace's tip
/// is behind the person's (it has no commits of its own they lack), their tip and its tree with
/// the mirror's content on the agent's paths; otherwise the mirror as it is.
fn discard_target(source: &Path, clone: &Path, workspace_branch: &str, m: &Mirror) -> Result<(String, String)> {
    let ws = Git::open(clone)?;
    let tip = rev(&ws, &format!("refs/heads/{workspace_branch}"))?;
    let (_, dest) = super::head_of(&Git::open(source)?)?;
    if m.upstream == dest || !super::descends_in(&ws, &tip, &dest)? {
        return Ok((m.tree.clone(), m.upstream.clone()));
    }
    let theirs = tree_entries(&ws, &m.tree, &m.paths)?;
    let entries: Vec<(String, Have)> = m.paths.iter().map(|p| (p.clone(), theirs.get(p).cloned())).collect();
    Ok((build_tree(&ws, &dest, &entries)?, dest))
}

/// Undo a Discard: put snapshot `discarded` back, only while the clone has nothing to bring
/// (nothing done since the Discard). `Ok(false)` when it changed.
pub fn undo(source: &Path, clone: &Path, workspace_branch: &str, discarded: &str) -> Result<bool> {
    if unapplied(source, clone, workspace_branch)? {
        return Ok(false);
    }
    reset_to_snapshot(clone, workspace_branch, discarded)?;
    Ok(true)
}

#[cfg(test)]
#[path = "flow_tests.rs"]
mod tests;
