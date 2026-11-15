//! Workspaces: a session's own clone and branch of a repository (docs/parallel-coding.md).
//!
//! This module is the Git side only: checking a source, making the clone, the anchors that keep
//! its borrowed objects alive, the person's fast-forward Merge, and retaining a discarded clone.
//! The daemon owns the records, the journal and the jail; nothing here starts an agent.
//!
//! Every Git command on a repository an agent can write runs through the hardened view
//! ([`crate::changes::Git`]): nothing in a repository's config can make it run a program. The
//! clone borrows the source's objects through `objects/info/alternates`, naming the real object
//! store; the source gets `refs/sill/workspaces/<id>/…` anchors before anything is borrowed.
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::changes::Git;

/// Why a source cannot have a workspace in the first version (§10 item 9, §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotARepository,
    NoCommits,
    LinkedWorktree,
    Submodules,
    PartialClone,
    Lfs,
    FilterDrivers,
    BorrowedObjects,
    DetachedHead,
    RefFormat,
}

impl Refusal {
    /// The stable `details.reason` string.
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::NotARepository => "not_a_repository",
            Refusal::NoCommits => "no_commits",
            Refusal::LinkedWorktree => "linked_worktree",
            Refusal::Submodules => "submodules",
            Refusal::PartialClone => "partial_clone",
            Refusal::Lfs => "lfs",
            Refusal::FilterDrivers => "filter_drivers",
            Refusal::BorrowedObjects => "borrowed_objects",
            Refusal::DetachedHead => "detached_head",
            Refusal::RefFormat => "ref_format",
        }
    }

    /// What the person reads.
    pub fn message(self) -> &'static str {
        match self {
            Refusal::NotARepository => "This folder is not the top of a Git repository.",
            Refusal::NoCommits => "The repository has no commits yet.",
            Refusal::LinkedWorktree => "Linked worktrees are not supported; choose the main checkout.",
            Refusal::Submodules => "Repositories with submodules are not supported yet.",
            Refusal::PartialClone => "Partial clones are not supported: fetch the full history first.",
            Refusal::Lfs => "Repositories using Git LFS are not supported yet.",
            Refusal::FilterDrivers => "The repository uses its own Git filter drivers, which a workspace cannot apply.",
            Refusal::BorrowedObjects => "The repository already borrows objects from another (alternates).",
            Refusal::DetachedHead => "Check out a branch in the source first (HEAD is detached).",
            Refusal::RefFormat => "The reftable ref format is not supported yet.",
        }
    }
}

/// A source that can have a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The repository's top, canonical.
    pub root: PathBuf,
    /// The checked-out branch, without `refs/heads/`.
    pub branch: String,
    pub head: String,
    pub object_format: String,
    /// History is cut off: a rebase past the boundary stops (§10 item 9).
    pub shallow: bool,
    /// Uncommitted work the workspace will not see (from HEAD only, §10 item 7).
    pub dirty: bool,
}

fn run(mut cmd: std::process::Command) -> Result<Output> {
    cmd.env("LC_ALL", "C");
    let out = cmd.output().context("run git")?;
    Ok(out)
}

fn ok(cmd: std::process::Command, what: &str) -> Result<String> {
    Ok(ok_raw(cmd, what)?.trim().to_string())
}

/// As [`ok`], the output untrimmed: porcelain formats start entries with a space.
fn ok_raw(cmd: std::process::Command, what: &str) -> Result<String> {
    let out = run(cmd)?;
    if !out.status.success() {
        bail!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A repository config file as `key=value` pairs, read by git without running anything.
fn config_file(path: &Path) -> Vec<(String, String)> {
    let mut c = std::process::Command::new("git");
    c.arg("config").arg("--file").arg(path).args(["--list", "-z"]).env("GIT_CONFIG_NOSYSTEM", "1");
    let Ok(out) = run(c) else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter_map(|e| e.split_once('\n').map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())))
        .collect()
}

