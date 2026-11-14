//! Known disposable build outputs. All traversal is relative to directory descriptors,
//! without following symlinks or crossing mounts. Session overlays are never visited.
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags};
use std::{
    os::fd::{AsFd, OwnedFd},
    path::Path,
};

pub const BUILD_PARENT: &str = "private/home%sill%.cache/sill-dev";
const FLAGS: OFlags = OFlags::DIRECTORY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_XDEV);

pub fn space(path: &Path) -> Result<sill_proto::storage::DiskSpace> {
    let s = fs::statvfs(path).with_context(|| format!("reading space on {}", path.display()))?;
    let unit = s.f_frsize;
    Ok(sill_proto::storage::DiskSpace {
        total: s.f_blocks.saturating_mul(unit),
        used: s.f_blocks.saturating_sub(s.f_bfree).saturating_mul(unit),
        available: s.f_bavail.saturating_mul(unit),
    })
}

fn parent(root: &Path, session: &str) -> Result<OwnedFd> {
    ensure!(crate::session::valid_id(session), "invalid session id");
    let root_fd = fs::open(root, FLAGS, Mode::empty())?;
    Ok(fs::openat2(
        &root_fd,
        Path::new(session).join(BUILD_PARENT),
        FLAGS,
        Mode::empty(),
        RESOLVE,
    )?)
}
fn missing(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|e| e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT))
}

