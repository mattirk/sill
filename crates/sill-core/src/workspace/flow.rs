//! The session flow of §12 (docs/parallel-coding.md): Apply as a repeatable checkpoint that brings
//! a workspace's commits into the person's checkout by fast-forward — commits only (2026-11-10):
//! work still uncommitted is committed first, by the agent or, for an ended session, by sill
//! ([`commit_all`]) — and Discard back to the person's commit (the discarded work kept as a
//! snapshot for Undo).
//!
//! A **snapshot** is a commit of a clone's whole state — its HEAD plus every tracked change and
//! every untracked file that is not ignored — made with a private index, so the clone's own index
//! and files are untouched; it costs what is dirty, not the repository's size. The **mirror**
//! ([`mirror`]) is the person's side: their commit the workspace's own work starts from, read
//! fresh from the checkout at every look; what Apply would bring is the workspace's state
//! against it. A look makes trees only, no commits, and what it leaves behind is pruned when the
//! folder changes hands ([`prune`]). All Git here runs through the hardened view ([`Git`]);
//! nothing in either repository's config runs.
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Stop, ok, ok_raw, read_tree_stop, run};
use crate::changes::Git;

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

/// A commit of the clone's whole state on top of its HEAD (see the module docs).
pub fn snapshot(clone: &Path, message: &str) -> Result<String> {
    let git = Git::open(clone)?;
    let tree = worktree_tree(&git, clone)?;
    commit_tree(&git, &tree, message, &IDENT.map(|(k, v)| (k, v.to_string())))
}

fn commit_tree(git: &Git, tree: &str, message: &str, ident: &[(&str, String); 4]) -> Result<String> {
    let mut c = git.cmd_write();
    for (k, v) in ident {
        c.env(k, v);
    }
    c.args(["commit-tree", tree, "-p", "HEAD", "-m", message]);
    ok(c, "snapshot: commit")
}

/// Who the person's own commits in `source` are by: `user.name` and `user.email` from the
/// repository's config, else from their own (global) one. Read as values only: nothing a config
/// names runs. None when either is not set.
pub fn identity(source: &Path) -> Result<Option<(String, String)>> {
    let git = Git::open(source)?;
    let get = |key: &str| -> Result<Option<String>> {
        let mut own = std::process::Command::new("git");
        own.args(["config", "--file"]).arg(git.dirs().1.join("config")).args(["--get", key]).env("GIT_CONFIG_NOSYSTEM", "1");
        let mut theirs = git.cmd();
        theirs.args(["config", "--get", key]);
        for c in [own, theirs] {
            let out = run(c)?;
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !value.is_empty() {
                return Ok(Some(value));
            }
        }
        Ok(None)
    };
    Ok(get("user.name")?.zip(get("user.email")?))
}

/// Whether the clone holds work that is not committed: its state ([`state_tree`]) is not its
/// HEAD's tree. What [`commit_all`] would commit, so the two never disagree.
pub fn uncommitted(clone: &Path) -> Result<bool> {
    let git = Git::open(clone)?;
    Ok(worktree_tree(&git, clone)? != rev(&git, "HEAD^{tree}")?)
}

