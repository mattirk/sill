//! Work from before the pin (docs/parallel-coding.md §12): a session may write below a repository
//! before the person pins it — the writes land in the session's overlay, over the checkout. Once
//! pinned, the workspace's clone is bound at that path and hides them: the agent no longer sees its
//! work, and the overlay entries are not the session's changes against the host any more. At the
//! jail start that binds the clone (the jail is down: the overlay is not mounted, so its upper tree
//! may be written), they are **carried** into the clone and leave the overlay.
//!
//! The overlay holds whole files, not changes: what the agent started from is not kept. Its base
//! is taken as the checkout's HEAD when the agent first wrote there (the earliest write below the
//! repository, looked up in the checkout's reflog; never before the session began). The agent's
//! files make a commit on that base, merged into the workspace's state (`merge-tree`): clean, the
//! result is written into the clone, uncommitted, as if the agent had made it there; in conflict
//! it is rebased in the helper jail and stops there like an Apply's rebase, for Resolve. Without a
//! base in the reflog, the workspace's own HEAD is the base: the files are taken as they are.
//!
//! Not carried, and left in the overlay: anything below a `.git` (the clone has its own), and paths
//! the repository ignores (build output: the clone has the checkout's own).
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Result, bail};

use super::flow::{self, Have, IDENT};
use super::{ok, ok_raw, run};
use crate::changes::Git;

/// What the overlay holds below a repository, ready to merge.
#[derive(Debug, Clone)]
pub struct Pending {
    /// The repository's place in the session's upper tree.
    pub upper: PathBuf,
    /// Repository-relative paths carried: files, links and deletions (whiteouts).
    pub paths: Vec<String>,
    /// Directories the agent replaced wholesale (opaque in the upper tree).
    pub opaque: Vec<String>,
    /// The agent's work as a commit on `base`.
    pub commit: String,
    pub base: String,
    /// The base came from the checkout's reflog (false: the workspace's HEAD, files taken as they are).
    pub from_reflog: bool,
}

/// How the agent's work meets the workspace.
#[derive(Debug, Clone)]
pub enum Merge {
    /// The workspace's state (`ours`, a commit) and the merged tree.
    Clean { ours: String, tree: String },
    /// The files both changed differently.
    Conflict(Vec<String>),
}

/// The entries of `upper` (the repository's place in an upper tree) worth carrying, unfiltered:
/// repository-relative paths of files, links and whiteouts, the opaque directories, and the
/// earliest write among them.
fn walk(upper: &Path) -> Result<(Vec<String>, Vec<String>, Option<SystemTime>)> {
    let (mut paths, mut opaque, mut earliest) = (Vec::new(), Vec::new(), None::<SystemTime>);
    let mut dirs = vec![PathBuf::new()];
    while let Some(rel) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(upper.join(&rel)) else { continue };
        for e in rd.flatten() {
            if e.file_name() == ".git" {
                continue;
            }
            let rel = rel.join(e.file_name());
            let Some(name) = rel.to_str().map(str::to_string) else { continue };
            let Ok(md) = e.path().symlink_metadata() else { continue };
            let ft = md.file_type();
            let whiteout = ft.is_char_device() && md.rdev() == 0;
            if ft.is_dir() {
                if crate::ledger::is_opaque(&e.path()) {
                    opaque.push(name);
                }
                dirs.push(rel);
                continue;
            }
            if !(ft.is_file() || ft.is_symlink() || whiteout) {
                continue;
            }
            if let Ok(t) = md.modified() {
                earliest = Some(earliest.map_or(t, |e| e.min(t)));
            }
            paths.push(name);
        }
    }
    paths.sort();
    opaque.sort();
    Ok((paths, opaque, earliest))
}

