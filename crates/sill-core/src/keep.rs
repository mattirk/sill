//! Keep: copy a recorded change from a session's upper tree to the host (SPEC §4.4). Runs on the host,
//! never inside the jail. Rules:
//! - never follow symlinks on either side: every path component is opened with `RESOLVE_NO_SYMLINKS`
//!   below the mount root, symlinks are recreated as symlinks;
//! - refuse if the content copied differs from the one the person reviewed (`expected_hash`, from the
//!   ledger index): the hash is taken from the bytes that were actually copied, so nothing swapped in
//!   between the review and the copy can land on the host;
//! - deletions (whiteouts) remove the host path; directories are created with the upper's mode;
//! - mode is preserved, setuid/setgid bits are stripped unless `allow_special`; ownership is left to
//!   whoever runs this (the person; root-owned targets go through a separate sudo helper later);
//! - the host path is looked at before the previous version is saved and again just before it is
//!   replaced or removed: an edit on the host in between (the person saving in an editor while the
//!   list was open) stops the keep instead of being overwritten (SPEC §4.11 #16); a file is written
//!   to a temporary next to the target and renamed over it, so the host never sees a half-written
//!   file and nothing is left behind when a step fails. The rename gives the path a new inode: other
//!   hard links to the old file keep the old content (a keep must not write through a link into
//!   another path), and extended attributes of the old file are not carried over;
//! - **undo** (SPEC §4.4): with `KeepOptions::undo` set to the session's `kept/` directory, the host's
//!   previous version is saved there before anything is touched and a line in `kept/log.jsonl` says
//!   what was done; [`undo`] puts the previous version back, refusing if the host changed since.
//!   Nothing leaves the overlay without a click, and no click is final.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustix::fs::{Mode, OFlags, OpenOptionsExt, ResolveFlags, openat2};

use crate::ledger::{Change, Kind};

#[derive(Debug, Clone, Default)]
pub struct KeepOptions {
    /// Hex SHA-256 of the upper content the person reviewed; `None` skips the check.
    pub expected_hash: Option<String>,
    /// Keep setuid/setgid bits.
    pub allow_special: bool,
    /// The session's `kept/` directory: the host's previous version goes there before every keep so
    /// the keep can be undone. `None` keeps without undo.
    pub undo: Option<PathBuf>,
}

pub use sill_proto::wire::Kept;

const LOG: &str = "log.jsonl";
const PRIOR: &str = "prior";

/// What tells a host path's state apart between the review and the write: a change in any of
/// these means the host moved on and the keep stops (SPEC §4.11 #16). `None` = absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    dev: u64,
    ino: u64,
    mode: u32,
    size: u64,
    mtime: (i64, i64),
}

fn snapshot(host: &Path) -> Option<Snapshot> {
    let m = host.symlink_metadata().ok()?;
    Some(Snapshot { dev: m.dev(), ino: m.ino(), mode: m.mode(), size: m.len(), mtime: (m.mtime(), m.mtime_nsec()) })
}

/// The host still is what it was when the keep started.
fn still(host: &Path, before: Option<Snapshot>) -> Result<()> {
    if snapshot(host) != before {
        bail!("{} changed on the host while it was being kept; review it again", host.display());
    }
    Ok(())
}

/// Why keep refuses a path outright, or `None`. What the shelf itself runs or reads — its
/// configuration (policies, agent overrides: a session's mounts and allowed domains are made of
/// them) and its state (the session store, the staged binaries, the agents' homes) — and a
/// repository's own executable configuration (`.git/config`, `.git/hooks`). A kept line there is
/// the agent configuring the host; the person does it by hand if they mean it (security.md S2).
pub fn protected(host: &Path) -> Option<&'static str> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/state"));
    if host.starts_with(config.join("sill")) {
        return Some("sill's own configuration");
    }
    if host.starts_with(state.join("sill")) {
        return Some("sill's own state");
    }
    let parts: Vec<&std::ffi::OsStr> = host.components().map(|c| c.as_os_str()).collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == ".git"
            && let Some(next) = parts.get(i + 1)
            && (*next == "config" || *next == "hooks")
        {
            return Some("the repository's git configuration");
        }
    }
    None
}

