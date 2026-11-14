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

use anyhow::Result;

use crate::jail::Layer;

pub use sill_proto::wire::{Change, FileType, Kind};

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
    // Mount setup can create parents for absent masked targets. Hide an added
    // parent only when it contains no reportable change after applying skips.
    let empty_mount_parents: Vec<PathBuf> = out.iter().filter(|c| {
        c.kind == Kind::Added && c.file_type == FileType::Dir
            && opts.skip.iter().any(|s| s.starts_with(&c.path))
            && !out.iter().any(|child| child.path != c.path && child.path.starts_with(&c.path)
                && !(child.kind == Kind::Added && child.file_type == FileType::Dir
                    && opts.skip.iter().any(|s| s.starts_with(&child.path))))
    }).map(|c| c.path.clone()).collect();
    out.retain(|c| !empty_mount_parents.contains(&c.path));
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The entries of an upper directory, sorted; an unreadable one (a directory the agent made
/// mode 000) is skipped with a warning instead of ending the whole scan (SPEC §4.11 #17).
fn entries(dir: &Path) -> Vec<std::fs::DirEntry> {
    let mut v: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(Result::ok).collect(),
        Err(e) => {
            tracing::warn!("ledger: {} not read ({e}); its changes are not listed", dir.display());
            return Vec::new();
        }
    };
    v.sort_by_key(|e| e.file_name());
    v
}

fn walk(upper_root: &Path, host_root: &Path, dir: &Path, opts: &ScanOptions, out: &mut Vec<Change>) -> Result<()> {
    for e in entries(dir) {
        let upper = e.path();
        let rel = upper.strip_prefix(upper_root).expect("below upper root");
        let host = host_root.join(rel);
        if opts.skip.iter().any(|s| host.starts_with(s)) {
            continue;
        }
        let Ok(md) = upper.symlink_metadata() else { continue };
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
                // unreadable on either side: shown as modified rather than hidden
                if same_content(&upper, &md, &host, h).unwrap_or(false) {
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
    for e in entries(dir) {
        let upper = e.path();
        let rel = upper.strip_prefix(upper_root).expect("below upper root");
        let host = host_root.join(rel);
        if opts.skip.iter().any(|s| host.starts_with(s)) {
            continue;
        }
        let Ok(md) = upper.symlink_metadata() else { continue };
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

pub(crate) fn is_opaque(dir: &Path) -> bool {
    let mut buf = [0u8; 8];
    match rustix::fs::lgetxattr(dir, "user.overlay.opaque", &mut buf) {
        Ok(n) => &buf[..n] == b"y",
        Err(_) => false,
    }
}

/// `target/` matches any path with a `target` directory component; `.cargo/registry/` needs the two
/// consecutive components. A pattern without a trailing slash matches a file or directory name.
pub fn is_ignored(rel: &Path, patterns: &[String]) -> bool {
    let comps: Vec<&std::ffi::OsStr> = rel.iter().collect();
    patterns.iter().any(|pat| {
        let parts: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return false;
        }
        comps.windows(parts.len()).any(|w| w.iter().zip(&parts).all(|(actual, expected)| *actual == *expected))
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_directory_respects_masks_and_hides_mount_parents() {
        let base = std::env::temp_dir().join(format!("sill-ledger-masks-{}", std::process::id()));
        let upper = base.join("upper");
        let host = base.join("host");
        std::fs::create_dir_all(upper.join(".cargo")).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(upper.join(".cargo/credentials"), "").unwrap();
        let layers = [Layer { host: host.clone(), upper: upper.clone(), work: base.join("work") }];
        let opts = ScanOptions { skip: vec![host.join(".cargo/credentials")], ..Default::default() };
        assert!(scan(&layers, &opts).unwrap().is_empty());
        std::fs::write(upper.join(".cargo/config.toml"), "real change").unwrap();
        let changes = scan(&layers, &opts).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c.path == host.join(".cargo/config.toml")));
        assert!(!changes.iter().any(|c| c.path == host.join(".cargo/credentials")));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn non_utf8_components_do_not_disappear_from_ignore_matching() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(b".cargo/\xff/registry".to_vec()));
        assert!(!is_ignored(&path, &[".cargo/registry/".into()]));
    }

    #[test]
    fn ignore_patterns() {
        let p = vec!["target/".to_string(), ".cargo/registry/".to_string(), "*.log".to_string()];
        assert!(is_ignored(Path::new("x/target/debug/a"), &p));
        assert!(is_ignored(Path::new("home/me/.cargo/registry/src"), &p));
        assert!(!is_ignored(Path::new("home/me/.cargo/bin/x"), &p));
        assert!(!is_ignored(Path::new("home/me/targets/x"), &p));
    }
}
