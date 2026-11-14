//! Path helpers: `~` expansion, glob expansion against the host, escaped upper-tree names.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/` to `$HOME`. Other users' homes (`~bob`) are not supported.
pub fn expand_home(p: &str, home: &Path) -> PathBuf {
    if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Expand a policy path pattern (may contain `~` and glob characters) to the existing host paths it
/// matches. A pattern without glob characters yields itself only if it exists.
pub fn expand_pattern(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let expanded = expand_home(pattern, home);
    let s = expanded.to_string_lossy();
    if !s.contains(['*', '?', '[']) {
        return if expanded.symlink_metadata().is_ok() { vec![expanded] } else { vec![] };
    }
    match glob::glob(&s) {
        Ok(paths) => paths.filter_map(Result::ok).collect(),
        Err(_) => vec![],
    }
}

/// Name of the upper/work directory for a host mount point or top-level directory.
/// `/etc` → `etc`, `/mnt/disk` → `mnt%disk`. Flat on purpose: one overlay's upper must never sit inside
/// another's (SPEC §4.9).
pub fn layer_name(mount_point: &Path) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = mount_point.as_os_str().as_bytes();
    let trimmed = bytes.iter().position(|b| *b != b'/').map(|i| &bytes[i..]).unwrap_or_default();
    if trimmed.is_empty() { "root".into() } else {
        std::ffi::OsString::from_vec(trimmed.iter().map(|b| if *b == b'/' { b'%' } else { *b }).collect())
    }
}

/// Whether `child` is `parent` or lies below it.
pub fn is_under(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}