/// Open `path` with every component resolved without following symlinks and without leaving `/`.
fn open_nofollow(path: &Path, flags: OFlags, mode: Mode) -> Result<std::fs::File> {
    let root = std::fs::File::open("/")?;
    let rel = path.strip_prefix("/").unwrap_or(path);
    let fd = openat2(&root, rel, flags | OFlags::CLOEXEC, mode, ResolveFlags::NO_SYMLINKS | ResolveFlags::BENEATH)
        .with_context(|| format!("open {} without following symlinks", path.display()))?;
    Ok(std::fs::File::from(fd))
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_file(path: &Path) -> Result<String> { sha256_reader(std::fs::File::open(path)?) }

/// Hash a stream with bounded working memory (also supports anonymous snapshot files).
pub fn sha256_reader(mut f: impl std::io::Read) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Copy a regular file without following symlinks on either side; `dst` must not exist. Returns the
/// SHA-256 of the bytes copied.
fn copy_file(src: &Path, dst: &Path, mode: u32) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut from = open_nofollow(src, OFlags::RDONLY, Mode::empty())?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .mode(mode & 0o7777)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        to.write_all(&buf[..n])?;
    }
    to.flush()?;
    to.sync_all()?;
    Ok(hex(&h.finalize()))
}

/// Copy a directory tree: files, symlinks (as symlinks) and directories with their modes; sockets,
/// devices and fifos are skipped. Nothing is followed. `dst` may exist (entries are merged in).
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let md = src.symlink_metadata()?;
    if !dst.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
        std::fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))?;
    }
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        let m = from.symlink_metadata()?;
        let ft = m.file_type();
        if ft.is_symlink() {
            remove_if_exists(&to)?;
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            remove_if_exists(&to)?;
            copy_file(&from, &to, m.mode())?;
        }
    }
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(md.mode() & 0o7777))?;
    Ok(())
}

/// The host's previous version of a path, saved below `kept/<id>/` before keep touches the host.
struct Prior {
    dir: PathBuf,
    id: String,
    before: &'static str,
}

impl Prior {
    fn save(dir: &Path, change: &Change) -> Result<Self> {
        if !dir.is_dir() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let id = format!("{}-{}", crate::store::now(), crate::store::new_id());
        let payload = dir.join(&id);
        std::fs::create_dir(&payload).with_context(|| format!("create {}", payload.display()))?;
        let host = &change.path;
        let prior = payload.join(PRIOR);
        let before = match host.symlink_metadata() {
            Err(_) => "absent",
            Ok(md) if md.file_type().is_symlink() => {
                std::os::unix::fs::symlink(std::fs::read_link(host)?, &prior)?;
                "symlink"
            }
            Ok(md) if md.is_file() => {
                copy_file(host, &prior, md.mode()).with_context(|| format!("save the previous {}", host.display()))?;
                "file"
            }
            Ok(md) if md.is_dir() => {
                // keep removes a host directory only for a deletion or a type change; a directory kept
                // over a directory just gets the upper's mode, so only the mode is worth saving
                let upper_is_dir = change.upper.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false);
                if upper_is_dir {
                    std::fs::create_dir(&prior)?;
                    std::fs::set_permissions(&prior, std::fs::Permissions::from_mode(md.mode() & 0o7777))?;
                } else {
                    copy_tree(host, &prior).with_context(|| format!("save the previous {}", host.display()))?;
                }
                "dir"
            }
            Ok(_) => "other",
        };
        Ok(Self { dir: dir.to_path_buf(), id, before })
    }

    fn record(self, change: &Change, what: &str, hash: Option<String>) -> Result<()> {
        let rec = Kept {
            id: self.id,
            ts: crate::store::now(),
            path: change.path.clone(),
            what: what.to_string(),
            before: self.before.to_string(),
            hash,
            undone_at: None,
        };
        let mut log = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(self.dir.join(LOG))?;
        writeln!(log, "{}", serde_json::to_string(&rec)?)?;
        Ok(())
    }

    /// Nothing was done: no record, no payload.
    fn discard(self) {
        let _ = std::fs::remove_dir_all(self.dir.join(&self.id));
    }
}