/// Commit the clone's uncommitted work — tracked changes and untracked files that are not
/// ignored — at its branch's tip, as the person (`name`, `email`) with `message`: for a workspace
/// whose session has ended, where no agent is left to commit it (2026-11-10: Apply brings commits
/// only). False when there is nothing to commit.
pub fn commit_all(clone: &Path, workspace_branch: &str, message: &str, name: &str, email: &str) -> Result<bool> {
    let git = Git::open(clone)?;
    let tree = worktree_tree(&git, clone)?;
    if tree == rev(&git, "HEAD^{tree}")? {
        return Ok(false);
    }
    let ident = [
        ("GIT_AUTHOR_NAME", name.to_string()),
        ("GIT_AUTHOR_EMAIL", email.to_string()),
        ("GIT_COMMITTER_NAME", name.to_string()),
        ("GIT_COMMITTER_EMAIL", email.to_string()),
    ];
    let commit = commit_tree(&git, &tree, message, &ident)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill: commit for Apply"]).arg(format!("refs/heads/{workspace_branch}")).arg(&commit);
    ok(c, "commit the work")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", &commit]);
    ok(c, "index the commit")?;
    Ok(true)
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

/// The person's side of a workspace, as it is now (§12): their commit the workspace's own work
/// starts from. What Apply would bring is the difference from its tree to the workspace's state;
/// Discard resets the workspace to it. Read fresh from the checkout at every look, so no record
/// of what the person has can go stale under them (2026-11-09; the recorded last Apply had,
/// twice). Since Apply brings commits only (2026-11-10), nothing of the agent's lies uncommitted
/// in the checkout: every dirty file there is the person's own.
pub struct Mirror {
    /// The tree of the person's side: `upstream`'s.
    pub tree: String,
    /// Where the workspace's own commits start: the checkout's tip when the workspace has it,
    /// else their common ancestor advanced over a linear prefix of patch-equivalent commits
    /// already in the checkout (a rebase is due). `rev-list upstream..branch` is the
    /// workspace's own commits.
    pub upstream: String,
    /// The tree of the workspace's state now.
    pub now: String,
    /// Whether the workspace differs from the person's side at all: anything to apply.
    pub changed: bool,
    /// Whether all of the workspace's state is committed: `now` is its tip's tree.
    pub committed: bool,
    /// The person's own uncommitted work the workspace does not have (§5: a workspace starts from
    /// HEAD): paths dirty in the checkout whose content there is not the workspace's. Sorted.
    pub checkout_only: Vec<String>,
}

pub fn mirror(source: &Path, clone: &Path, workspace_branch: &str) -> Result<Mirror> {
    let ws = Git::open(clone)?;
    let src = Git::open(source)?;
    let tip = rev(&ws, &format!("refs/heads/{workspace_branch}"))?;
    let (_, dest) = super::head_of(&src)?;
    let upstream = if super::descends_in(&ws, &dest, &tip)? {
        dest.clone()
    } else {
        let mut c = ws.cmd();
        c.args(["merge-base", &dest, &tip]);
        let out = run(c)?;
        if out.status.success() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { rev(&ws, "refs/sill/base")? }
    };
    // A checkout rebase rewrites identities without removing the agent's patches.
    // Advance only over a contiguous, linear prefix already present there. Never skip
    // an unmatched commit or a merge: later work must stay visible in the summary.
    let upstream = equivalent_prefix(&ws, &dest, &tip, upstream)?;
    let now = worktree_tree(&ws, clone)?;
    let tree = rev(&ws, &format!("{upstream}^{{tree}}"))?;
    let committed = now == rev(&ws, &format!("{tip}^{{tree}}"))?;
    // the person's dirty files, against what the workspace has there
    let dirty_paths: Vec<String> = dirty(&src)?.into_iter().map(|(_, _, p)| p).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let host: BTreeMap<String, Have> = worktree_entries(&ws, source, &dirty_paths)?.into_iter().collect();
    let here = tree_entries(&ws, &now, &dirty_paths)?;
    let checkout_only: Vec<String> = dirty_paths.into_iter().filter(|p| !same_blob(host.get(p), here.get(p))).collect();
    Ok(Mirror { changed: tree != now, tree, upstream, now, committed, checkout_only })
}

fn equivalent_prefix(ws: &Git, dest: &str, tip: &str, mut upstream: String) -> Result<String> {
    if upstream == tip || super::descends_in(ws, dest, tip)? {
        return Ok(upstream);
    }
    // A read can precede the next workspace fetch. Keep the base chosen by mirror
    // until the checkout's newer commit is available for patch comparison here.
    let mut known = ws.cmd();
    known.args(["cat-file", "-e", &format!("{dest}^{{commit}}")]);
    if !run(known)?.status.success() {
        return Ok(upstream);
    }
    let mut c = ws.cmd();
    c.args(["cherry", dest, tip, &upstream]);
    let cherry = ok(c, "compare rebased workspace patches")?;
    let equivalent: std::collections::BTreeSet<_> =
        cherry.lines().filter_map(|line| line.strip_prefix("- ")).collect();
    if equivalent.is_empty() {
        return Ok(upstream);
    }
    let mut c = ws.cmd();
    c.args(["rev-list", "--reverse", "--topo-order", "--parents", &format!("{upstream}..{tip}")]);
    for line in ok(c, "list workspace patch prefix")?.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 || fields[1] != upstream || !equivalent.contains(fields[0]) {
            break;
        }
        upstream = fields[0].to_string();
    }
    Ok(upstream)
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
}