/// Filter names `.gitattributes` files at HEAD and `info/attributes` ask for.
fn attribute_filters(git: &Git, gitdir: &Path) -> Result<Vec<String>> {
    let mut texts = Vec::new();
    let mut c = git.cmd();
    c.args(["ls-tree", "-r", "-z", "--name-only", "HEAD"]);
    for path in ok(c, "ls-tree")?.split('\0').filter(|p| p == &".gitattributes" || p.ends_with("/.gitattributes")) {
        let mut c = git.cmd();
        c.arg("cat-file").arg("blob").arg(format!("HEAD:{path}"));
        texts.push(ok(c, "cat-file")?);
    }
    texts.push(std::fs::read_to_string(gitdir.join("info/attributes")).unwrap_or_default());
    let mut names: Vec<String> = texts
        .iter()
        .flat_map(|t| t.lines())
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .filter_map(|w| w.strip_prefix("filter=").map(str::to_string))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Check `path` can have a workspace: `Ok(Err(refusal))` names why not.
pub fn inspect(path: &Path) -> Result<std::result::Result<Source, Refusal>> {
    let Ok(root) = path.canonicalize() else { return Ok(Err(Refusal::NotARepository)) };
    let dot = root.join(".git");
    if dot.is_file() {
        return Ok(Err(Refusal::LinkedWorktree));
    }
    if !dot.join("HEAD").is_file() || !dot.join("objects").is_dir() {
        return Ok(Err(Refusal::NotARepository));
    }
    let config = config_file(&dot.join("config"));
    let get = |key: &str| config.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    if get("extensions.refstorage").is_some_and(|v| v != "files") {
        return Ok(Err(Refusal::RefFormat));
    }
    if get("extensions.partialclone").is_some()
        || config.iter().any(|(k, v)| {
            (k.starts_with("remote.") && k.ends_with(".promisor") && v == "true")
                || (k.starts_with("remote.") && k.ends_with(".partialclonefilter"))
        })
    {
        return Ok(Err(Refusal::PartialClone));
    }
    if std::fs::read_to_string(dot.join("objects/info/alternates")).is_ok_and(|t| !t.trim().is_empty()) {
        return Ok(Err(Refusal::BorrowedObjects));
    }
    let head_file = std::fs::read_to_string(dot.join("HEAD"))?;
    let Some(branch) = head_file.trim().strip_prefix("ref: refs/heads/").map(str::to_string) else {
        return Ok(Err(Refusal::DetachedHead));
    };
    let git = Git::open(&root)?;
    let mut c = git.cmd();
    c.args(["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
    let Ok(head) = ok(c, "rev-parse") else { return Ok(Err(Refusal::NoCommits)) };
    let mut c = git.cmd();
    c.args(["cat-file", "-e", "HEAD:.gitmodules"]);
    if run(c)?.status.success() || dot.join("modules").exists() {
        return Ok(Err(Refusal::Submodules));
    }
    let filters = attribute_filters(&git, &dot)?;
    if filters.iter().any(|f| f == "lfs") || dot.join("lfs").is_dir() {
        return Ok(Err(Refusal::Lfs));
    }
    // every file of the person's own config as git reads it here, keys in lower case: a filter
    // driver defined there applies too
    let global: Vec<(String, String)> = git.config().into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    let defined = |name: &str| {
        let prefix = format!("filter.{}.", name.to_ascii_lowercase());
        config.iter().chain(global.iter()).any(|(k, _)| {
            k.starts_with(&prefix) && [".clean", ".smudge", ".process"].iter().any(|s| k.ends_with(s))
        })
    };
    if filters.iter().any(|f| defined(f)) {
        return Ok(Err(Refusal::FilterDrivers));
    }
    let mut c = git.cmd();
    c.args(["status", "--porcelain", "-z", "--untracked-files=normal", "--ignore-submodules=all"]);
    let dirty = !ok(c, "status")?.is_empty();
    Ok(Ok(Source {
        root,
        branch,
        head,
        object_format: get("extensions.objectformat").unwrap_or("sha1").to_string(),
        shallow: dot.join("shallow").is_file(),
        dirty,
    }))
}

/// A workspace name as the branch and directory carry it: lowercase letters, digits and `-`.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s: String = s.trim_end_matches('-').chars().take(40).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "work".into() } else { s }
}

/// Where a workspace's clone goes: `<root>/<repository name>/<slug>-<id>`.
pub fn path_for(root: &Path, source: &Source, id: &str, name: &str) -> PathBuf {
    let repo = source.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    root.join(repo).join(format!("{}-{id}", slug(name)))
}

/// The ref every source keeps for a workspace's borrowed objects.
pub fn anchor_ref(id: &str, name: &str) -> String {
    format!("refs/sill/workspaces/{id}/{name}")
}

/// Point `refs/sill/workspaces/<id>/<name>` in `source` at `oid` (§4: before anything is borrowed).
pub fn anchor(source: &Path, id: &str, name: &str, oid: &str) -> Result<()> {
    let git = Git::open(source)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill workspace anchor"]).arg(anchor_ref(id, name)).arg(oid);
    ok(c, "anchor").map(drop)
}

/// Remove every anchor of workspace `id` from `source`.
pub fn drop_anchors(source: &Path, id: &str) -> Result<()> {
    let git = Git::open(source)?;
    let mut c = git.cmd();
    c.args(["for-each-ref", "--format=%(refname)"]).arg(format!("refs/sill/workspaces/{id}/"));
    for name in ok(c, "for-each-ref")?.lines().filter(|l| !l.is_empty()) {
        let mut c = git.cmd_write();
        c.args(["update-ref", "-d", name]);
        ok(c, "drop anchor")?;
    }
    Ok(())
}

/// The file in a clone's git directory naming the operation that made it.
const MARKER: &str = "sill-workspace";

/// The operation that made the clone at `path`, if sill made it.
pub fn marker(path: &Path) -> Option<String> {
    std::fs::read_to_string(path.join(".git").join(MARKER)).ok().map(|s| s.trim().to_string())
}

/// A created workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

/// Make the clone at `path` for workspace `id`, from `source`'s HEAD: anchor the base, make an
/// empty repository borrowing the source's real object store, check out a new branch at the base.
/// The source is read only through the hardened view and gets nothing but the anchor. `path` must
/// not exist. Nothing of a failed attempt is left but what [`remove_partial`] removes.
///
/// The folder starts **warm** where the filesystem can share file data (§12): the checkout's
/// working tree — its ignored build output with it — is taken as it is, then brought to the base.
/// Unreadable entries are skipped. Where it cannot share, or the checkout root cannot be read,
/// the folder starts cold: tracked files only.
pub fn create(source: &Source, id: &str, name: &str, path: &Path, operation: &str) -> Result<Created> {
    create_as(source, id, name, path, operation, seed::How::Share)
}

fn create_as(source: &Source, id: &str, name: &str, path: &Path, operation: &str, how: seed::How) -> Result<Created> {
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    anchor(&source.root, id, "base", &source.head)?;
    let objects = Git::open(&source.root)?.objects().canonicalize()?;
    std::fs::create_dir_all(path.parent().context("workspace path has no parent")?)?;
    // the marker first: whatever is left of an interrupted attempt is known to be this one's
    std::fs::create_dir_all(path.join(".git"))?;
    std::fs::write(path.join(".git").join(MARKER), operation)?;
    let warm = match seed::copy(&source.root, path, how) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(path = %source.root.display(), "workspace starts cold: {error:#}");
            false
        }
    };
    if !warm {
        // nothing of a copy that stopped half way stays
        remove_folder(path)?;
        std::fs::create_dir_all(path.join(".git"))?;
        std::fs::write(path.join(".git").join(MARKER), operation)?;
    }
    let mut c = std::process::Command::new("git");
    c.args(["init", "-q", "--template="])
        .arg(format!("--object-format={}", source.object_format))
        .arg(path)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    ok(c, "init")?;
    std::fs::create_dir_all(path.join(".git/objects/info"))?;
    std::fs::write(path.join(".git/objects/info/alternates"), format!("{}\n", objects.display()))?;
    let branch = format!("sill/{}-{id}", slug(name));
    let git = Git::open(path)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill workspace base"]).arg("refs/sill/base").arg(&source.head);
    ok(c, "base ref")?;
    if warm {
        to_base(&git, path, &branch, &source.head)?;
    }
    drop(git);
    if !warm {
        // a fresh repository with sill's own config: check out on it directly, hooks and fsmonitor off
        let mut c = std::process::Command::new("git");
        c.arg("-C").arg(path)
            .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-c", "advice.detachedHead=false"])
            .args(["checkout", "-q", "-b", &branch, &source.head])
            .env("GIT_CONFIG_NOSYSTEM", "1");
        ok(c, "checkout")?;
    }
    let git = Git::open(path)?;
    let mut c = git.cmd();
    c.arg("rev-parse").arg("--verify").arg(format!("{branch}^{{tree}}"));
    ok(c, "verify the clone")?;
    drop(git);
    Ok(Created { path: path.to_path_buf(), branch, base: source.head.clone() })
}