/// Missing caches are zero; symlinked paths and unreadable caches are errors, not empty.
pub fn cache_bytes(root: &Path, session: &str) -> Result<u64> {
    let parent = match parent(root, session) {
        Ok(fd) => fd,
        Err(e) if missing(&e) => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut bytes = 0;
    let mut budget = 1_000_000;
    let mut seen = std::collections::HashSet::new();
    for name in ["target", "target.sill-clearing"] {
        match fs::openat2(&parent, name, FLAGS, Mode::empty(), RESOLVE) {
            Ok(fd) => bytes += measure(&fd, &mut budget, &mut seen, 0)?,
            Err(rustix::io::Errno::NOENT) => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(bytes)
}
fn measure(
    fd: &OwnedFd,
    budget: &mut usize,
    seen: &mut std::collections::HashSet<(u64, u64)>,
    depth: usize,
) -> Result<u64> {
    ensure!(depth < 64, "build cache is too deeply nested");
    let mut bytes = 0;
    for entry in Dir::read_from(fd)? {
        ensure!(*budget > 0, "build cache exceeds scan limit");
        *budget -= 1;
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let st = match fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(rustix::io::Errno::NOENT) => continue, // active cache changed during scan
            Err(e) => return Err(e.into()),
        };
        if seen.insert((st.st_dev, st.st_ino)) {
            bytes += st.st_blocks.max(0) as u64 * 512;
        }
        if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
            let child = fs::openat2(fd, name, FLAGS, Mode::empty(), RESOLVE)?;
            bytes += measure(&child, budget, seen, depth + 1)?;
        }
    }
    Ok(bytes)
}

/// Caller serializes this with session startup and checks inactivity under that lock.
/// Deletion uses the returned descriptor after releasing the startup lock. A new jail can
/// create a fresh target while the old one is removed. An interrupted cleanup is resumable.
pub fn detach(root: &Path, session: &str) -> Result<Option<OwnedFd>> {
    let parent = match parent(root, session) {
        Ok(fd) => fd,
        Err(e) if missing(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    match fs::openat2(
        &parent,
        "target.sill-clearing",
        FLAGS,
        Mode::empty(),
        RESOLVE,
    ) {
        Ok(_) => return Ok(Some(parent)),
        Err(rustix::io::Errno::NOENT) => (),
        Err(e) => return Err(e.into()),
    }
    match fs::openat2(&parent, "target", FLAGS, Mode::empty(), RESOLVE) {
        Ok(_) => (),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    fs::renameat_with(
        &parent,
        "target",
        &parent,
        "target.sill-clearing",
        fs::RenameFlags::NOREPLACE,
    )?;
    Ok(Some(parent))
}

pub fn remove_detached(parent: &OwnedFd) -> Result<()> {
    let fd = fs::openat2(
        parent,
        "target.sill-clearing",
        FLAGS,
        Mode::empty(),
        RESOLVE,
    )?;
    remove_contents(&fd, 0)?;
    fs::unlinkat(parent, "target.sill-clearing", AtFlags::REMOVEDIR)?;
    Ok(())
}
fn remove_contents(fd: &OwnedFd, depth: usize) -> Result<()> {
    ensure!(depth < 64, "build cache is too deeply nested");
    for entry in Dir::read_from(fd.as_fd())? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let st = fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
            let child = fs::openat2(fd, name, FLAGS, Mode::empty(), RESOLVE)?;
            remove_contents(&child, depth + 1)?;
            fs::unlinkat(fd, name, AtFlags::REMOVEDIR)?;
        } else {
            fs::unlinkat(fd, name, AtFlags::empty())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink, path::PathBuf};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("sill-storage-{:032x}", rand::random::<u128>()));
            fs::create_dir_all(root.join("one").join(BUILD_PARENT).join("target/debug")).unwrap();
            Self(root)
        }
        fn target(&self) -> PathBuf {
            self.0.join("one").join(BUILD_PARENT).join("target")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cleanup_preserves_new_builds_other_sessions_source_and_overlays() {
        let f = Fixture::new();
        fs::write(f.target().join("debug/old"), vec![1; 8192]).unwrap();
        assert!(cache_bytes(&f.0, "one").unwrap() >= 8192);
        for rel in [
            "one/upper/home/source",
            "one/private/home%sill%.cache/sill-dev/cargo/toolchain",
            "two/private/cache",
        ] {
            let path = f.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "keep").unwrap();
        }
        let detached = detach(&f.0, "one").unwrap().unwrap();
        fs::create_dir_all(f.target()).unwrap();
        fs::write(f.target().join("new"), "new build").unwrap();
        remove_detached(&detached).unwrap();
        assert_eq!(
            fs::read_to_string(f.target().join("new")).unwrap(),
            "new build"
        );
        for rel in [
            "one/upper/home/source",
            "one/private/home%sill%.cache/sill-dev/cargo/toolchain",
            "two/private/cache",
        ] {
            assert_eq!(fs::read_to_string(f.0.join(rel)).unwrap(), "keep");
        }
    }

    #[test]
    fn links_inside_target_are_unlinked_without_following_them() {
        let f = Fixture::new();
        let outside = f.0.join("source");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "source").unwrap();
        symlink(&outside, f.target().join("link")).unwrap();
        let detached = detach(&f.0, "one").unwrap().unwrap();
        remove_detached(&detached).unwrap();
        assert!(outside.join("keep").exists());
        assert!(!f.target().exists());
    }

    #[test]
    fn symlinked_parent_or_target_is_refused() {
        let f = Fixture::new();
        let actual = f.0.join("actual");
        fs::rename(f.target(), &actual).unwrap();
        symlink(&actual, f.target()).unwrap();
        assert!(detach(&f.0, "one").is_err());
        assert!(cache_bytes(&f.0, "one").is_err());
        let parent = f.target().parent().unwrap().to_path_buf();
        let moved = f.0.join("moved");
        fs::rename(&parent, &moved).unwrap();
        symlink(&moved, &parent).unwrap();
        assert!(detach(&f.0, "one").is_err());
        assert!(actual.is_dir());
    }

    #[test]
    fn interrupted_cleanup_can_resume_without_losing_a_new_target() {
        let f = Fixture::new();
        fs::write(f.target().join("old"), "old").unwrap();
        drop(detach(&f.0, "one").unwrap());
        fs::create_dir(f.target()).unwrap();
        fs::write(f.target().join("new"), "new").unwrap();
        let parent = detach(&f.0, "one").unwrap().unwrap();
        remove_detached(&parent).unwrap();
        assert!(f.target().join("new").exists());
        assert!(cache_bytes(&f.0, "one").unwrap() > 0);
        remove_detached(&detach(&f.0, "one").unwrap().unwrap()).unwrap();
        assert_eq!(cache_bytes(&f.0, "one").unwrap(), 0);
    }

    #[test]
    fn invalid_sessions_and_missing_caches_do_not_create_state() {
        let f = Fixture::new();
        assert!(detach(&f.0, "../one").is_err());
        assert_eq!(cache_bytes(&f.0, "missing").unwrap(), 0);
        assert!(detach(&f.0, "missing").unwrap().is_none());
        assert!(!f.0.join("missing").exists());
    }

    #[test]
    fn filesystem_space_reports_available_blocks() {
        let space = space(&std::env::temp_dir()).unwrap();
        assert!(space.total > 0);
        assert!(space.used <= space.total);
        assert!(space.available <= space.total - space.used);
    }
}
