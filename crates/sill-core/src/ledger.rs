//! The change ledger (SPEC §4.4): decode a session's overlay upper trees into a list of changes
//! against the host, the way `podman diff` reads a running container's upper layer.
//!
//! Upper-tree encoding (unprivileged overlayfs, `userxattr`):
//! - a character device 0:0 is a **whiteout**: the path was deleted;
//! - a directory with xattr `user.overlay.opaque = "y"` replaces the host directory wholesale;
//! - any other directory is just a copy-up container for changes below it (or a new directory);
//! - files and symlinks are the new content (copy-up preserves the host mtime, so mtime alone proves
//!   nothing; content is compared).
//!
//! Reading the upper tree while the overlay is mounted is fine; modifying it is not — keep writes the
//! host, revert goes through the jail (SPEC §4.4). Nothing here writes.

use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::jail::Layer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Modified,
    Deleted,
    /// File type differs between host and upper (file ↔ symlink ↔ dir).
    TypeChanged,
    /// Directory replaced wholesale (opaque); its new contents are listed as Added.
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct Change {
    /// Host path.
    pub path: PathBuf,
    /// Path inside the upper tree (absent for deletions it is the whiteout node).
    pub upper: PathBuf,
    pub kind: Kind,
    pub file_type: FileType,
    pub size: u64,
    /// The host changed this path after the session started (SPEC §4.4 conflicts).
    pub conflict: bool,
    /// Matches a `ledger_ignore` pattern: recorded, not hashed per turn, never auto-kept.
    pub ignored: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Session start; host paths with a newer mtime are conflicts.
    pub session_start: Option<SystemTime>,
    /// Relative patterns such as `target/`, `.cargo/registry/`.
    pub ignore: Vec<String>,
    /// Paths that are masked or bound directly: nothing below them is a change.
    pub skip: Vec<PathBuf>,
}

/// Scan every layer's upper tree.
pub fn scan(layers: &[Layer], opts: &ScanOptions) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    for l in layers {
        if !l.upper.is_dir() {
            continue;
        }
        walk(&l.upper, &l.host, &l.upper, opts, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(upper_root: &Path, host_root: &Path, dir: &Path, opts: &ScanOptions, out: &mut Vec<Change>) -> Result<()> {
    let rd = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    let mut entries: Vec<_> = rd.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let upper = e.path();
        let rel = upper.strip_prefix(upper_root).expect("below upper root");
        let host = host_root.join(rel);
        if opts.skip.iter().any(|s| host.starts_with(s)) {
            continue;
        }
        let md = upper.symlink_metadata()?;
        let ft = md.file_type();
        let host_md = host.symlink_metadata().ok();
        // Conflict: the host changed after the agent's copy was written (host mtime newer than the
        // upper entry's), or after the session started when a start time is known. Copy-up preserves
        // the host's mtime, the agent's write then sets the upper mtime, so a later host edit is newer.
        let conflict = match &host_md {
            Some(h) => {
                let host_m = h.modified().ok();
                let upper_m = md.modified().ok();
                let newer_than_upper = matches!((host_m, upper_m), (Some(hm), Some(um)) if hm > um);
                let newer_than_start = matches!((host_m, opts.session_start), (Some(hm), Some(start)) if hm > start);
                newer_than_upper || newer_than_start
            }
            None => false,
        };
        let ignored = is_ignored(rel, &opts.ignore);

        if ft.is_char_device() && md.rdev() == 0 {
            // whiteout: a deletion of something on the host; a file created and deleted in-session
            // leaves no whiteout (overlay removes the upper entry) — if it exists, the host had it
            if let Some(h) = host_md {
                out.push(Change {
                    path: host,
                    upper,
                    kind: Kind::Deleted,
                    file_type: file_type_of(&h.file_type()),
                    size: h.len(),
                    conflict,
                    ignored,
                });
            }
            continue;
        }

        if ft.is_dir() {
            let opaque = is_opaque(&upper);
            match &host_md {
                Some(h) if h.is_dir() && !opaque => {} // copy-up container, nothing to report itself
                Some(h) if h.is_dir() => out.push(Change {
                    path: host.clone(),
                    upper: upper.clone(),
                    kind: Kind::Replaced,
                    file_type: FileType::Dir,
                    size: 0,
                    conflict,
                    ignored,
                }),
                Some(h) => out.push(Change {
                    path: host.clone(),
                    upper: upper.clone(),
                    kind: Kind::TypeChanged,
                    file_type: file_type_of(&h.file_type()),
                    size: 0,
                    conflict,
                    ignored,
                }),
                None => out.push(Change {
                    path: host.clone(),
                    upper: upper.clone(),
                    kind: Kind::Added,
                    file_type: FileType::Dir,
                    size: 0,
                    conflict,
                    ignored,
                }),
            }
            // below an opaque or new dir the host counterpart doesn't apply: compare against nothing
            if opaque || !host_md.as_ref().map(|h| h.is_dir()).unwrap_or(false) {
                walk_new(upper_root, host_root, &upper, opts, out)?;
            } else {
                walk(upper_root, host_root, &upper, opts, out)?;
            }
            continue;
        }

        let file_type = file_type_of(&ft);
        let kind = match &host_md {
            None => Kind::Added,
            Some(h) if file_type_of(&h.file_type()) != file_type => Kind::TypeChanged,
            Some(h) => {
                if same_content(&upper, &md, &host, h)? {
                    continue; // identical to the host: hidden (SPEC §4.4)
                }
                Kind::Modified
            }
        };
        out.push(Change { path: host, upper, kind, file_type, size: md.len(), conflict, ignored });
    }
    Ok(())
}

/// Below a replaced or new directory: everything is Added, no host comparison.
fn walk_new(upper_root: &Path, host_root: &Path, dir: &Path, opts: &ScanOptions, out: &mut Vec<Change>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let upper = e.path();
        let rel = upper.strip_prefix(upper_root).expect("below upper root");
        let host = host_root.join(rel);
        let md = upper.symlink_metadata()?;
        if md.file_type().is_char_device() && md.rdev() == 0 {
            continue;
        }
        let ignored = is_ignored(rel, &opts.ignore);
        out.push(Change {
            path: host,
            upper: upper.clone(),
            kind: Kind::Added,
            file_type: file_type_of(&md.file_type()),
            size: md.len(),
            conflict: false,
            ignored,
        });
        if md.is_dir() {
            walk_new(upper_root, host_root, &upper, opts, out)?;
        }
    }
    Ok(())
}

