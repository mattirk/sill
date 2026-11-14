//! Host-side writes below a directory the agent can reach (a pin, a private scratch dir, its own
//! home): every component is resolved without following symlinks and without leaving the root
//! (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`), so a link the agent planted cannot
//! steer the daemon's write anywhere else on the host; a FIFO cannot stall it (security.md S6, S9,
//! S11). Also the owner check for a per-user directory under a world-writable parent (S5).

use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawMode, ResolveFlags, fchmod, fstat, mkdirat, open, openat2, unlinkat};
use rustix::io::Errno;

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH.union(ResolveFlags::NO_SYMLINKS);

/// `root` as a directory fd: symlinks on the way to it are the host's own; one at its end is not.
fn open_root(root: &Path) -> Result<OwnedFd> {
    open(root, OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
        .with_context(|| format!("open {}", root.display()))
}

/// The directory `rel` below `root`, every component a real directory (no symlink, nothing outside
/// `root`); missing ones are created 0755 when `create`.
pub fn open_dir_beneath(root: &Path, rel: &Path, create: bool) -> Result<OwnedFd> {
    let mut fd = open_root(root)?;
    for c in rel.components() {
        let name = match c {
            Component::Normal(n) => n,
            Component::CurDir => continue,
            _ => bail!("{}: not a plain relative path", rel.display()),
        };
        let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        fd = match openat2(&fd, name, flags, Mode::empty(), RESOLVE) {
            Ok(d) => d,
            Err(Errno::NOENT) if create => {
                match mkdirat(&fd, name, Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(e) => return Err(e).with_context(|| format!("create {} below {}", rel.display(), root.display())),
                }
                openat2(&fd, name, flags, Mode::empty(), RESOLVE)
                    .with_context(|| format!("open {} below {} (no symlinks)", rel.display(), root.display()))?
            }
            Err(e) => return Err(e).with_context(|| format!("open {} below {} (no symlinks)", rel.display(), root.display())),
        };
    }
    Ok(fd)
}

/// Write `bytes` to the regular file `rel` below `root` (created with `mode`, else truncated and
/// set to `mode`); parents are created. A symlink anywhere on the way, or anything at the end that
/// is not a regular file, is refused.
pub fn write_file_beneath(root: &Path, rel: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = rel.file_name().with_context(|| format!("{}: no file name", rel.display()))?;
    let dir = open_dir_beneath(root, parent, true)?;
    // NONBLOCK: a FIFO planted here would otherwise hold the writer until something reads it
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = openat2(&dir, name, flags, Mode::from_raw_mode(mode), RESOLVE)
        .with_context(|| format!("create {} below {} (no symlinks)", rel.display(), root.display()))?;
    let st = fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode as RawMode) != FileType::RegularFile {
        bail!("{}: not a regular file", rel.display());
    }
    fchmod(&fd, Mode::from_raw_mode(mode))?;
    let mut f = std::fs::File::from(fd);
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Unlink `rel` below `root` without following a symlink on the way (a symlink at the end is what
/// gets removed, not its target).
pub fn remove_file_beneath(root: &Path, rel: &Path) -> Result<()> {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = rel.file_name().with_context(|| format!("{}: no file name", rel.display()))?;
    let dir = open_dir_beneath(root, parent, false)?;
    unlinkat(&dir, name, AtFlags::empty()).with_context(|| format!("remove {} below {}", rel.display(), root.display()))?;
    Ok(())
}

/// Initialize a missing declared file without reading or replacing an existing one. An atomic
/// no-replace rename prevents a concurrent launcher from overwriting newly saved settings.
pub fn initialize_private_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> Result<()> {
    use rustix::fs::{renameat_with, RenameFlags};
    let dir = open_dir_beneath(root, rel.parent().unwrap_or(Path::new("")), true)?;
    let name = rel.file_name().context("declared file has no name")?;
    let check = || -> Result<bool> {
        match openat2(&dir, name, OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty(), RESOLVE) {
            Ok(fd) => {
                anyhow::ensure!(FileType::from_raw_mode(fstat(&fd)?.st_mode) == FileType::RegularFile, "{}: not a regular file", rel.display());
                Ok(true)
            }
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    };
    if check()? { return Ok(()); }
    let tmp = format!(".sill-init-{:032x}", rand::random::<u128>());
    let fd = openat2(&dir, tmp.as_str(), OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC, Mode::from_raw_mode(0o600), RESOLVE)?;
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        match renameat_with(&dir, tmp.as_str(), &dir, name, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(()),
            Err(Errno::EXIST) => { check()?; Ok(()) }
            Err(e) => Err(e.into()),
        }
    })();
    let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty());
    result
}

