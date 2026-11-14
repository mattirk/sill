//! Keep: copy a recorded change from a session's upper tree to the host (SPEC §4.4). Runs on the host,
//! never inside the jail. Rules:
//! - never follow symlinks on either side: every path component is opened with `RESOLVE_NO_SYMLINKS`
//!   below the mount root, symlinks are recreated as symlinks;
//! - refuse if the upper file's content hash differs from the one the person reviewed
//!   (`expected_hash`, from the ledger index) — the agent or a background process changed it since;
//! - deletions (whiteouts) remove the host path; directories are created with the upper's mode;
//! - mode is preserved, setuid/setgid bits are stripped unless `allow_special`; ownership is left to
//!   whoever runs this (the person; root-owned targets go through a separate sudo helper later).

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
}

/// Open `path` with every component resolved without following symlinks and without leaving `/`.
fn open_nofollow(path: &Path, flags: OFlags, mode: Mode) -> Result<std::fs::File> {
    let root = std::fs::File::open("/")?;
    let rel = path.strip_prefix("/").unwrap_or(path);
    let fd = openat2(&root, rel, flags | OFlags::CLOEXEC, mode, ResolveFlags::NO_SYMLINKS | ResolveFlags::BENEATH)
        .with_context(|| format!("open {} without following symlinks", path.display()))?;
    Ok(std::fs::File::from(fd))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Apply one ledger entry to the host. Returns what was done.
pub fn keep(change: &Change, opts: &KeepOptions) -> Result<&'static str> {
    let host = &change.path;
    if !host.is_absolute() {
        bail!("{} is not absolute", host.display());
    }
    // the parent must exist and must not be reached through a symlink
    let parent = host.parent().context("path has no parent")?;
    if parent != Path::new("/") {
        let _ = open_nofollow(parent, OFlags::PATH | OFlags::DIRECTORY, Mode::empty())?;
    }

    match change.kind {
        Kind::Deleted => {
            match host.symlink_metadata() {
                Ok(md) if md.is_dir() => std::fs::remove_dir_all(host)?,
                Ok(_) => std::fs::remove_file(host)?,
                Err(_) => return Ok("already absent"),
            }
            Ok("removed")
        }
        Kind::Added | Kind::Modified | Kind::TypeChanged | Kind::Replaced => {
            let up = change.upper.symlink_metadata().with_context(|| format!("stat {}", change.upper.display()))?;
            if up.file_type().is_symlink() {
                let target = std::fs::read_link(&change.upper)?;
                remove_if_exists(host)?;
                std::os::unix::fs::symlink(target, host)?;
                return Ok("symlink");
            }
            if up.is_dir() {
                if host.symlink_metadata().map(|m| !m.is_dir()).unwrap_or(false) {
                    remove_if_exists(host)?;
                }
                std::fs::create_dir_all(host)?;
                std::fs::set_permissions(host, std::fs::Permissions::from_mode(strip_special(up.mode(), opts)))?;
                return Ok("directory");
            }
            if !up.is_file() {
                bail!("{}: not a regular file, symlink or directory", change.upper.display());
            }
            if let Some(expected) = &opts.expected_hash {
                let actual = sha256_file(&change.upper)?;
                if &actual != expected {
                    bail!("{} changed since it was reviewed (hash {}… ≠ {}…)", host.display(), &actual[..12], &expected[..12]);
                }
            }
            // write to a temp file next to the target, then rename: never a half-written file on the host
            let tmp = temp_name(host);
            {
                let mut src = open_nofollow(&change.upper, OFlags::RDONLY, Mode::empty())?;
                let mut dst = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(OFlags::NOFOLLOW.bits() as i32)
                    .mode(strip_special(up.mode(), opts) & 0o7777)
                    .open(&tmp)
                    .with_context(|| format!("create {}", tmp.display()))?;
                std::io::copy(&mut src, &mut dst)?;
                dst.flush()?;
                dst.sync_all()?;
            }
            if host.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
                std::fs::remove_dir_all(host)?;
            }
            std::fs::rename(&tmp, host).with_context(|| format!("rename into {}", host.display()))?;
            Ok(if change.kind == Kind::Added { "added" } else { "updated" })
        }
    }
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
    let name = host.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    host.with_file_name(format!(".{name}.sill-keep.{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::FileType;

    #[test]
    fn keeps_file_and_refuses_symlink_parent() {
        let dir = std::env::temp_dir().join(format!("sill-keep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("upper")).unwrap();
        std::fs::create_dir_all(dir.join("host")).unwrap();
        std::fs::write(dir.join("upper/a.txt"), "new\n").unwrap();
        let change = Change {
            path: dir.join("host/a.txt"),
            upper: dir.join("upper/a.txt"),
            kind: Kind::Added,
            file_type: FileType::File,
            size: 4,
            conflict: false,
            ignored: false,
        };
        assert_eq!(keep(&change, &KeepOptions::default()).unwrap(), "added");
        assert_eq!(std::fs::read_to_string(dir.join("host/a.txt")).unwrap(), "new\n");
        // hash check
        let bad = KeepOptions { expected_hash: Some("00".repeat(32)), allow_special: false };
        assert!(keep(&change, &bad).is_err());
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
}