/// Bring a folder holding the checkout's files to `head` on the new `branch`: the person's
/// uncommitted edits and deletions go back to the base, their untracked files go, **ignored
/// files stay** (the warm build output). A file that already is as the base has it is not
/// written, so it keeps the checkout's time and build tools see it as unchanged.
fn to_base(git: &Git, path: &Path, branch: &str, head: &str) -> Result<()> {
    std::fs::write(path.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n"))?;
    let mut c = git.cmd_write();
    c.args(["update-ref", &format!("refs/heads/{branch}"), head]);
    ok(c, "set the branch")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", head]);
    ok(c, "read the base")?;
    // which files are the base's already: fails where one is not, and that is the answer
    let mut c = git.cmd_write();
    c.args(["update-index", "-q", "--refresh"]);
    let _ = run(c);
    let mut c = git.cmd_write();
    c.args(["read-tree", "--reset", "-u", head]);
    ok(c, "reset the files")?;
    // twice forced: a repository nested in the checkout, not ignored, goes with the rest
    let mut c = git.cmd_write();
    c.args(["clean", "-ffdq"]);
    ok(c, "remove untracked files")?;
    Ok(())
}

/// Remove a folder sill made, also where a tool left folders it may not write (a module cache).
pub(crate) fn remove_folder(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    let mut dirs = vec![path.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(&dir)?.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(entry.path());
            }
        }
    }
    Ok(std::fs::remove_dir_all(path)?)
}

