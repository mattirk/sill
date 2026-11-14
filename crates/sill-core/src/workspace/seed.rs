//! A new workspace's files taken from the person's checkout (docs/parallel-coding.md §12, warm
//! folders): every file of the working tree but `.git`, sharing its data with the checkout
//! (a reflink: nothing is copied until one side changes) and keeping its times, so build tools
//! find their output and see unchanged sources as unchanged. A filesystem that cannot share
//! refuses the first file, and the workspace is made cold instead.
use std::fs::{File, FileTimes, OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

/// How a file's data gets to the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum How {
    /// Shared with the original (`FICLONE`); an error where the filesystem cannot.
    Share,
    /// Read and written: for tests on a filesystem that cannot share.
    #[cfg(test)]
    Bytes,
}

/// Copy the working tree at `from` into the existing folder `to`, without the top `.git`.
/// Links are copied as links, never followed; sockets, pipes and devices are left out, and so
/// is `to` itself where it lies inside `from` (a checkout holding sill's workspaces: the copy
/// would otherwise copy itself without end).
pub(super) fn copy(from: &Path, to: &Path, how: How) -> Result<()> {
    let (from, to) = (from.canonicalize()?, to.canonicalize()?);
    fill(&from, &to, how, &to, true)
}

fn fill(from: &Path, to: &Path, how: How, itself: &Path, top: bool) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if top && name == ".git" {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        if src == itself {
            continue;
        }
        let md = std::fs::symlink_metadata(&src)?;
        let kind = md.file_type();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if kind.is_dir() {
            std::fs::create_dir(&dst)?;
            fill(&src, &dst, how, itself, false)?;
            // after its entries: a folder the person cannot write still gets them, and its
            // time is not the copy's
            let dir = File::open(&dst)?;
            dir.set_permissions(Permissions::from_mode(md.permissions().mode()))?;
            dir.set_times(times(&md)?)?;
        } else if kind.is_file() {
            file(&src, &dst, &md, how).with_context(|| format!("copy {}", src.display()))?;
        }
    }
    Ok(())
}

fn times(md: &std::fs::Metadata) -> Result<FileTimes> {
    Ok(FileTimes::new().set_modified(md.modified()?).set_accessed(md.accessed()?))
}

fn file(src: &Path, dst: &Path, md: &std::fs::Metadata, how: How) -> Result<()> {
    let from = OpenOptions::new().read(true).custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32).open(src)?;
    let to = OpenOptions::new().write(true).create_new(true).mode(md.permissions().mode()).open(dst)?;
    match how {
        // nothing to share in an empty file, and some filesystems refuse to
        How::Share if md.len() == 0 => {}
        How::Share => rustix::fs::ioctl_ficlone(&to, &from).context("share the file's data")?,
        #[cfg(test)]
        How::Bytes => {
            std::io::copy(&mut &from, &mut &to)?;
        }
    }
    // as the original has them, whatever the umask made of the mode
    to.set_permissions(Permissions::from_mode(md.permissions().mode()))?;
    to.set_times(times(md)?)?;
    Ok(())
}