/// Apply one ledger entry to the host. Returns what was done.
pub fn keep(change: &Change, opts: &KeepOptions) -> Result<&'static str> {
    if let Some(why) = protected(&change.path) {
        bail!("{}: not kept — {why}; do it by hand if you mean it", change.path.display());
    }
    let host = &change.path;
    if !host.is_absolute() {
        bail!("{} is not absolute", host.display());
    }
    // the parent must exist and must not be reached through a symlink
    let parent = host.parent().context("path has no parent")?;
    if parent != Path::new("/") {
        let _ = open_nofollow(parent, OFlags::PATH | OFlags::DIRECTORY, Mode::empty())?;
    }
    // the host as it is now: the previous version saved next is this one, and the write below
    // happens only if it still is
    let before = snapshot(host);
    // the previous version first: if it can't be saved, nothing is kept
    let prior = match &opts.undo {
        Some(dir) => Some(Prior::save(dir, change)?),
        None => None,
    };
    let (what, hash) = match apply(change, opts, before) {
        Ok(done) => done,
        Err(e) => {
            if let Some(p) = prior {
                p.discard();
            }
            return Err(e);
        }
    };
    if let Some(p) = prior {
        if what == "already absent" { p.discard() } else { p.record(change, what, hash)? }
    }
    Ok(what)
}

fn apply(change: &Change, opts: &KeepOptions, before: Option<Snapshot>) -> Result<(&'static str, Option<String>)> {
    let host = &change.path;
    match change.kind {
        Kind::Deleted => {
            still(host, before)?;
            match host.symlink_metadata() {
                Ok(md) if md.is_dir() => std::fs::remove_dir_all(host)?,
                Ok(_) => std::fs::remove_file(host)?,
                Err(_) => return Ok(("already absent", None)),
            }
            Ok(("removed", None))
        }
        Kind::Added | Kind::Modified | Kind::TypeChanged | Kind::Replaced => {
            let up = change.upper.symlink_metadata().with_context(|| format!("stat {}", change.upper.display()))?;
            if up.file_type().is_symlink() {
                let target = std::fs::read_link(&change.upper)?;
                remove_if_exists(host)?;
                std::os::unix::fs::symlink(target, host)?;
                return Ok(("symlink", None));
            }
            if up.is_dir() {
                if host.symlink_metadata().map(|m| !m.is_dir()).unwrap_or(false) {
                    remove_if_exists(host)?;
                }
                std::fs::create_dir_all(host)?;
                std::fs::set_permissions(host, std::fs::Permissions::from_mode(strip_special(up.mode(), opts)))?;
                return Ok(("directory", None));
            }
            if !up.is_file() {
                bail!("{}: not a regular file, symlink or directory", change.upper.display());
            }
            // write to a temp file next to the target, then rename: never a half-written file on the
            // host; the hash of the bytes copied is the one checked against the review
            let tmp = temp_name(host);
            let hash = match copy_file(&change.upper, &tmp, strip_special(up.mode(), opts)) {
                Ok(h) => h,
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            };
            if let Some(expected) = &opts.expected_hash
                && &hash != expected
            {
                let _ = std::fs::remove_file(&tmp);
                bail!("{} changed since it was reviewed (hash {}… ≠ {}…)", host.display(), &hash[..12], &expected[..12]);
            }
            // the last look before the host changes: whatever fails from here on leaves no temp file
            let placed = (|| -> Result<()> {
                still(host, before)?;
                if host.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
                    std::fs::remove_dir_all(host)?;
                }
                std::fs::rename(&tmp, host).with_context(|| format!("rename into {}", host.display()))
            })();
            if let Err(e) = placed {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            Ok((if change.kind == Kind::Added { "added" } else { "updated" }, Some(hash)))
        }
    }
}