/// Remove what an unfinished [`create`] by `operation` left at `path`. Refuses a directory whose
/// marker names another operation, or none: sill never removes what it did not make.
pub fn remove_partial(path: &Path, operation: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if marker(path).as_deref() != Some(operation) {
        bail!("{} was not made by this operation; left in place", path.display());
    }
    remove_folder(path)?;
    Ok(true)
}

/// One path of a result: how it differs and by how many lines. A rename is its two paths, one
/// deleted and one added, so that each path's own diff says the same as its line here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Relative to the repository.
    pub path: PathBuf,
    /// `A` added, `M` modified, `D` deleted, `T` type changed.
    pub status: char,
    /// Lines; both 0 for a binary file.
    pub added: u64,
    pub removed: u64,
}

/// The paths that differ from tree-ish `from` to `to` (the working tree when None).
fn changed_files(git: &Git, from: &str, to: Option<&str>) -> Result<Vec<FileChange>> {
    use std::os::unix::ffi::OsStrExt;
    let diff = |format: &str| -> Result<Vec<u8>> {
        let mut c = git.cmd();
        c.args(["diff", format, "-z", "--no-renames"]).args(Git::DIFF_SAFE).arg(from);
        if let Some(to) = to {
            c.arg(to);
        }
        c.arg("--");
        let out = run(c)?;
        if !out.status.success() {
            bail!("diff {format}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    };
    let path = |bytes: &[u8]| PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
    let mut lines: std::collections::BTreeMap<PathBuf, (u64, u64)> = Default::default();
    for e in diff("--numstat")?.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let mut parts = e.splitn(3, |b| *b == b'\t');
        let count = |part: Option<&[u8]>| part.and_then(|n| std::str::from_utf8(n).ok()).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        let (added, removed) = (count(parts.next()), count(parts.next()));
        if let Some(p) = parts.next() {
            lines.insert(path(p), (added, removed));
        }
    }
    let status = diff("--name-status")?;
    let mut fields = status.split(|b| *b == 0);
    let mut changed = Vec::new();
    while let (Some(code), Some(p)) = (fields.next(), fields.next()) {
        let Some(code) = code.first() else { continue };
        let p = path(p);
        let (added, removed) = lines.get(&p).copied().unwrap_or_default();
        changed.push(FileChange { path: p, status: *code as char, added, removed });
    }
    changed.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changed)
}

/// What the person judges before Merge or Discard (§6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub commits: u32,
    pub files: u32,
    pub added: u64,
    pub removed: u64,
    /// The paths that differ, sorted: `files`, `added` and `removed` are their count and sums.
    pub changed: Vec<FileChange>,
    /// Uncommitted paths, untracked included.
    pub uncommitted: Vec<String>,
    /// Ignored paths: outside the result, but deleted with the clone.
    pub ignored: Vec<String>,
}

fn status_paths(git: &Git, ignored: bool) -> Result<Vec<String>> {
    let mut c = git.cmd();
    c.args(["status", "--porcelain", "-z", "--untracked-files=all", "--ignore-submodules=all"]);
    if ignored {
        c.arg("--ignored=matching");
    }
    let out = ok_raw(c, "status")?;
    let mut paths = Vec::new();
    let mut entries = out.split('\0');
    while let Some(e) = entries.next() {
        if e.len() < 4 {
            continue;
        }
        let (code, path) = e.split_at(3);
        if code.starts_with('R') || code.starts_with('C') {
            entries.next();
        }
        if (code == "!! ") == ignored {
            paths.push(path.to_string());
        }
    }
    Ok(paths)
}

