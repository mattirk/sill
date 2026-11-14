//! The session flow of §12 (docs/parallel-coding.md): Apply as a repeatable checkpoint that brings
//! a workspace's commits and uncommitted changes into the person's checkout, Discard back to what
//! was last applied (kept as a snapshot for Undo), and a finished folder reused as a warm spare.
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
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Created, MARKER, Source, Stop, anchor, ok, ok_raw, read_tree_stop, run, slug};
use crate::changes::Git;

/// The snapshot of what the person already has from this workspace.
pub const APPLIED: &str = "refs/sill/applied";

const IDENT: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "sill"),
    ("GIT_AUTHOR_EMAIL", "sill@localhost"),
    ("GIT_COMMITTER_NAME", "sill"),
    ("GIT_COMMITTER_EMAIL", "sill@localhost"),
];

fn rev(git: &Git, what: &str) -> Result<String> {
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
type Have = Option<(String, String)>;

/// The files at `paths` below `root` as git would store them, their blobs written into `store`'s
/// own object store: one `hash-object` for all regular files, read with `root`'s attributes so
/// the person's clean filters run as for their own commits; a link's target is the blob, never
/// followed; nothing (None) where there is no file or link. A file that goes while being read
/// counts as gone.
fn worktree_entries(store: &Git, root: &Path, paths: &[String]) -> Result<Vec<(String, Have)>> {
    use std::os::unix::fs::PermissionsExt;
    let mut out: Vec<(String, Have)> = Vec::with_capacity(paths.len());
    let mut files = Vec::new();
    for p in paths {
        let full = root.join(p);
        let entry = match std::fs::symlink_metadata(&full) {
            Err(_) => None,
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(&full)?;
                Some(("120000".to_string(), hash_stdin(store, target.as_os_str().as_encoded_bytes())?))
            }
            Ok(m) if m.is_file() => {
                files.push(out.len());
                Some(((if m.permissions().mode() & 0o111 != 0 { "100755" } else { "100644" }).to_string(), String::new()))
            }
            Ok(_) => None,
        };
        out.push((p.clone(), entry));
    }
    let hash = |indices: &[usize]| -> Result<Vec<String>> {
        let mut c = store.cmd_write_over(root);
        c.args(["hash-object", "-w", "--"]);
        for &i in indices {
            c.arg(&paths[i]);
        }
        let ids: Vec<String> = ok(c, "hash-object")?.lines().map(str::to_string).collect();
        if ids.len() != indices.len() {
            bail!("hash-object: {} ids for {} files", ids.len(), indices.len());
        }
        Ok(ids)
    };
    for chunk in files.chunks(256) {
        let ids = match hash(chunk) {
            Ok(ids) => ids.into_iter().map(Some).collect::<Vec<_>>(),
            // one of them went: read them one by one
            Err(_) => chunk.iter().map(|&i| hash(&[i]).ok().map(|mut v| v.remove(0))).collect(),
        };
        for (&i, id) in chunk.iter().zip(ids) {
            out[i].1 = id.map(|id| (out[i].1.take().unwrap().0, id));
        }
    }
    Ok(out)
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
fn build_tree(git: &Git, base: &str, entries: &[(String, Have)]) -> Result<String> {
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
    let unstaged: Vec<String> =
        dirty(&src)?.into_iter().filter(|(x, _, p)| (*x == ' ' || *x == '?') && !paths.contains(p)).map(|(_, _, p)| p).collect();
    let mut host: BTreeMap<String, Have> =
        worktree_entries(&ws, source, &paths.iter().chain(&unstaged).cloned().collect::<Vec<_>>())?.into_iter().collect();
    let was = tree_entries(&ws, &applied, &unstaged)?;
    for p in unstaged {
        if same_blob(host.get(&p), was.get(&p)) {
            paths.push(p);
        }
    }
    // what the checkout has on them, where that differs from the workspace
    let ours = tree_entries(&ws, &now, &paths)?;
    let entries: Vec<(String, Have)> = paths
        .into_iter()
        .filter_map(|p| {
            let theirs = host.remove(&p).flatten();
            (theirs.as_ref() != ours.get(&p)).then_some((p, theirs))
        })
        .collect();
    let tree = build_tree(&ws, &now, &entries)?;
    Ok(Mirror { changed: tree != now, tree, upstream, now })
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
struct Entry {
    mode: String,
    blob: Option<String>,
    path: String,
}

fn diff_tree(git: &Git, from: &str, to: &str) -> Result<Vec<Entry>> {
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
fn dirty(git: &Git) -> Result<Vec<(char, char, String)>> {
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
fn write_entry(git: &Git, root: &Path, path: &str, mode: &str, blob: &str) -> Result<()> {
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

fn remove_entry(root: &Path, path: &str) -> Result<()> {
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
    for e in diff_tree(&ws, &tip, &snap)? {
        match &e.blob {
            Some(blob) => write_entry(&ws, source, &e.path, &e.mode, blob)?,
            None => remove_entry(source, &e.path)?,
        }
    }
    Ok(Ok(Applied { before: expected.to_string(), tip, snapshot: snap }))
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
/// paths, the branch at their tip (after a rebase, still on their commit). Returns the discarded
/// snapshot.
pub fn discard(source: &Path, clone: &Path, id: &str, n: &str, workspace_branch: &str) -> Result<String> {
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
        .arg(format!("+refs/sill/discarding:{}", super::anchor_ref(id, &format!("discarded/{n}"))));
    ok(c, "keep the discarded work")?;
    drop(ws);
    let m = mirror(source, clone, workspace_branch)?;
    reset_to(clone, workspace_branch, &m.tree, &m.upstream)?;
    let ws = Git::open(clone)?;
    let mut c = ws.cmd_write();
    c.args(["update-ref", "-d", "refs/sill/discarding"]);
    let _ = run(c);
    Ok(snap)
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

/// A fresh repository's config as `git init` writes it for this object format.
fn fresh_config(object_format: &str) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("sill-fresh-{}-{}", std::process::id(), crate::store::new_id()));
    let mut c = std::process::Command::new("git");
    c.args(["init", "-q", "--template="]).arg(format!("--object-format={object_format}")).arg(&dir).env("GIT_CONFIG_NOSYSTEM", "1");
    let made = ok(c, "init").and_then(|_| Ok(std::fs::read_to_string(dir.join(".git/config"))?));
    let _ = std::fs::remove_dir_all(&dir);
    made
}

/// Make a finished folder `spare` the new workspace `id` at `path` (§12 spare): renamed, its Git
/// settings reset to a fresh repository's — config, hooks, `info/`, refs, reflogs and any
/// stopped operation go — so nothing one session planted runs in the next; its objects stay and
/// it borrows the source's store; tracked files reset to the source's HEAD on a new branch,
/// untracked files removed, **ignored files kept** (the warm build output).
pub fn reuse(spare: &Path, source: &Source, id: &str, name: &str, path: &Path, operation: &str) -> Result<Created> {
    if path.exists() && path != spare {
        bail!("{} already exists", path.display());
    }
    anchor(&source.root, id, "base", &source.head)?;
    if path != spare {
        std::fs::create_dir_all(path.parent().context("no parent")?)?;
        std::fs::rename(spare, path).with_context(|| format!("move {}", spare.display()))?;
    }
    let gitdir = path.join(".git");
    let config = fresh_config(&source.object_format)?;
    for gone in ["hooks", "info", "logs", "refs", "rebase-merge", "rebase-apply", "sequencer"] {
        let p = gitdir.join(gone);
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
    }
    for gone in ["packed-refs", "MERGE_HEAD", "MERGE_MSG", "CHERRY_PICK_HEAD", "REVERT_HEAD", "ORIG_HEAD", "FETCH_HEAD", "index.lock", "shallow"] {
        let _ = std::fs::remove_file(gitdir.join(gone));
    }
    std::fs::write(gitdir.join("config"), config)?;
    for dir in ["refs/heads", "refs/tags", "info"] {
        std::fs::create_dir_all(gitdir.join(dir))?;
    }
    std::fs::write(gitdir.join(MARKER), operation)?;
    let objects = Git::open(&source.root)?.objects().canonicalize()?;
    std::fs::create_dir_all(gitdir.join("objects/info"))?;
    std::fs::write(gitdir.join("objects/info/alternates"), format!("{}\n", objects.display()))?;
    let branch = format!("sill/{}-{id}", slug(name));
    std::fs::write(gitdir.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
    let git = Git::open(path)?;
    for (r, v) in [(format!("refs/heads/{branch}"), &source.head), ("refs/sill/base".to_string(), &source.head)] {
        let mut c = git.cmd_write();
        c.args(["update-ref", &r, v]);
        ok(c, "set refs")?;
    }
    let mut c = git.cmd_write();
    c.args(["read-tree", "--reset", "-u", &source.head]);
    ok(c, "reset the files")?;
    let mut c = git.cmd_write();
    c.args(["clean", "-fdq"]);
    ok(c, "remove untracked files")?;
    drop(git);
    set_applied(path, &snapshot(path, "sill: base")?)?;
    Ok(Created { path: path.to_path_buf(), branch, base: source.head.clone() })
}

/// The folder kept as a repository's spare, if any: `<root>/.spare/<repository key>`.
pub fn spare_path(root: &Path, source: &Path) -> PathBuf {
    use sha2::Digest;
    let key: String = sha2::Sha256::digest(source.as_os_str().as_encoded_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    root.join(".spare").join(key)
}

#[cfg(test)]
#[path = "flow_tests.rs"]
mod tests;