/// Of `paths`, those the clone's repository ignores and does not track.
fn ignored(clone: &Git, paths: &[String]) -> Result<Vec<String>> {
    use std::io::Write;
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut c = clone.cmd();
    c.args(["check-ignore", "-z", "--stdin"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0])).collect();
    // git answers as it reads: written from a thread so neither side waits on a full pipe (a
    // build's output in the overlay is thousands of ignored paths, far more than a pipe holds)
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    // 1: none ignored
    if !out.status.success() && out.status.code() != Some(1) {
        bail!("check-ignore: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
}

/// The checkout's HEAD at `when` (unix seconds), from its reflog: the newest entry not after it.
fn head_at(source: &Path, when: i64) -> Result<Option<String>> {
    let git = Git::open(source)?;
    let mut c = git.cmd();
    c.args(["log", "-g", "--date=unix", "--format=%H %gd", "HEAD", "--"]);
    let out = run(c)?;
    if !out.status.success() {
        return Ok(None);
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // `<oid> HEAD@{<unix>}`
        let Some((oid, at)) = line.split_once(' ') else { continue };
        let Some(t) = at.rsplit_once('{').and_then(|(_, t)| t.strip_suffix('}')).and_then(|t| t.parse::<i64>().ok()) else { continue };
        if t <= when {
            return Ok(Some(oid.to_string()));
        }
    }
    Ok(None)
}

/// Paths `commit`'s tree has below any of `dirs` (each a directory there).
fn below(git: &Git, commit: &str, dirs: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for chunk in dirs.chunks(256) {
        let mut c = git.cmd();
        c.args(["ls-tree", "-r", "-z", "--name-only", commit, "--"]);
        for d in chunk {
            c.arg(format!("{d}/"));
        }
        out.extend(ok_raw(c, "ls-tree")?.split('\0').filter(|p| !p.is_empty()).map(str::to_string));
    }
    Ok(out)
}

/// What the session's overlay holds below the repository, at `upper`, as a commit in the clone's
/// store; None when there is nothing to carry. `started`: the session's start (unix seconds), the
/// earliest the agent can have read the checkout.
pub fn pending(source: &Path, clone: &Path, upper: &Path, started: i64) -> Result<Option<Pending>> {
    if !upper.is_dir() {
        return Ok(None);
    }
    let (mut paths, opaque, earliest) = walk(upper)?;
    let git = Git::open(clone)?;
    let skip: std::collections::HashSet<String> = ignored(&git, &paths)?.into_iter().collect();
    paths.retain(|p| !skip.contains(p));
    if paths.is_empty() && opaque.is_empty() {
        return Ok(None);
    }
    let head = flow::rev(&git, "HEAD")?;
    let first = earliest.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(started).max(started);
    let found = head_at(source, first)?;
    let from_reflog = found.is_some();
    let base = found.unwrap_or_else(|| head.clone());
    // the files as git stores them, read from the upper tree; a whiteout is no file: a deletion
    let mut entries: Vec<(String, Have)> = flow::worktree_entries(&git, upper, &paths)?;
    // what the base has below a deleted or replaced path, or an opaque directory, goes too
    let mut gone_below: Vec<String> = paths.clone();
    gone_below.extend(opaque.iter().cloned());
    let carried: std::collections::BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    let extra: Vec<String> = below(&git, &base, &gone_below)?.into_iter().filter(|p| !carried.contains(p.as_str())).collect();
    entries.extend(extra.into_iter().map(|p| (p, None)));
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup_by(|a, b| a.0 == b.0);
    let tree = flow::build_tree(&git, &base, &entries)?;
    if tree == flow::rev(&git, &format!("{base}^{{tree}}"))? {
        // the agent's files are the base's: nothing to bring, only entries to drop
        return Ok(Some(Pending { upper: upper.to_path_buf(), paths, opaque, commit: base.clone(), base, from_reflog }));
    }
    let mut c = git.cmd_write();
    for (k, v) in IDENT {
        c.env(k, v);
    }
    c.args(["commit-tree", &tree, "-p", &base, "-m", &format!("Work from before {} was pinned", source.display())]);
    let commit = ok(c, "carry: commit")?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill: carried", "refs/sill/carried", &commit]);
    ok(c, "carry: keep the commit")?;
    Ok(Some(Pending { upper: upper.to_path_buf(), paths, opaque, commit, base, from_reflog }))
}

/// Nothing to merge: the agent's files equal their base.
pub fn is_empty(p: &Pending) -> bool {
    p.commit == p.base
}

/// Merge the agent's work into the clone's state (its HEAD and uncommitted changes), no file
/// touched.
pub fn merge(clone: &Path, p: &Pending) -> Result<Merge> {
    let git = Git::open(clone)?;
    let ours = if flow::dirty(&git)?.is_empty() { flow::rev(&git, "HEAD")? } else { flow::snapshot(clone, "sill: before the carry")? };
    let mut c = git.cmd_write();
    c.args(["merge-tree", "--write-tree", "-z", "--name-only", "--no-messages", &format!("--merge-base={}", p.base), &ours, &p.commit]);
    let out = run(c)?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut parts = text.split('\0');
    let tree = parts.next().unwrap_or("").trim().to_string();
    match out.status.code() {
        Some(0) => Ok(Merge::Clean { ours, tree }),
        Some(1) => {
            let mut files: Vec<String> = parts.filter(|p| !p.is_empty()).map(str::to_string).collect();
            files.dedup();
            Ok(Merge::Conflict(files))
        }
        _ => bail!("merge-tree: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// A clean merge into the clone's files: what differs between its state `ours` and `tree` is
/// written, uncommitted; the index and branch are untouched.
pub fn write_clean(clone: &Path, ours: &str, tree: &str) -> Result<usize> {
    let git = Git::open(clone)?;
    let entries = flow::diff_tree(&git, ours, tree)?;
    for e in &entries {
        match &e.blob {
            Some(blob) => flow::write_entry(&git, clone, &e.path, &e.mode, blob)?,
            None => flow::remove_entry(clone, &e.path)?,
        }
    }
    Ok(entries.len())
}

/// Before the rebase that stops on the conflict: the workspace's branch at the agent's commit,
/// its files as that commit has them. Returns the branch's tip before, to rebase onto.
pub fn stage_rebase(clone: &Path, branch: &str, p: &Pending) -> Result<String> {
    let git = Git::open(clone)?;
    let tip = flow::rev(&git, "HEAD")?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill: before the carry", "refs/sill/pre-carry", &tip]);
    ok(c, "carry: keep the tip")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", "--reset", "-u", &p.commit]);
    ok(c, "carry: the agent's files")?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill: carry"]).arg(format!("refs/heads/{branch}")).arg(&p.commit);
    ok(c, "carry: the branch")?;
    Ok(tip)
}

/// The branch back at `tip`, its files as `tip` has them: after a rebase that could not run.
pub fn unstage(clone: &Path, branch: &str, tip: &str) -> Result<()> {
    flow::reset_to(clone, branch, tip, tip)
}

/// After a rebase that went through: the branch back at `tip`, the rebased work left in the files,
/// uncommitted.
pub fn uncommit(clone: &Path, branch: &str, tip: &str) -> Result<()> {
    let git = Git::open(clone)?;
    let mut c = git.cmd_write();
    c.args(["update-ref", "-m", "sill: carried, uncommitted"]).arg(format!("refs/heads/{branch}")).arg(tip);
    ok(c, "carry: the branch")?;
    let mut c = git.cmd_write();
    c.args(["read-tree", tip]);
    ok(c, "carry: the index")?;
    Ok(())
}

/// The carried entries leave the upper tree, and the directories they leave empty. Only while
/// the overlay is not mounted.
pub fn forget(p: &Pending) -> Result<()> {
    for path in &p.paths {
        let full = p.upper.join(path);
        if full.symlink_metadata().is_ok() {
            crate::safe_fs::remove_file_beneath(&p.upper, Path::new(path))?;
        }
        let mut dir = Path::new(path).parent();
        while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
            // not empty, or not ours to remove: it stays
            if std::fs::remove_dir(p.upper.join(d)).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    // an opaque directory left empty (the agent emptied it): carried as well
    let mut dirs: Vec<&String> = p.opaque.iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
    for d in dirs {
        let _ = std::fs::remove_dir(p.upper.join(d));
    }
    Ok(())
}
