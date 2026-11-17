//! Open images in the shim's mount namespace, never the daemon's host namespace.
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{FileType, Mode, OFlags, ResolveFlags, fstat, open, openat2};
use rustix::io::Errno;

pub fn open_image(bytes: &[u8]) -> Result<OwnedFd, Errno> {
    open_beneath(Path::new("/"), Path::new(std::ffi::OsStr::from_bytes(bytes)))
}

fn open_beneath(root: &Path, path: &Path) -> Result<OwnedFd, Errno> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Errno::INVAL);
    }
    // The host-view mount and shim internals are not image sources. Refusing symlinks
    // below also prevents reaching them indirectly, or following /proc magic links.
    if path.starts_with("/.sill") {
        return Err(Errno::ACCESS);
    }
    let image = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp"));
    if !image {
        return Err(Errno::INVAL);
    }
    let root = open(root, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
    let file = openat2(
        &root,
        path.strip_prefix("/").map_err(|_| Errno::INVAL)?,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
    )?;
    if FileType::from_raw_mode(fstat(&file)?.st_mode) != FileType::RegularFile {
        return Err(Errno::OPNOTSUPP);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn images_use_the_session_root_and_refuse_escape_and_special_files() {
        let root = std::env::temp_dir().join(format!("sill-image-root-{}", std::process::id()));
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        let bytes = b"\xff\xd8\x00\xfe\x80";
        std::fs::write(root.join("tmp/image.jpg"), bytes).unwrap();
        let mut file = std::fs::File::from(open_beneath(&root, Path::new("/tmp/image.jpg")).unwrap());
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, bytes);
        assert_eq!(open_beneath(&root, Path::new("/tmp/missing.jpg")).unwrap_err(), Errno::NOENT);
        for path in ["tmp/image.jpg", "/tmp/../tmp/image.jpg", "/tmp/file.txt"] {
            assert_eq!(open_beneath(&root, Path::new(path)).unwrap_err(), Errno::INVAL);
        }
        assert_eq!(open_beneath(&root, Path::new("/.sill/host/tmp/image.jpg")).unwrap_err(), Errno::ACCESS);
        std::os::unix::fs::symlink("image.jpg", root.join("tmp/link.jpg")).unwrap();
        std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
        for path in ["/tmp/link.jpg", "/escape/tmp/image.jpg"] {
            assert_eq!(open_beneath(&root, Path::new(path)).unwrap_err(), Errno::LOOP);
        }
        rustix::fs::mkfifoat(rustix::fs::CWD, root.join("tmp/pipe.jpg"), Mode::RUSR | Mode::WUSR).unwrap();
        std::fs::create_dir(root.join("tmp/directory.jpg")).unwrap();
        for path in ["/tmp/pipe.jpg", "/tmp/directory.jpg"] {
            assert_eq!(open_beneath(&root, Path::new(path)).unwrap_err(), Errno::OPNOTSUPP);
        }
        let exact = std::ffi::OsStr::from_bytes(b"tmp/non-utf8-\xff.jpg");
        std::fs::write(root.join(exact), bytes).unwrap();
        assert!(open_beneath(&root, Path::new("/").join(exact).as_path()).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