/// Replace the regular file `name` in `dir` with `bytes` in one step: written to a temporary next
/// to it, synced, renamed over — a crash leaves the old or the new version whole, never an empty
/// file. `dir` is opened without following a link at its end; a symlink at `name` is refused (its
/// target may be anywhere on the host — edit that under its own path, security.md S9). The mode
/// is the file's own, 0644 for a new one.
pub fn replace_file(dir: &Path, name: &std::ffi::OsStr, bytes: &[u8]) -> Result<()> {
    use rustix::fs::{renameat, statat};
    let d = open_root(dir)?;
    let mode = match statat(&d, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) if FileType::from_raw_mode(st.st_mode as RawMode) == FileType::Symlink => {
            bail!("{} is a symlink: open its target to edit it", Path::new(name).display())
        }
        Ok(st) if FileType::from_raw_mode(st.st_mode as RawMode) != FileType::RegularFile => {
            bail!("{} is not a regular file", Path::new(name).display())
        }
        Ok(st) => st.st_mode as RawMode & 0o7777,
        Err(_) => 0o644,
    };
    // A unique ASCII sibling avoids concurrent writers sharing a temp file, preserves
    // arbitrary target bytes, and works even when the target is already NAME_MAX long.
    let tmp = format!(".sill-save-{:032x}", rand::random::<u128>());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = openat2(&d, tmp.as_str(), flags, Mode::from_raw_mode(mode), RESOLVE)
        .with_context(|| format!("create {} in {}", tmp, dir.display()))?;
    let written = (|| -> Result<()> {
        let mut f = std::fs::File::from(fd);
        f.write_all(bytes)?;
        f.sync_all()?;
        renameat(&d, tmp.as_str(), &d, name).with_context(|| format!("rename over {}", Path::new(name).display()))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = unlinkat(&d, tmp.as_str(), AtFlags::empty());
    }
    written
}

/// Copy the regular files below `src` — a tree sill itself wrote — to the same paths below
/// `dst`, a tree the agent can reach, through [`write_file_beneath`]. Symlinks and special files
/// in `src` are skipped. Returns how many files were written.
pub fn copy_files_beneath(src: &Path, dst: &Path) -> Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    fn walk(base: &Path, dir: &Path, dst: &Path, n: &mut usize) -> Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            let md = std::fs::symlink_metadata(&p)?;
            if md.is_dir() {
                walk(base, &p, dst, n)?;
            } else if md.is_file() {
                let rel = p.strip_prefix(base).unwrap_or(&p);
                write_file_beneath(dst, rel, &std::fs::read(&p)?, md.permissions().mode() & 0o777)?;
                *n += 1;
            }
        }
        Ok(())
    }
    let mut n = 0;
    if src.is_dir() {
        walk(src, src, dst, &mut n)?;
    }
    Ok(n)
}

