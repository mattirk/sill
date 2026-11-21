//! A session-private lower tree for a reviewed pin, including folders on Mac shares.
//! Refresh visible host files at launch, retaining the conflict baseline of pending edits.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::jail::Layer;

pub fn directory(state: &Path, pin: &Path) -> PathBuf {
    state.join("staged-pins").join(Sha256::digest(pin.as_os_str().as_encoded_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

pub fn layer(state: &Path, pin: &Path) -> Layer {
    let dir = directory(state, pin);
    Layer { host: pin.to_owned(), upper: dir.join("upper"), work: dir.join("work") }
}

/// Capture only entries visible at this path. Directory descriptors keep traversal
/// anchored if a source directory is concurrently replaced by a symlink.
fn copy_visible(src: &Path, dst: &Path, logical: &Path, hidden: &[PathBuf], ancestors: &mut Vec<(u64, u64)>) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    if hidden.iter().any(|mask| logical.starts_with(mask)) {
        // The read-only reference view needs a mount point for the jail mask.
        // Inspect only the entry type. Never open a masked file or enumerate a masked directory.
        if src.symlink_metadata()?.is_dir() {
            fs::create_dir(dst)?;
        } else {
            fs::File::create_new(dst)?;
        }
        return Ok(());
    }
    let meta = src.symlink_metadata()?;
    if meta.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        return Ok(());
    }
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    if meta.is_dir() {
        let source = fs::OpenOptions::new().read(true).custom_flags((flags | rustix::fs::OFlags::DIRECTORY).bits() as i32).open(src)?;
        let opened = source.metadata()?;
        let id = (opened.dev(), opened.ino());
        ensure!(!ancestors.contains(&id), "mount cycle at {}", logical.display());
        ancestors.push(id);
        fs::create_dir(dst)?;
        for entry in fs::read_dir(format!("/proc/self/fd/{}", source.as_raw_fd()))? {
            let entry = entry?;
            let name = entry.file_name();
            copy_visible(&entry.path(), &dst.join(&name), &logical.join(&name), hidden, ancestors)
                .with_context(|| format!("staging {}", logical.join(name).display()))?;
        }
        ancestors.pop();
    } else if meta.is_file() {
        let mut source = fs::OpenOptions::new().read(true).custom_flags(flags.bits() as i32).open(src)?;
        ensure!(source.metadata()?.is_file(), "file changed type during staging");
        let mut dest = fs::File::create_new(dst)?;
        std::io::copy(&mut source, &mut dest)?;
    } else {
        anyhow::bail!("cannot snapshot special file {}", logical.display());
    }
    fs::File::open(dst)?.set_times(fs::FileTimes::new().set_modified(meta.modified()?))?;
    fs::set_permissions(dst, meta.permissions())?;
    Ok(())
}

fn remove_entry(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => {
            // Private snapshots can contain read-only source directories.
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            for child in fs::read_dir(path)? {
                remove_entry(&child?.path())?;
            }
            fs::remove_dir(path)?;
        }
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Keep records older than a clean refresh no longer describe the host baseline.
pub fn baseline_keeps(dir: &Path) -> Vec<crate::keep::Kept> {
    crate::keep::list(dir).into_iter().filter(|k| !dir.join(&k.id).join("refreshed").exists()).collect()
}

// Applied and discarded regular files can still have an upper inode. Remove clean
// copies while the jail is stopped, otherwise they hide the freshly captured host file.
fn collect_clean(upper: &Path, old: &Path, logical: &Path, kept: &[crate::keep::Kept], clean_paths: &mut std::collections::BTreeSet<PathBuf>) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let m = match upper.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let last = kept.iter().rev().find(|k| k.path == logical && k.undone_at.is_none());
    if m.is_dir() {
        let baseline_dir = old.symlink_metadata().is_ok_and(|p| p.is_dir() && p.permissions().mode() == m.permissions().mode());
        if !crate::ledger::is_opaque(upper) && (baseline_dir || last.is_some_and(|k| k.what == "directory")) {
            let mut empty = true;
            for child in fs::read_dir(upper)? {
                let child = child?;
                let name = child.file_name();
                collect_clean(&child.path(), &old.join(&name), &logical.join(name), kept, clean_paths)?;
                empty &= clean_paths.contains(&child.path());
            }
            if empty {
                clean_paths.insert(upper.to_owned());
            }
        }
        return Ok(());
    }
    let clean = if let Some(k) = last {
        if let Some(hash) = &k.hash {
            m.is_file() && crate::keep::sha256_file(upper)? == *hash
        } else {
            k.what == "removed" && m.file_type().is_char_device() && m.rdev() == 0
        }
    } else {
        matches(old, upper)?
    };
    if clean {
        clean_paths.insert(upper.to_owned());
    }
    Ok(())
}

// Preserve the comparison point for pending edits. Plain upper directories
// generally just hold changed children; opaque and type-changed entries cover a subtree.
fn retain_pending(upper: &Path, old: &Path, fresh: &Path, host: &Path, hidden: &[PathBuf], clean: &std::collections::BTreeSet<PathBuf>) -> Result<()> {
    if clean.contains(upper) || hidden.iter().any(|p| host.starts_with(p)) {
        return Ok(());
    }
    let upper_meta = match upper.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let prior = match old.symlink_metadata() {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if upper_meta.is_dir()
        && !crate::ledger::is_opaque(upper)
        && prior.as_ref().is_some_and(|m| m.is_dir())
        && fresh.symlink_metadata().is_ok_and(|m| m.is_dir())
    {
        let permissions = fresh.metadata()?.permissions();
        fs::set_permissions(fresh, fs::Permissions::from_mode(permissions.mode() | 0o700))?;
        for e in fs::read_dir(upper)? {
            let e = e?;
            let n = e.file_name();
            retain_pending(&e.path(), &old.join(&n), &fresh.join(&n), &host.join(&n), hidden, clean)?;
        }
        fs::set_permissions(fresh, permissions)?;
    } else {
        remove_entry(fresh)?;
        if prior.is_some() {
            copy_visible(old, fresh, host, hidden, &mut vec![])?;
        }
    }
    Ok(())
}

fn publish(next: &Path, current: &Path) -> Result<()> {
    if current.try_exists()? {
        rustix::fs::renameat_with(rustix::fs::CWD, next, rustix::fs::CWD, current, rustix::fs::RenameFlags::EXCHANGE)?;
    } else {
        fs::rename(next, current)?;
    }
    Ok(())
}

pub fn prepare(state: &Path, pin: &Path, hidden: &[PathBuf], upper: &Path) -> Result<()> {
    let dir = directory(state, pin);
    ensure!(!state.starts_with(pin), "cannot stage {} containing session storage", pin.display());
    ensure!(!hidden.iter().any(|mask| pin.starts_with(mask)), "cannot stage hidden pin {}", pin.display());
    fs::create_dir_all(&dir)?;
    let temp = dir.join(format!("copy-{:032x}", rand::random::<u128>()));
    fs::create_dir(&temp)?;
    let lower = dir.join("lower");
    let next = temp.join("lower");
    let reference = dir.join("reference");
    let kept_dir = state.join("kept");
    let kept = baseline_keeps(&kept_dir);
    let result = (|| -> Result<()> {
        copy_visible(pin, &next, pin, hidden, &mut vec![])?;
        let mut clean = std::collections::BTreeSet::new();
        if lower.try_exists()? {
            let old = if reference.is_dir() { &reference } else { &lower };
            collect_clean(upper, old, pin, &kept, &mut clean)?;
            retain_pending(upper, old, &next, pin, hidden, &clean)?;
        }
        if next.is_dir() {
            // Hard links give Discard a separate directory tree without another data copy.
            let result = std::process::Command::new("cp").args(["-al", "--"]).arg(&next).arg(temp.join("reference")).output()?;
            ensure!(result.status.success(), "prepare Discard view: {}", String::from_utf8_lossy(&result.stderr));
        }
        publish(&next, &lower)?;
        if temp.join("reference").is_dir() {
            publish(&temp.join("reference"), &reference)?;
        }
        for path in clean.into_iter().rev() {
            if path.symlink_metadata()?.is_dir() {
                if path != upper {
                    fs::remove_dir(path)?;
                }
            } else {
                fs::remove_file(path)?;
            }
        }
        for k in &kept {
            if let Ok(rel) = k.path.strip_prefix(pin) {
                let entry = if rel.as_os_str().is_empty() { upper.to_owned() } else { upper.join(rel) };
                if entry.symlink_metadata().is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
                    fs::write(kept_dir.join(&k.id).join("refreshed"), b"")?;
                }
            }
        }
        remove_entry(&dir.join("readonly"))
    })();
    remove_entry(&temp)?;
    if let Err(error) = result {
        // A refresh failure must never hide pending edits behind a read-only mount.
        if lower.try_exists()? {
            return Err(error).with_context(|| format!("refreshing {}", pin.display()));
        }
        fs::write(dir.join("readonly"), format!("{error:#}"))?;
    }
    Ok(())
}
/// Refresh the entry the shim will read for Discard. The original snapshot remains immutable.
pub fn refresh_reference(pin: &Path, state: &Path, host: &Path) -> Result<()> {
    let relative = host.strip_prefix(pin)?;
    let dir = directory(state, pin);
    let reference = dir.join("reference");
    let parent = relative.parent().context("cannot discard the pin root")?;
    let target_parent = crate::safe_fs::open_dir_beneath(&reference, parent, true)?;
    let target = reference.join(relative);
    let source_parent = crate::safe_fs::open_dir_beneath(pin, parent, false);
    if let Err(error) = &source_parent {
        let missing = error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT)
            || error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        if !missing {
            anyhow::bail!("checking original parent: {error:#}");
        }
    }
    // Both trees are outside the agent's writable view. Use the opened parent
    // descriptors so a host directory rename cannot redirect the copy.
    use std::os::fd::AsRawFd;
    let proc_dir = |fd: &std::os::fd::OwnedFd| PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    let name = target.file_name().context("missing file name")?;
    let target = proc_dir(&target_parent).join(name);
    let source = source_parent.ok().map(|fd| (proc_dir(&fd).join(name), fd));
    let metadata = source.as_ref().map(|(path, _)| path.symlink_metadata()).transpose();
    let metadata = match metadata {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(m) = &metadata
        && m.is_dir()
        && target.symlink_metadata().is_ok_and(|t| t.is_dir())
    {
        return Ok(());
    }
    let temp = dir.join(format!("discard-{:032x}", rand::random::<u128>()));
    if let (Some(m), Some((path, _))) = (metadata, source.as_ref()) {
        if m.is_file() {
            let mut input =
                std::fs::OpenOptions::new().read(true).custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32).open(path)?;
            ensure!(input.metadata()?.is_file(), "original changed type");
            let mut dest = fs::File::create_new(&temp)?;
            let copy = std::io::copy(&mut input, &mut dest).and_then(|_| dest.set_permissions(m.permissions()));
            if let Err(e) = copy {
                let _ = fs::remove_file(&temp);
                return Err(e.into());
            }
        } else if m.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(path)?, &temp)?;
        } else if m.is_dir() {
            fs::create_dir(&temp)?;
            fs::set_permissions(&temp, m.permissions())?;
        } else {
            anyhow::bail!("cannot discard against a special host file");
        }
    }
    if target.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(&target)?;
    }
    if temp.symlink_metadata().is_ok() {
        fs::rename(temp, target)?;
    } else if target.symlink_metadata().is_ok() {
        fs::remove_file(target)?;
    }
    Ok(())
}