/// Apply (§12): fast-forward the person's checkout `source` from `expected` to the clone's
/// commits, its files with it. Commits only: a clone with uncommitted work stops
/// ([`Stop::Uncommitted`]) for the agent, or sill for an ended session, to commit first. A dirty
/// or untracked file of the person's that the commits touch stops it, nothing written: Git's
/// own check for a fast-forward, and [`files_in_the_way`] for the ignored files Git would write
/// over. No record of earlier Applies is consulted: every dirty file there is the person's.
pub fn apply(
    source: &Path,
    clone: &Path,
    id: &str,
    branch: &str,
    expected: &str,
    workspace_branch: &str,
    copied: &Copied,
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
    if worktree_tree(&ws, clone)? != rev(&ws, &format!("{tip}^{{tree}}"))? {
        return Ok(Err(Stop::Uncommitted));
    }
    if tip == expected {
        return Ok(Ok(Applied { before: expected.to_string(), tip }));
    }
    // what an earlier copy left uncommitted in the checkout goes back to the branch's state
    // first: the fast-forward writes it again, committed
    let ours = match copied_in_the_way(&src, &ws, source, expected, &tip, copied)? {
        Ok(ours) => ours,
        Err(stop) => return Ok(Err(stop)),
    };
    for (path, _) in &ours {
        match try_entry(&src, expected, path)? {
            Some((mode, blob)) => write_entry(&src, source, path, &mode, &blob)?,
            None => remove_entry(source, path)?,
        }
    }
    let done = fast_forward(&src, &ws, source, id, branch, expected, workspace_branch, &tip);
    if !matches!(done, Ok(Ok(_))) {
        // nothing applied: the copy's files as they were
        for (path, was) in &ours {
            let _ = match was {
                Some((mode, blob)) => write_entry(&ws, source, path, mode, blob),
                None => remove_entry(source, path),
            };
        }
    }
    done
}

/// [`apply`]'s fast-forward itself, the checkout's files checked and written by Git.
#[allow(clippy::too_many_arguments)]
fn fast_forward(
    src: &Git,
    ws: &Git,
    source: &Path,
    id: &str,
    branch: &str,
    expected: &str,
    workspace_branch: &str,
    tip: &str,
) -> Result<std::result::Result<Applied, Stop>> {
    let tip = tip.to_string();
    let collisions = files_in_the_way(src, ws, source, expected, &tip)?;
    if !collisions.is_empty() {
        return Ok(Err(Stop::Untracked(collisions)));
    }
    // the commits into the person's own store first, from the clone's view into the source's
    let incoming = super::anchor_ref(id, "incoming");
    let mut c = src.cmd_write();
    c.args(["fetch", "-q", "--no-tags", "--no-write-fetch-head"]).arg(ws.view_dir()).arg(format!("+refs/heads/{workspace_branch}:{incoming}"));
    ok(c, "import")?;
    let mut c = src.cmd_write();
    c.args(["update-index", "-q", "--refresh"]);
    let _ = run(c);
    let mut c = src.cmd_write();
    c.args(["read-tree", "-m", "-u", "-n", expected, &tip]);
    let dry = run(c)?;
    if !dry.status.success() {
        return Ok(Err(read_tree_stop(&String::from_utf8_lossy(&dry.stderr))));
    }
    let message = format!("sill: apply workspace {id}");
    let mut c = src.cmd_write();
    c.args(["update-ref", "-m", &message]).arg(format!("refs/heads/{branch}")).arg(&tip).arg(expected);
    if !run(c)?.status.success() {
        return Ok(Err(Stop::SourceMoved));
    }
    let mut c = src.cmd_write();
    c.args(["read-tree", "-m", "-u", expected, &tip]);
    let moved = run(c)?;
    if !moved.status.success() {
        let mut c = src.cmd_write();
        c.args(["update-ref", "-m", "sill: apply undone"]).arg(format!("refs/heads/{branch}")).arg(expected).arg(&tip);
        ok(c, "put the branch back")?;
        return Ok(Err(read_tree_stop(&String::from_utf8_lossy(&moved.stderr))));
    }
    Ok(Ok(Applied { before: expected.to_string(), tip }))
}

/// A file's mode and blob in `commit`, None where it has none.
fn try_entry(git: &Git, commit: &str, path: &str) -> Result<Have> {
    Ok(tree_entries(git, commit, &[path.to_string()])?.remove(path))
}

/// What "Apply without commit" wrote into the person's checkout ([`copy`]): each path's blob, None
/// where it removed the file. Kept by sill, outside the workspace: a file of the checkout that
/// still holds exactly this is the copy's and may be written over by the next copy or Apply; one
/// that holds anything else is the person's.
pub type Copied = BTreeMap<String, Option<String>>;

/// The checkout's unstaged files, among `paths`, as they are on disk.
fn unstaged(src: &Git, ws: &Git, source: &Path, paths: &std::collections::BTreeSet<String>) -> Result<Vec<(String, Have)>> {
    let listed: Vec<String> = dirty(src)?
        .into_iter()
        .filter(|(x, _, p)| (*x == ' ' || *x == '?') && paths.contains(p))
        .map(|(_, _, p)| p)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    worktree_entries(ws, source, &listed)
}