/// Open `dir` and check it is `uid`'s: a directory under a world-writable parent (`/var/lib/sill`
/// is 1777 like `/tmp`) could have been put there by anyone. `create` makes it 0700 first when it
/// is missing. A symlink at `dir` is refused by `O_NOFOLLOW`.
pub fn owned_dir(dir: &Path, uid: u32, create: bool) -> Result<OwnedFd> {
    if create {
        match rustix::fs::mkdir(dir, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e).with_context(|| format!("create {}", dir.display())),
        }
    }
    let fd = open_root(dir)?;
    let st = fstat(&fd)?;
    if st.st_uid != uid {
        bail!("{} belongs to uid {}, not {uid}: not sill's, refusing to use it", dir.display(), st.st_uid);
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sill-safefs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_below_the_root_and_never_through_a_link() {
        let root = scratch("w");
        let outside = scratch("outside");
        write_file_beneath(&root, Path::new("a/b/c.txt"), b"hi", 0o644).unwrap();
        assert_eq!(std::fs::read(root.join("a/b/c.txt")).unwrap(), b"hi");
        // a directory on the way replaced by a link out of the root
        std::fs::remove_dir_all(root.join("a/b")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a/b")).unwrap();
        assert!(write_file_beneath(&root, Path::new("a/b/c.txt"), b"x", 0o644).is_err());
        assert!(!outside.join("c.txt").exists());
        // a link at the end
        std::os::unix::fs::symlink(outside.join("t"), root.join("link")).unwrap();
        assert!(write_file_beneath(&root, Path::new("link"), b"x", 0o644).is_err());
        assert!(!outside.join("t").exists());
        // `..` never
        assert!(write_file_beneath(&root, Path::new("../esc"), b"x", 0o644).is_err());
        // removing goes through the same resolution; a link at the end is unlinked, not followed
        std::fs::write(outside.join("keep"), b"k").unwrap();
        std::os::unix::fs::symlink(outside.join("keep"), root.join("l2")).unwrap();
        remove_file_beneath(&root, Path::new("l2")).unwrap();
        assert!(outside.join("keep").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn replace_file_is_whole_and_refuses_a_link() {
        let root = scratch("replace");
        std::fs::write(root.join("f.txt"), "old").unwrap();
        std::fs::set_permissions(root.join("f.txt"), std::fs::Permissions::from_mode(0o640)).unwrap();
        replace_file(&root, std::ffi::OsStr::new("f.txt"), b"new").unwrap();
        assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"new");
        assert_eq!(std::fs::metadata(root.join("f.txt")).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "no temp file left");
        replace_file(&root, std::ffi::OsStr::new("g.txt"), b"g").unwrap();
        assert_eq!(std::fs::metadata(root.join("g.txt")).unwrap().permissions().mode() & 0o777, 0o644);
        let outside = scratch("replace-out");
        std::fs::write(outside.join("t"), "t").unwrap();
        std::os::unix::fs::symlink(outside.join("t"), root.join("l")).unwrap();
        assert!(replace_file(&root, std::ffi::OsStr::new("l"), b"x").is_err());
        assert_eq!(std::fs::read(outside.join("t")).unwrap(), b"t");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn concurrent_replacements_keep_whole_values_and_exact_long_names() {
        use std::os::unix::ffi::OsStringExt;
        let root = scratch("concurrent-replace");
        let name = std::ffi::OsString::from_vec(vec![0xff; 255]);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8u8).map(|byte| {
            let root = root.clone(); let name = name.clone(); let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                replace_file(&root, &name, &vec![byte; 65536]).unwrap();
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        let bytes = std::fs::read(root.join(&name)).unwrap();
        assert_eq!(bytes.len(), 65536);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owned_dir_refuses_another_owner_and_links() {
        let root = scratch("own");
        let me = rustix::process::getuid().as_raw();
        let d = root.join("mine");
        owned_dir(&d, me, true).unwrap();
        assert!(owned_dir(&d, me + 1, false).is_err());
        std::os::unix::fs::symlink(&d, root.join("link")).unwrap();
        assert!(owned_dir(&root.join("link"), me, false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Read a bounded regular file without following any link, including intermediate components.
/// Missing files are distinct from unsafe paths and I/O failures.
pub fn read_file_beneath(root: &Path, rel: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let dir = match open_dir_beneath(root, rel.parent().unwrap_or(Path::new("")), false) {
        Ok(d) => d,
        Err(e) if e.chain().any(|e| e.downcast_ref::<Errno>() == Some(&Errno::NOENT)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let name = rel.file_name().context("no file name")?;
    let fd = match openat2(&dir, name, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty(), RESOLVE) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let st = fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode as RawMode) != FileType::RegularFile { bail!("not a regular file"); }
    let mut bytes = Vec::new();
    std::fs::File::from(fd).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { bail!("file exceeds {limit} bytes"); }
    Ok(Some(bytes))
}

/// Atomic private-file replacement anchored to directory descriptors throughout.
pub fn replace_private_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> Result<()> {
    use rustix::fs::{renameat, statat};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = open_dir_beneath(root, rel.parent().unwrap_or(Path::new("")), true)?;
    let name = rel.file_name().context("no file name")?;
    match statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) if FileType::from_raw_mode(st.st_mode as RawMode) != FileType::RegularFile => bail!("not a regular file"),
        Ok(_) | Err(Errno::NOENT) => {},
        Err(e) => return Err(e.into()),
    }
    let tmp = format!(".sill-credential-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let fd = openat2(&dir, tmp.as_str(), OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::from_raw_mode(0o600), RESOLVE)?;
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        renameat(&dir, tmp.as_str(), &dir, name)?;
        Ok(())
    })();
    if result.is_err() { let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty()); }
    result
}
