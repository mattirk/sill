//! The session flow of §12 (docs/parallel-coding.md): Apply as a repeatable checkpoint that brings
//! a workspace's commits and uncommitted changes into the person's checkout, Discard back to what
//! was last applied (kept as a snapshot for Undo), and a finished folder reused as a warm spare.
//!
//! A **snapshot** is a commit of a clone's whole state — its HEAD plus every tracked change and
//! every untracked file that is not ignored — made with a private index, so the clone's own index
//! and files are untouched. `refs/sill/applied` in the clone is the snapshot of the last Apply (at
//! creation, of the base): what the person already has. All Git here runs through the hardened
//! view ([`Git`]); nothing in either repository's config runs.
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
    let index = git.dirs().0.join(format!("sill-snapshot-index-{}", std::process::id()));
    let _ = std::fs::remove_file(&index);
    let with = |args: &[&str]| {
        let mut c = git.cmd_write();
        c.env("GIT_INDEX_FILE", &index);
        for (k, v) in IDENT {
            c.env(k, v);
        }
        c.args(args);
        c
    };
    let made = (|| {
        ok(with(&["read-tree", "HEAD"]), "snapshot: read HEAD")?;
        ok(with(&["add", "-A", "--", ":/"]), "snapshot: add")?;
        let tree = ok(with(&["write-tree"]), "snapshot: write tree")?;
        ok(with(&["commit-tree", &tree, "-p", "HEAD", "-m", message]), "snapshot: commit")
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

/// Whether the clone differs from what the person has: its whole state against `refs/sill/applied`.
pub fn unapplied(clone: &Path) -> Result<bool> {
    let git = Git::open(clone)?;
    let now = snapshot(clone, "sill: check")?;
    Ok(rev(&git, &format!("{now}^{{tree}}"))? != rev(&git, &format!("{APPLIED}^{{tree}}"))?)
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

/// The person's checkout's dirty entries: (index status, worktree status, path), untracked as `??`.
fn dirty(src: &Git) -> Result<Vec<(char, char, String)>> {
    let mut c = src.cmd();
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

/// The blob id of `path` in the person's working tree as Git would store it (no filters), or None.
fn worktree_blob(src: &Git, root: &Path, path: &str) -> Result<Option<String>> {
    let full = root.join(path);
    let Ok(meta) = std::fs::symlink_metadata(&full) else { return Ok(None) };
    let mut c = src.cmd();
    if meta.file_type().is_symlink() {
        use std::io::Write;
        let target = std::fs::read_link(&full)?;
        c.args(["hash-object", "--no-filters", "--stdin"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
        let mut child = c.spawn()?;
        child.stdin.take().unwrap().write_all(target.as_os_str().as_encoded_bytes())?;
        let out = child.wait_with_output()?;
        return Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()));
    }
    if !meta.is_file() {
        return Ok(None);
    }
    c.args(["hash-object", "--no-filters", "--"]).arg(path);
    Ok(Some(ok(c, "hash-object")?))
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
    let changed: std::collections::BTreeSet<String> = diff_tree(&ws, expected, &snap)?.into_iter().map(|e| e.path).collect();
    // the person's own dirty files against the agent's earlier Apply
    let (mut ours, mut overlap, mut untracked) = (Vec::new(), Vec::new(), Vec::new());
    for (x, _y, path) in dirty(&src)? {
        let staged = x != ' ' && x != '?';
        let was_applied = try_rev(&ws, &format!("{applied}:{path}"))?;
        let is_ours = !staged && worktree_blob(&src, source, &path)? == was_applied;
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

/// Put the clone into the state of snapshot `snap` (§12 Discard, Undo): its branch at the
/// snapshot's parent, the files as in the snapshot, the snapshot's own changes uncommitted, other
/// untracked files gone, ignored files kept.
pub fn reset_to(clone: &Path, workspace_branch: &str, snap: &str) -> Result<()> {
    let gitdir = clone.join(".git");
    std::fs::write(gitdir.join("HEAD"), format!("ref: refs/heads/{workspace_branch}\n"))?;
    let git = Git::open(clone)?;
    let base = rev(&git, &format!("{snap}^"))?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill: reset"]).arg(format!("refs/heads/{workspace_branch}")).arg(&base);
    ok(c, "reset the branch")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", "--reset", "-u", snap]);
    ok(c, "reset the files")?;
    let mut c = git.cmd_write();
    c.args(["clean", "-fdq"]);
    ok(c, "remove untracked files")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", &base]);
    ok(c, "reset the index")?;
    Ok(())
}

/// Discard (§12): keep the clone's whole state as a snapshot in the source's own store
/// (`refs/sill/workspaces/<id>/discarded/<n>`, for Undo even after the folder is reused), then
/// reset the clone to what was last applied. Returns the discarded snapshot.
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
    let applied = rev(&ws, APPLIED)?;
    drop(ws);
    reset_to(clone, workspace_branch, &applied)?;
    let ws = Git::open(clone)?;
    let mut c = ws.cmd_write();
    c.args(["update-ref", "-d", "refs/sill/discarding"]);
    let _ = run(c);
    Ok(snap)
}

/// Undo a Discard: put snapshot `discarded` back, only while the clone still is what was applied
/// (nothing done since the Discard). `Ok(false)` when it changed.
pub fn undo(clone: &Path, workspace_branch: &str, discarded: &str) -> Result<bool> {
    if unapplied(clone)? {
        return Ok(false);
    }
    reset_to(clone, workspace_branch, discarded)?;
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