/// Every keep recorded in `kept/`, oldest first (undone ones included; see [`undoable`]).
pub fn list(dir: &Path) -> Vec<Kept> {
    let Ok(text) = std::fs::read_to_string(dir.join(LOG)) else { return Vec::new() };
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// The keeps that can still be undone, newest first.
pub fn undoable(dir: &Path) -> Vec<Kept> {
    let mut v: Vec<Kept> = list(dir).into_iter().filter(|k| k.undone_at.is_none()).collect();
    v.reverse();
    v
}

/// Where a keep's previous version lies (nothing there for `before` = `absent` or `other`).
pub fn prior_path(dir: &Path, rec: &Kept) -> PathBuf {
    dir.join(&rec.id).join(PRIOR)
}

/// Put the host's previous version back. Refuses when the host changed since the keep (a kept file
/// edited, a removed path recreated) unless `force`; the same symlink rules as keep apply.
pub fn undo(dir: &Path, rec: &Kept, force: bool) -> Result<&'static str> {
    if rec.undone_at.is_some() {
        bail!("{} was already undone", rec.path.display());
    }
    let host = &rec.path;
    if !host.is_absolute() {
        bail!("{} is not absolute", host.display());
    }
    let parent = host.parent().context("path has no parent")?;
    if parent != Path::new("/") {
        let _ = open_nofollow(parent, OFlags::PATH | OFlags::DIRECTORY, Mode::empty())?;
    }
    let cur = host.symlink_metadata().ok();
    if !force {
        match (&rec.hash, &cur) {
            (Some(h), Some(md)) if md.is_file() => {
                if &sha256_file(host)? != h {
                    bail!("{} was edited on the host after it was kept", host.display());
                }
            }
            (Some(_), Some(_)) => bail!("{} is no longer a regular file", host.display()),
            (Some(_), None) => bail!("{} is gone from the host", host.display()),
            (None, _) => {}
        }
        if rec.what == "removed" && cur.is_some() {
            bail!("{} was recreated on the host after it was removed", host.display());
        }
    }
    let prior = prior_path(dir, rec);
    let what = match rec.before.as_str() {
        "absent" => {
            match &cur {
                // a directory is only removed empty: what was kept below it has its own records
                Some(md) if md.is_dir() => std::fs::remove_dir(host).with_context(|| format!("remove {}", host.display()))?,
                Some(_) => std::fs::remove_file(host)?,
                None => {}
            }
            "removed"
        }
        "file" => {
            let md = prior.symlink_metadata().with_context(|| format!("previous version {} is missing", prior.display()))?;
            if !md.is_file() {
                bail!("{} is not a regular file", prior.display());
            }
            let tmp = temp_name(host);
            if let Err(e) = copy_file(&prior, &tmp, md.mode()) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            if cur.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
                std::fs::remove_dir_all(host)?;
            }
            std::fs::rename(&tmp, host).with_context(|| format!("rename into {}", host.display()))?;
            "restored"
        }
        "symlink" => {
            let target = std::fs::read_link(&prior).with_context(|| format!("previous version {} is missing", prior.display()))?;
            remove_if_exists(host)?;
            std::os::unix::fs::symlink(target, host)?;
            "restored"
        }
        "dir" => {
            if !prior.is_dir() {
                bail!("previous version {} is missing", prior.display());
            }
            if cur.as_ref().map(|m| !m.is_dir()).unwrap_or(false) {
                remove_if_exists(host)?;
            }
            copy_tree(&prior, host)?;
            "restored"
        }
        other => bail!("the host had a {other} at {}; put it back by hand", host.display()),
    };
    mark_undone(dir, &rec.id)?;
    let _ = std::fs::remove_dir_all(dir.join(&rec.id));
    Ok(what)
}