/// Compare the host entry to the copy the session started from, including descendants
/// when Apply would replace or delete a directory.
pub fn unchanged(original: &Path, host: &Path, kept: &[crate::keep::Kept]) -> Result<bool> {
    if let Some(last) = kept.iter().rev().find(|k| k.path == host && k.undone_at.is_none()) {
        if let Some(hash) = &last.hash {
            return Ok(host.symlink_metadata().is_ok_and(|m| m.is_file()) && crate::keep::sha256_file(host)? == *hash);
        }
        if last.what == "removed" {
            return Ok(host.symlink_metadata().is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound));
        }
    }
    matches(original, host)
}

fn matches(original: &Path, host: &Path) -> Result<bool> {
    let metadata = |p: &Path| match p.symlink_metadata() {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    let (a, b) = (metadata(original)?, metadata(host)?);
    let (Some(a), Some(b)) = (&a, &b) else {
        return Ok(a.is_none() && b.is_none());
    };
    if a.file_type() != b.file_type() || a.permissions().mode() != b.permissions().mode() {
        return Ok(false);
    }
    if a.is_symlink() {
        return Ok(fs::read_link(original)? == fs::read_link(host)?);
    }
    if a.is_file() {
        return Ok(a.len() == b.len() && crate::keep::sha256_file(original)? == crate::keep::sha256_file(host)?);
    }
    if !a.is_dir() {
        return Ok(false);
    }
    let names = |p: &Path| fs::read_dir(p)?.map(|e| Ok(e?.file_name())).collect::<std::io::Result<std::collections::BTreeSet<_>>>();
    let entries = names(original)?;
    if entries != names(host)? {
        return Ok(false);
    }
    for name in entries {
        if !matches(&original.join(&name), &host.join(name))? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keep::{KeepOptions, keep};
    use crate::ledger::{Change, FileType, Kind};

    #[test]
    fn refresh_retries_failed_copies_and_skips_masks_before_reading() {
        let root = std::env::temp_dir().join(format!("silli-refresh-{:032x}", rand::random::<u128>()));
        let pin = root.join("host");
        let state = root.join("state");
        fs::create_dir_all(pin.join("secret")).unwrap();
        fs::write(pin.join("secret/token"), "private").unwrap();
        fs::write(pin.join("visible"), "first").unwrap();
        let socket = std::os::unix::net::UnixListener::bind(pin.join("uncopyable")).unwrap();
        let dir = directory(&state, &pin);
        let upper = dir.join("upper");
        let run = || prepare(&state, &pin, &[pin.join("secret")], &upper).unwrap();
        run();
        assert!(dir.join("readonly").is_file());
        drop(socket);
        fs::remove_file(pin.join("uncopyable")).unwrap();
        run();
        assert!(!dir.join("readonly").exists());
        fs::write(pin.join("visible"), "second").unwrap();
        fs::write(pin.join("new"), "created on host").unwrap();
        run();
        for view in ["lower", "reference"] {
            assert!(!dir.join(view).join("secret/token").exists());
            assert_eq!(fs::read(dir.join(view).join("visible")).unwrap(), b"second");
            assert_eq!(fs::read(dir.join(view).join("new")).unwrap(), b"created on host");
        }
        fs::create_dir_all(&upper).unwrap();
        fs::write(upper.join("visible"), "pending").unwrap();
        fs::remove_file(pin.join("visible")).unwrap();
        run();
        assert_eq!(fs::read(dir.join("reference/visible")).unwrap(), b"second");
        fs::remove_file(upper.join("visible")).unwrap(); // Discard while stopped
        run();
        assert!(!dir.join("lower/visible").exists());
        fs::remove_dir_all(root).unwrap();
    }

    // SILLI_TEST_NESTED_FILE names one readable file on a volume mounted below
    // SILLI_TEST_PIN. Only that file and its ancestors are included in the fixture.
    #[test]
    #[ignore = "requires a pin with a real nested filesystem"]
    fn snapshot_reads_across_a_nested_mount() {
        use std::os::unix::fs::MetadataExt;
        let pin = PathBuf::from(std::env::var_os("SILLI_TEST_PIN").unwrap());
        let file = PathBuf::from(std::env::var_os("SILLI_TEST_NESTED_FILE").unwrap());
        assert_ne!(pin.metadata().unwrap().dev(), file.metadata().unwrap().dev());
        let rel = file.strip_prefix(&pin).unwrap();
        let mut hidden = Vec::new();
        let mut parent = pin.clone();
        for component in rel {
            let child = parent.join(component);
            for e in fs::read_dir(&parent).unwrap() {
                let path = e.unwrap().path();
                if path != child {
                    hidden.push(path);
                }
            }
            parent = child;
        }
        let state = std::env::temp_dir().join(format!("silli-nested-{:032x}", rand::random::<u128>()));
        let dir = directory(&state, &pin);
        prepare(&state, &pin, &hidden, &dir.join("upper")).unwrap();
        assert_eq!(fs::read(dir.join("lower").join(rel)).unwrap(), fs::read(file).unwrap());
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn review_edits_deletions_new_files_conflicts_and_retries() {
        let root = std::env::temp_dir().join(format!("silli-reviewed-pin-{:032x}", rand::random::<u128>()));
        let pin = root.join("host");
        let state = root.join("session");
        fs::create_dir_all(&pin).unwrap();
        for file in ["edit", "delete", "conflict"] {
            fs::write(pin.join(file), "original").unwrap();
        }
        prepare(&state, &pin, &[], &directory(&state, &pin).join("upper")).unwrap();
        let layer = layer(&state, &pin);
        fs::create_dir_all(&layer.upper).unwrap();
        let apply = |name: &str, kind| {
            let path = pin.join(name);
            let upper = layer.upper.join(name);
            let original = directory(&state, &pin).join("lower").join(name);
            let change = Change { path, upper, kind, file_type: FileType::File, size: 0, conflict: false, ignored: false };
            keep(&change, &KeepOptions { staged_original: Some(original), undo: Some(state.join("kept")), ..Default::default() })
        };
        for file in ["edit", "added", "conflict", "discard"] {
            fs::write(layer.upper.join(file), "agent").unwrap();
        }
        fs::remove_file(layer.upper.join("discard")).unwrap();
        assert!(!pin.join("added").exists() && !pin.join("discard").exists());
        assert_eq!(fs::read(pin.join("edit")).unwrap(), b"original");
        fs::write(pin.join("conflict"), "person").unwrap();
        prepare(&state, &pin, &[], &directory(&state, &pin).join("upper")).unwrap(); // a restart preserves the original for comparison
        assert!(apply("conflict", Kind::Modified).is_err());
        assert_eq!(fs::read(pin.join("conflict")).unwrap(), b"person");
        refresh_reference(&pin, &state, &pin.join("conflict")).unwrap();
        assert_eq!(fs::read(directory(&state, &pin).join("reference/conflict")).unwrap(), b"person");
        assert_eq!(fs::read(directory(&state, &pin).join("lower/conflict")).unwrap(), b"original");
        apply("edit", Kind::Modified).unwrap();
        apply("added", Kind::Added).unwrap();
        apply("delete", Kind::Deleted).unwrap();
        assert!(!pin.join("delete").exists());
        assert_eq!(fs::read(pin.join("added")).unwrap(), b"agent");
        fs::write(layer.upper.join("edit"), "second").unwrap();
        apply("edit", Kind::Modified).unwrap();
        assert_eq!(fs::read(pin.join("edit")).unwrap(), b"second");
        fs::write(pin.join("edit"), "host after Apply").unwrap();
        prepare(&state, &pin, &[], &layer.upper).unwrap();
        assert!(!layer.upper.join("edit").exists());
        assert_eq!(fs::read(directory(&state, &pin).join("lower/edit")).unwrap(), b"host after Apply");
        fs::write(layer.upper.join("edit"), "third").unwrap();
        apply("edit", Kind::Modified).unwrap();
        assert_eq!(fs::read(pin.join("edit")).unwrap(), b"third");
        fs::remove_dir_all(root).unwrap();
    }
}