/// Before a fast-forward from `expected` to `tip`: the checkout's unstaged files that an earlier
/// copy wrote — still holding what it wrote, or already what the commits bring — each with what
/// it holds now, for [`apply`] to put back to the branch's state. A file the commits change that
/// holds anything else is the person's own edit and stops the Apply, nothing written. Staged
/// files are Git's to judge.
fn copied_in_the_way(src: &Git, ws: &Git, source: &Path, expected: &str, tip: &str, copied: &Copied) -> Result<std::result::Result<Vec<(String, Have)>, Stop>> {
    if copied.is_empty() {
        return Ok(Ok(vec![]));
    }
    let brought: std::collections::BTreeSet<String> = diff_tree(ws, expected, tip)?.into_iter().map(|e| e.path).collect();
    let paths: std::collections::BTreeSet<String> = brought.iter().chain(copied.keys()).cloned().collect();
    let there = unstaged(src, ws, source, &paths)?;
    let listed: Vec<String> = there.iter().map(|(p, _)| p.clone()).collect();
    let (base, want) = (tree_entries(src, expected, &listed)?, tree_entries(ws, tip, &listed)?);
    let (mut ours, mut overlap, mut untracked) = (Vec::new(), Vec::new(), Vec::new());
    for (path, have) in there {
        let blob = have.as_ref().map(|(_, b)| b);
        if blob == base.get(&path).map(|(_, b)| b) {
            continue;
        }
        let wrote = copied.get(&path).is_some_and(|w| w.as_ref() == blob);
        if wrote || (copied.contains_key(&path) && blob == want.get(&path).map(|(_, b)| b)) {
            ours.push((path, have));
        } else if brought.contains(&path) {
            if base.contains_key(&path) { overlap.push(path) } else { untracked.push(path) }
        }
    }
    Ok(if !overlap.is_empty() {
        Err(Stop::Overlap(overlap))
    } else if !untracked.is_empty() {
        Err(Stop::Untracked(untracked))
    } else {
        Ok(ours)
    })
}

/// Apply without commit (2026-11-12): write the workspace's whole state — its commits and its
/// uncommitted work, untracked files included — into the person's checkout as uncommitted
/// changes, for them to try. No commit is made, the checkout's branch and index stay as they
/// are, and the workspace is unchanged: its work is still there to Apply. Returns what it wrote,
/// for the next copy or Apply to know the copy's files from the person's (`before`: what the
/// last one wrote). A file of the person's — changed or untracked in the checkout, holding
/// neither what the last copy wrote nor what this one would — stops it, nothing written.
pub fn copy(source: &Path, clone: &Path, branch: &str, expected: &str, before: &Copied) -> Result<std::result::Result<Copied, Stop>> {
    let src = Git::open(source)?;
    let head = std::fs::read_to_string(src.dirs().0.join("HEAD"))?;
    if head.trim() != format!("ref: refs/heads/{branch}") || rev(&src, &format!("refs/heads/{branch}"))? != expected {
        return Ok(Err(Stop::SourceMoved));
    }
    let ws = Git::open(clone)?;
    if !super::descends(clone, expected, &rev(&ws, "HEAD")?)? {
        return Ok(Err(Stop::NeedsPrepare));
    }
    let state = worktree_tree(&ws, clone)?;
    let changes: BTreeMap<String, Have> = diff_tree(&ws, expected, &state)?
        .into_iter()
        .filter(|e| e.mode != "160000")
        .map(|e| (e.path, e.blob.map(|b| (e.mode, b))))
        .collect();
    // a path the last copy wrote that the workspace no longer changes goes back too
    let paths: Vec<String> = changes.keys().chain(before.keys()).cloned().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let base = tree_entries(&ws, expected, &paths)?;
    let (mut write, mut overlap, mut untracked) = (Vec::new(), Vec::new(), Vec::new());
    for (path, have) in worktree_entries(&ws, source, &paths)? {
        let want = changes.get(&path).cloned().unwrap_or_else(|| base.get(&path).cloned());
        if have == want {
            continue;
        }
        let blob = have.as_ref().map(|(_, b)| b);
        let on_disk = std::fs::symlink_metadata(source.join(&path)).ok();
        let clean = blob == base.get(&path).map(|(_, b)| b) && !on_disk.as_ref().is_some_and(|m| m.is_dir());
        let wrote = before.get(&path).is_some_and(|w| w.as_ref() == blob);
        // something on the way down to it that is no directory
        let blocked = want.is_some() && Path::new(&path).ancestors().skip(1).any(|d| !d.as_os_str().is_empty() && std::fs::symlink_metadata(source.join(d)).is_ok_and(|m| !m.is_dir()));
        if (clean || wrote) && !blocked {
            write.push((path, want));
        } else if base.contains_key(&path) {
            overlap.push(path);
        } else {
            untracked.push(path);
        }
    }
    if !overlap.is_empty() {
        return Ok(Err(Stop::Overlap(overlap)));
    }
    if !untracked.is_empty() {
        return Ok(Err(Stop::Untracked(untracked)));
    }
    for (path, want) in &write {
        match want {
            Some((mode, blob)) => write_entry(&ws, source, path, mode, blob)?,
            None => remove_entry(source, path)?,
        }
    }
    Ok(Ok(changes.into_iter().map(|(p, e)| (p, e.map(|(_, b)| b))).collect()))
}

