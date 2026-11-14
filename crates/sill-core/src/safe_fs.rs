//! Host-side writes below a directory the agent can reach (a pin, a private scratch dir, its own
//! home): every component is resolved without following symlinks and without leaving the root
//! (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`), so a link the agent planted cannot
//! steer the shelf's write anywhere else on the host; a FIFO cannot stall it (security.md S6, S9,
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
    // NONBLOCK: a FIFO planted here would otherwise hold the shelf until something reads it
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

/// Copy the regular files below `src` — a tree the shelf wrote itself — to the same paths below
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
