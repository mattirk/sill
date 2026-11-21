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

/// A mask must also hide future matches. Wildcards therefore hide their literal parent
/// tree; exact paths are retained even when absent. Never expand against today's files.
pub fn mask_root(pattern: &str, home: &Path) -> PathBuf {
    let expanded = expand_home(pattern, home);
    let mut root = PathBuf::new();
    for part in expanded.components() {
        if part.as_os_str().to_string_lossy().contains(['*', '?', '[']) {
            break;
        }
        root.push(part);
    }
    root
}

/// Name of the upper/work directory for a host mount point or top-level directory.
/// `/etc` → `etc`, `/mnt/disk` → `mnt%disk`. Flat on purpose: one overlay's upper must never sit inside
/// another's (SPEC §4.9).
pub fn layer_name(mount_point: &Path) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = mount_point.as_os_str().as_bytes();
    let trimmed = bytes.iter().position(|b| *b != b'/').map(|i| &bytes[i..]).unwrap_or_default();
    if trimmed.is_empty() { "root".into() } else { std::ffi::OsString::from_vec(trimmed.iter().map(|b| if *b == b'/' { b'%' } else { *b }).collect()) }
}

/// Whether `child` is `parent` or lies below it.
pub fn is_under(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

/// Legacy sibling backups are internal state too. Match a single path component,
/// including non-UTF-8 suffixes, without interpreting the state base as a glob.
pub fn is_silli_backup(path: &Path, state_base: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.strip_prefix(state_base).ok().and_then(|p| p.components().next()).is_some_and(|c| c.as_os_str().as_bytes().starts_with(b"silli.bak-"))
}

/// Enumerate legacy backups at launch; unreadable state must not silently omit masks.
pub fn silli_backups(state_base: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(state_base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if is_silli_backup(&path, state_base) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Silli's state directory of a home (`$XDG_STATE_HOME/silli`).
pub fn silli_state(home: &Path) -> PathBuf {
    state_base(home).join("silli")
}

fn state_base(home: &Path) -> PathBuf {
    std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/state"))
}

/// Silli's own files, hidden in every jail whatever its policy says and never covered by a pin: a
/// session's mounts, allowed domains and binaries are made of them. Its configuration, its state
/// directory and that directory's backups, and the user's journal — readable by ACL, it holds
/// every session's proxy lines and agent stderr (security.md S15).
pub fn silli_own(home: &Path) -> std::io::Result<Vec<PathBuf>> {
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let state = state_base(home);
    let mut own = vec![config.join("silli"), state.join("silli"), PathBuf::from("/var/log/journal")];
    own.extend(silli_backups(&state)?);
    Ok(own)
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    #[test]
    fn legacy_backup_matching_is_component_scoped() {
        let base = Path::new("/state");
        assert!(is_silli_backup(Path::new("/state/silli.bak-20261103/silli.db"), base));
        assert!(!is_silli_backup(Path::new("/state/other/silli.bak-20261103"), base));
        assert!(!is_silli_backup(Path::new("/state/silli-backup"), base));
        assert!(!is_silli_backup(Path::new("/state-other/silli.bak-20261103"), base));
    }

    #[test]
    fn enumerates_only_backup_roots() {
        let base = std::env::temp_dir().join(format!("silli-backup-masks-{}", std::process::id()));
        std::fs::create_dir_all(base.join("silli.bak-20261103")).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        assert_eq!(silli_backups(&base).unwrap(), vec![base.join("silli.bak-20261103")]);
        std::fs::remove_dir_all(base).unwrap();
    }
}