/// The clone's result against `base`.
pub fn summary(clone: &Path, branch: &str, base: &str) -> Result<Summary> {
    summary_to(clone, branch, base, base, None)
}

/// The branch's commits since `commits_from`, and the changes from tree-ish `from` to `to` (the
/// working tree when None): against a full snapshot, untracked files count.
fn summary_to(clone: &Path, branch: &str, commits_from: &str, from: &str, to: Option<&str>) -> Result<Summary> {
    let git = Git::open(clone)?;
    let mut c = git.cmd();
    c.args(["rev-list", "--count"]).arg(format!("{commits_from}..refs/heads/{branch}"));
    let commits = ok(c, "rev-list")?.parse().unwrap_or(0);
    let changed = changed_files(&git, from, to)?;
    Ok(Summary {
        commits,
        files: changed.len() as u32,
        added: changed.iter().map(|f| f.added).sum(),
        removed: changed.iter().map(|f| f.removed).sum(),
        changed,
        uncommitted: status_paths(&git, false)?,
        ignored: status_paths(&git, true)?,
    })
}

/// The revision the person sees (§11): the branch tip and a fingerprint of the uncommitted state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub tip: String,
    /// Empty when the working tree is clean.
    pub worktree: String,
}

pub fn revision(clone: &Path, branch: &str) -> Result<Revision> {
    let git = Git::open(clone)?;
    let mut c = git.cmd();
    c.arg("rev-parse").arg(format!("refs/heads/{branch}"));
    let tip = ok(c, "rev-parse")?;
    let uncommitted = status_paths(&git, false)?;
    if uncommitted.is_empty() {
        return Ok(Revision { tip, worktree: String::new() });
    }
    let mut hash = Sha256::new();
    let mut c = git.cmd();
    c.args(["diff", "--binary"]).args(Git::DIFF_SAFE).arg("HEAD");
    hash.update(run(c)?.stdout);
    let mut c = git.cmd();
    c.args(["ls-files", "-z", "--others", "--exclude-standard"]);
    for path in ok(c, "ls-files")?.split('\0').filter(|p| !p.is_empty()) {
        hash.update(path.as_bytes());
        hash.update([0]);
        match std::fs::symlink_metadata(clone.join(path)) {
            Ok(m) if m.is_file() => hash.update(std::fs::read(clone.join(path)).unwrap_or_default()),
            Ok(m) if m.is_symlink() => hash.update(std::fs::read_link(clone.join(path)).unwrap_or_default().as_os_str().as_encoded_bytes()),
            _ => {}
        }
        hash.update([0]);
    }
    let worktree = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(Revision { tip, worktree })
}

/// The source's branch tip, anchored and made visible in the clone as `refs/sill/dest` (§11
/// prepare): its objects are already borrowed, so the clone needs no fetch to rebase onto it.
/// The clone's tip before a rebase is kept as `refs/sill/pre-rebase`.
pub fn set_destination(source: &Path, clone: &Path, id: &str, branch: &str, workspace_branch: &str) -> Result<String> {
    let src = Git::open(source)?;
    let mut c = src.cmd();
    c.arg("rev-parse").arg(format!("refs/heads/{branch}"));
    let tip = ok(c, "destination tip")?;
    anchor(source, id, "dest", &tip)?;
    let git = Git::open(clone)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "refs/sill/dest", &tip]);
    ok(c, "destination ref")?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "refs/sill/pre-rebase"]).arg(format!("refs/heads/{workspace_branch}"));
    ok(c, "pre-rebase ref")?;
    Ok(tip)
}

/// The branch the clone's work is on: the one its HEAD names — an agent that made or switched to
/// a branch of its own is followed there (2026-11-12), so the summary, the uncommitted check and
/// Apply all read the same commits — or, while a rebase is stopped, the branch being rebased.
/// None when HEAD is detached otherwise, or unreadable.
pub fn clone_branch(clone: &Path) -> Option<String> {
    let dot = clone.join(".git");
    // the agent writes this folder: only a regular file is read
    let text = |p: PathBuf| std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()).then(|| std::fs::read_to_string(p).ok()).flatten();
    let named = |t: String| t.trim().strip_prefix("refs/heads/").filter(|b| !b.is_empty()).map(str::to_string);
    if let Some(b) = text(dot.join("HEAD")).and_then(|t| named(t.trim().strip_prefix("ref: ").unwrap_or("").to_string())) {
        return Some(b);
    }
    ["rebase-merge", "rebase-apply"].iter().find_map(|d| text(dot.join(d).join("head-name")).and_then(named))
}

