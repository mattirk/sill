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

/// Legacy sibling backups are internal state too. Match a single path component,
/// including non-UTF-8 suffixes, without interpreting the state base as a glob.
pub fn is_sill_backup(path: &Path, state_base: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.strip_prefix(state_base).ok().and_then(|p| p.components().next())
        .is_some_and(|c| c.as_os_str().as_bytes().starts_with(b"sill.bak-"))
}

/// Enumerate legacy backups at launch; unreadable state must not silently omit masks.
pub fn sill_backups(state_base: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(state_base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if is_sill_backup(&path, state_base) { paths.push(path); }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    #[test]
    fn legacy_backup_matching_is_component_scoped() {
        let base = Path::new("/state");
        assert!(is_sill_backup(Path::new("/state/sill.bak-20261103/sill.db"), base));
        assert!(!is_sill_backup(Path::new("/state/other/sill.bak-20261103"), base));
        assert!(!is_sill_backup(Path::new("/state/sill-backup"), base));
        assert!(!is_sill_backup(Path::new("/state-other/sill.bak-20261103"), base));
    }

    #[test]
    fn enumerates_only_backup_roots() {
        let base = std::env::temp_dir().join(format!("sill-backup-masks-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sill.bak-20261103")).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        assert_eq!(sill_backups(&base).unwrap(), vec![base.join("sill.bak-20261103")]);
        std::fs::remove_dir_all(base).unwrap();
    }
}