/// Files of the person's that Git does not track, where the fast-forward from `expected` to `tip`
/// would put a file: each path the commits add that is on disk in the checkout with other
/// content, or lies below something there that is not a directory. `read-tree` itself stops only
/// at an untracked file the person's `.gitignore` does not cover and writes over an ignored one —
/// a `.env` with no other copy — so what is on disk decides, not what `status` says. A file
/// that already holds what would be written is not in the way.
fn files_in_the_way(src: &Git, ws: &Git, source: &Path, expected: &str, tip: &str) -> Result<Vec<String>> {
    let added: Vec<Entry> = diff_tree(ws, expected, tip)?.into_iter().filter(|e| e.blob.is_some() && e.mode != "160000").collect();
    let paths: Vec<String> = added.iter().map(|e| e.path.clone()).collect();
    let tracked = tree_entries(src, expected, &paths)?;
    let added: Vec<Entry> = added.into_iter().filter(|e| !tracked.contains_key(&e.path)).collect();
    let mut there = Vec::new();
    let mut below = std::collections::BTreeSet::new();
    for e in &added {
        // something on the way that is no directory: the nearest such ancestor
        let mut at = Path::new(&e.path).parent();
        let mut blocked = None;
        while let Some(dir) = at.filter(|d| !d.as_os_str().is_empty()) {
            if std::fs::symlink_metadata(source.join(dir)).is_ok_and(|m| !m.is_dir()) {
                blocked = Some(dir.to_string_lossy().into_owned());
            }
            at = dir.parent();
        }
        match blocked {
            Some(dir) => drop(below.insert(dir)),
            None if std::fs::symlink_metadata(source.join(&e.path)).is_ok() => there.push(e.path.clone()),
            None => {}
        }
    }
    // an ancestor Git tracks is the fast-forward's own to replace
    let below: Vec<String> = below.into_iter().collect();
    let tracked = tree_entries(src, expected, &below)?;
    let mut collisions: Vec<String> = below.into_iter().filter(|d| !tracked.contains_key(d)).collect();
    let want = tree_entries(ws, tip, &there)?;
    for (p, have) in worktree_entries(ws, source, &there)? {
        if !have.as_ref().is_some_and(|(_, b)| want.get(&p).is_some_and(|(_, t)| t == b)) {
            collisions.push(p);
        }
    }
    collisions.sort();
    Ok(collisions)
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
/// reset the clone to the person's commit ([`discard_target`]), clean. Returns the discarded
/// snapshot.
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
    let at = discard_target(source, clone, workspace_branch)?;
    retain(&snap)?;
    reset_to(clone, workspace_branch, &at, &at)?;
    let ws = Git::open(clone)?;
    let mut c = ws.cmd_write();
    c.args(["update-ref", "-d", "refs/sill/discarding"]);
    let _ = run(c);
    Ok(snap)
}

/// Where Discard puts the clone's branch and files: the person's tip whenever nothing of the
/// workspace's own would be lost there — the workspace has it already (after an Apply or a
/// rebase), or their tip has moved on past the workspace's (their commits since come with it,
/// 2026-11-10); only when the two have diverged, their common ancestor.
fn discard_target(source: &Path, clone: &Path, workspace_branch: &str) -> Result<String> {
    let ws = Git::open(clone)?;
    let tip = rev(&ws, &format!("refs/heads/{workspace_branch}"))?;
    let (_, dest) = super::head_of(&Git::open(source)?)?;
    if super::descends_in(&ws, &dest, &tip)? || super::descends_in(&ws, &tip, &dest)? {
        return Ok(dest);
    }
    let mut c = ws.cmd();
    c.args(["merge-base", &dest, &tip]);
    ok(c, "the common ancestor")
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