fn file_type_of(ft: &std::fs::FileType) -> FileType {
    if ft.is_dir() {
        FileType::Dir
    } else if ft.is_symlink() {
        FileType::Symlink
    } else if ft.is_file() {
        FileType::File
    } else {
        FileType::Other
    }
}

fn is_opaque(dir: &Path) -> bool {
    let mut buf = [0u8; 8];
    match rustix::fs::lgetxattr(dir, "user.overlay.opaque", &mut buf) {
        Ok(n) => &buf[..n] == b"y",
        Err(_) => false,
    }
}

/// `target/` matches any path with a `target` directory component; `.cargo/registry/` needs the two
/// consecutive components. A pattern without a trailing slash matches a file or directory name.
pub fn is_ignored(rel: &Path, patterns: &[String]) -> bool {
    let comps: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    patterns.iter().any(|pat| {
        let parts: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return false;
        }
        comps.windows(parts.len()).any(|w| w == parts.as_slice())
    })
}

fn same_content(a: &Path, am: &std::fs::Metadata, b: &Path, bm: &std::fs::Metadata) -> Result<bool> {
    if am.file_type().is_symlink() {
        return Ok(std::fs::read_link(a)? == std::fs::read_link(b)?);
    }
    if !am.is_file() || !bm.is_file() || am.len() != bm.len() {
        return Ok(false);
    }
    if am.mode() & 0o7777 != bm.mode() & 0o7777 {
        return Ok(false);
    }
    let mut fa = std::fs::File::open(a)?;
    let mut fb = std::fs::File::open(b)?;
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let na = fa.read(&mut ba)?;
        let nb = fb.read(&mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

impl Kind {
    pub fn letter(self) -> char {
        match self {
            Kind::Added => 'A',
            Kind::Modified => 'M',
            Kind::Deleted => 'D',
            Kind::TypeChanged => 'T',
            Kind::Replaced => 'R',
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_patterns() {
        let p = vec!["target/".to_string(), ".cargo/registry/".to_string(), "*.log".to_string()];
        assert!(is_ignored(Path::new("x/target/debug/a"), &p));
        assert!(is_ignored(Path::new("home/me/.cargo/registry/src"), &p));
        assert!(!is_ignored(Path::new("home/me/.cargo/bin/x"), &p));
        assert!(!is_ignored(Path::new("home/me/targets/x"), &p));
    }
}