/// The source's checked-out branch (empty when detached) and its tip.
pub fn source_head(source: &Path) -> Result<(String, String)> {
    head_of(&Git::open(source)?)
}

pub(crate) fn head_of(git: &Git) -> Result<(String, String)> {
    let head = std::fs::read_to_string(git.dirs().0.join("HEAD"))?;
    let branch = head.trim().strip_prefix("ref: refs/heads/").unwrap_or("").to_string();
    let mut c = git.cmd();
    c.args(["rev-parse", "HEAD"]);
    Ok((branch, ok(c, "source HEAD")?))
}

/// Whether `ancestor` is reachable from `tip` in the clone (borrowed objects included).
pub fn descends(clone: &Path, ancestor: &str, tip: &str) -> Result<bool> {
    descends_in(&Git::open(clone)?, ancestor, tip)
}

pub(crate) fn descends_in(git: &Git, ancestor: &str, tip: &str) -> Result<bool> {
    let mut c = git.cmd();
    c.args(["merge-base", "--is-ancestor", ancestor, tip]);
    Ok(run(c)?.status.success())
}

/// The clone's working tree against `commit` (its base, or a destination it borrows), at most
/// `limit` bytes; true when cut there.
pub fn diff(clone: &Path, commit: &str, limit: usize) -> Result<(String, bool)> {
    diff_to(clone, commit, None, None, limit)
}

/// `only`: that path's diff alone (relative to the repository; a rename is then its two halves,
/// as [`FileChange`] lists it).
fn diff_to(clone: &Path, commit: &str, target: Option<&str>, only: Option<&Path>, limit: usize) -> Result<(String, bool)> {
    let git = Git::open(clone)?;
    let mut c = git.cmd();
    c.args(["diff"]).args(Git::DIFF_SAFE);
    if only.is_some() {
        c.arg("--no-renames");
    }
    c.arg(commit);
    if let Some(target) = target {
        c.arg(target);
    }
    c.arg("--");
    if let Some(only) = only {
        // the path itself, never a pattern
        c.env("GIT_LITERAL_PATHSPECS", "1").arg(only);
    }
    let out = run(c)?;
    if !out.status.success() {
        bail!("diff: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.len() <= limit {
        return Ok((text, false));
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    Ok((text, true))
}

/// Why a Merge did not apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The source's branch is not where the person reviewed it, or the source left the branch.
    SourceMoved,
    /// The result does not descend from the source's tip: prepare (rebase) first.
    NeedsPrepare,
    /// The person's uncommitted edits touch files the result changes.
    Overlap(Vec<String>),
    /// Untracked files in the source would be overwritten.
    Untracked(Vec<String>),
    /// The workspace holds work that is not committed: Apply brings commits only.
    Uncommitted,
}

impl Stop {
    pub fn reason(&self) -> &'static str {
        match self {
            Stop::SourceMoved => "source_moved",
            Stop::NeedsPrepare => "stale_revision",
            Stop::Overlap(_) => "source_overlap",
            Stop::Untracked(_) => "untracked_collision",
            Stop::Uncommitted => "uncommitted",
        }
    }
    pub fn paths(&self) -> &[String] {
        match self {
            Stop::Overlap(p) | Stop::Untracked(p) => p,
            _ => &[],
        }
    }
}

/// `read-tree`'s refusal, one `error: … 'path' …` line per path (`LC_ALL=C`):
/// `Entry 'f' not uptodate. Cannot merge.`, `Entry 'f' would be overwritten by merge.` or
/// `Untracked working tree file 'f' would be overwritten by merge.`
fn read_tree_stop(stderr: &str) -> Stop {
    let quoted = |l: &str| Some(l[l.find('\'')? + 1..l.rfind('\'')?].to_string()).filter(|p| !p.is_empty());
    let (mut untracked, mut overlap) = (Vec::new(), Vec::new());
    for line in stderr.lines().filter(|l| l.starts_with("error: ")) {
        if let Some(path) = quoted(line) {
            if line.contains("Untracked working tree file") { untracked.push(path) } else { overlap.push(path) }
        }
    }
    if overlap.is_empty() && !untracked.is_empty() { Stop::Untracked(untracked) } else { Stop::Overlap(overlap) }
}

pub mod flow;
pub mod records;
mod carry;
mod seed;

#[cfg(test)]
mod git_tests;