/// Rewrite the journal with the record marked undone (write next to it, then rename).
fn mark_undone(dir: &Path, id: &str) -> Result<()> {
    let now = crate::store::now();
    let mut lines = Vec::new();
    for mut k in list(dir) {
        if k.id == id {
            k.undone_at = Some(now);
        }
        lines.push(serde_json::to_string(&k)?);
    }
    let tmp = dir.join(format!(".{LOG}.{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        for l in &lines {
            writeln!(f, "{l}")?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(LOG))?;
    Ok(())
}

fn strip_special(mode: u32, opts: &KeepOptions) -> u32 {
    if opts.allow_special { mode } else { mode & !0o6000 }
}

fn remove_if_exists(p: &Path) -> Result<()> {
    match p.symlink_metadata() {
        Ok(md) if md.is_dir() => std::fs::remove_dir_all(p)?,
        Ok(_) => std::fs::remove_file(p)?,
        Err(_) => {}
    }
    Ok(())
}

fn temp_name(host: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(host.file_name().unwrap_or_default());
    name.push(format!(".sill-keep.{}", std::process::id()));
    host.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::FileType;

    fn change(path: PathBuf, upper: PathBuf, kind: Kind) -> Change {
        Change { path, upper, kind, file_type: FileType::File, size: 0, conflict: false, ignored: false }
    }

    #[test]
    fn non_utf8_keep_journal_recovers_exact_path_for_undo() {
        use std::os::unix::ffi::OsStringExt;
        let dir = std::env::temp_dir().join(format!("sill-keep-byte-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (upper, host, kept) = (dir.join("upper"), dir.join("host"), dir.join("kept"));
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        let name = std::ffi::OsString::from_vec(b"file\xff\nname".to_vec());
        std::fs::write(host.join(&name), "old").unwrap();
        std::fs::write(upper.join(&name), "new").unwrap();
        let opts = KeepOptions { undo: Some(kept.clone()), ..Default::default() };
        keep(&change(host.join(&name), upper.join(&name), Kind::Modified), &opts).unwrap();
        let recs = undoable(&kept);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].path, host.join(&name));
        assert_eq!(std::fs::read(host.join(&name)).unwrap(), b"new");
        undo(&kept, &recs[0], false).unwrap();
        assert_eq!(std::fs::read(host.join(&name)).unwrap(), b"old");
        assert!(undoable(&kept).is_empty());
        assert_eq!(list(&kept)[0].path, host.join(&name));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keeps_file_and_refuses_symlink_parent() {
        let dir = std::env::temp_dir().join(format!("sill-keep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("upper")).unwrap();
        std::fs::create_dir_all(dir.join("host")).unwrap();
        std::fs::write(dir.join("upper/a.txt"), "new\n").unwrap();
        let change = change(dir.join("host/a.txt"), dir.join("upper/a.txt"), Kind::Added);
        assert_eq!(keep(&change, &KeepOptions::default()).unwrap(), "added");
        assert_eq!(std::fs::read_to_string(dir.join("host/a.txt")).unwrap(), "new\n");
        // hash check: nothing lands on the host, no temp file stays behind
        let bad = KeepOptions { expected_hash: Some("00".repeat(32)), ..Default::default() };
        assert!(keep(&change, &bad).is_err());
        assert_eq!(std::fs::read_dir(dir.join("host")).unwrap().count(), 1);
        // the host edited after the scan but before the write: refused, nothing left behind
        std::fs::write(dir.join("host/a.txt"), "edited meanwhile\n").unwrap();
        let raced = Change { kind: Kind::Modified, ..change.clone() };
        let opts_race = KeepOptions { undo: Some(dir.join("kept")), ..Default::default() };
        // `before` is taken inside keep(); simulate the race with a snapshot that is already stale
        assert!(apply(&raced, &opts_race, None).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("host/a.txt")).unwrap(), "edited meanwhile\n");
        assert_eq!(std::fs::read_dir(dir.join("host")).unwrap().count(), 1);
        assert_eq!(keep(&raced, &KeepOptions::default()).unwrap(), "updated");
        // parent reached through a symlink: refused
        std::os::unix::fs::symlink(dir.join("host"), dir.join("link")).unwrap();
        let via_link = Change { path: dir.join("link/a.txt"), ..change.clone() };
        assert!(keep(&via_link, &KeepOptions::default()).is_err());
        // deletion
        let del = Change { kind: Kind::Deleted, ..change.clone() };
        assert_eq!(keep(&del, &KeepOptions::default()).unwrap(), "removed");
        assert!(!dir.join("host/a.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn undo_puts_the_previous_version_back() {
        let dir = std::env::temp_dir().join(format!("sill-undo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (upper, host, kept) = (dir.join("upper"), dir.join("host"), dir.join("kept"));
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::create_dir_all(host.join("d/sub")).unwrap();
        let opts = KeepOptions { undo: Some(kept.clone()), ..Default::default() };

        // a modified file: undo restores the old content, but not over a later host edit
        std::fs::write(host.join("a.txt"), "old\n").unwrap();
        std::fs::write(upper.join("a.txt"), "new\n").unwrap();
        let m = change(host.join("a.txt"), upper.join("a.txt"), Kind::Modified);
        assert_eq!(keep(&m, &opts).unwrap(), "updated");
        let recs = undoable(&kept);
        assert_eq!(recs.len(), 1);
        assert_eq!((recs[0].before.as_str(), recs[0].what.as_str()), ("file", "updated"));
        std::fs::write(host.join("a.txt"), "edited on the host\n").unwrap();
        assert!(undo(&kept, &recs[0], false).is_err());
        assert_eq!(undo(&kept, &recs[0], true).unwrap(), "restored");
        assert_eq!(std::fs::read_to_string(host.join("a.txt")).unwrap(), "old\n");
        assert!(undoable(&kept).is_empty());
        assert_eq!(list(&kept).len(), 1); // the journal remembers it
        assert!(!kept.join(&recs[0].id).exists());

        // an added file: undo removes it
        std::fs::write(upper.join("b.txt"), "b\n").unwrap();
        let a = change(host.join("b.txt"), upper.join("b.txt"), Kind::Added);
        assert_eq!(keep(&a, &opts).unwrap(), "added");
        let recs = undoable(&kept);
        assert_eq!(recs[0].before, "absent");
        assert_eq!(undo(&kept, &recs[0], false).unwrap(), "removed");
        assert!(!host.join("b.txt").exists());

        // a deleted directory: the whole tree comes back
        std::fs::write(host.join("d/sub/f.txt"), "f\n").unwrap();
        std::os::unix::fs::symlink("f.txt", host.join("d/sub/l")).unwrap();
        let d = change(host.join("d"), upper.join("d"), Kind::Deleted); // upper is a whiteout in real life
        assert_eq!(keep(&d, &opts).unwrap(), "removed");
        assert!(!host.join("d").exists());
        let recs = undoable(&kept);
        assert_eq!(recs[0].before, "dir");
        assert_eq!(undo(&kept, &recs[0], false).unwrap(), "restored");
        assert_eq!(std::fs::read_to_string(host.join("d/sub/f.txt")).unwrap(), "f\n");
        assert_eq!(std::fs::read_link(host.join("d/sub/l")).unwrap(), PathBuf::from("f.txt"));

        // a deletion of something already absent leaves no record
        let gone = change(host.join("nope"), upper.join("nope"), Kind::Deleted);
        assert_eq!(keep(&gone, &opts).unwrap(), "already absent");
        assert!(undoable(&kept).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
