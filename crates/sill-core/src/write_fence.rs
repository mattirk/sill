//! A Landlock fence for a host-side command that works in a folder the agent writes: the
//! command may create, change, rename or remove files only beneath the folders named, wherever a
//! path it writes through leads. The kernel checks the resolved place of every write, so a link
//! the agent plants at any moment, before the command or while it runs, cannot take a write
//! elsewhere (audit 2026-11-18 M2: git writes refs, ref locks and objects by path). Reads are not
//! fenced. Without Landlock in the kernel there is no fence and the caller's own checks are all.

use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;

const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: libc::c_int = 1;

const WRITE_FILE: u64 = 1 << 1;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
/// ABI 2: linking or renaming into another directory.
const REFER: u64 = 1 << 13;
/// ABI 3: truncating.
const TRUNCATE: u64 = 1 << 14;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The write rights the running kernel knows, every one of them handled (denied unless allowed).
fn write_rights(abi: i64) -> u64 {
    let mut rights = WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_CHAR | MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | MAKE_SYM;
    if abi >= 2 {
        rights |= REFER;
    }
    if abi >= 3 {
        rights |= TRUNCATE;
    }
    rights
}

/// The kernel's Landlock ABI version; None without Landlock (not built in, or not enabled).
fn abi() -> Option<i64> {
    // SAFETY: the version query takes no attribute and returns a number or an error.
    let v = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
    (v >= 1).then_some(v)
}

/// A ruleset allowing every write beneath each directory of `beneath` (and writing to
/// `/dev/null`), and no other write. None when the kernel has no Landlock.
pub fn ruleset(beneath: &[&Path]) -> Result<Option<Arc<OwnedFd>>> {
    let Some(abi) = abi() else {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| tracing::warn!("no Landlock in this kernel: host-side git in workspace clones runs without a write fence"));
        return Ok(None);
    };
    let rights = write_rights(abi);
    let attr = RulesetAttr { handled_access_fs: rights };
    // SAFETY: a valid attribute of the size given; the result is a new descriptor or an error.
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, std::mem::size_of::<RulesetAttr>(), 0u32) };
    anyhow::ensure!(fd >= 0, "landlock_create_ruleset: {}", std::io::Error::last_os_error());
    // SAFETY: the kernel just returned this descriptor, owned by nobody else.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let add = |path: &Path, allowed: u64, dir: bool| -> Result<()> {
        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW | if dir { OFlags::DIRECTORY } else { OFlags::empty() };
        let parent = rustix::fs::open(path, flags, Mode::empty()).with_context(|| format!("open {} for the write fence", path.display()))?;
        let rule = PathBeneathAttr { allowed_access: allowed, parent_fd: parent.as_raw_fd() };
        // SAFETY: a valid rule for a ruleset descriptor and a path descriptor both held open.
        let r = unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset.as_raw_fd(), RULE_PATH_BENEATH, &rule as *const PathBeneathAttr, 0u32) };
        anyhow::ensure!(r == 0, "landlock_add_rule {}: {}", path.display(), std::io::Error::last_os_error());
        Ok(())
    };
    for dir in beneath {
        add(dir, rights, true)?;
    }
    // a file takes only rights on files; `/dev/null` is a character device, never a link
    add(Path::new("/dev/null"), rights & (WRITE_FILE | TRUNCATE), false)?;
    Ok(Some(Arc::new(ruleset)))
}

/// Run `cmd` inside `ruleset`: its process restricts itself between fork and exec, and every
/// process it starts inherits the fence.
pub fn apply(cmd: &mut std::process::Command, ruleset: &Arc<OwnedFd>) {
    let ruleset = Arc::clone(ruleset);
    // SAFETY: the closure makes only raw system calls (async-signal-safe) on a descriptor the Arc
    // keeps open until the command is dropped.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(cmd, move || {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fenced process writes beneath its folder, and not through a link out of it.
    #[test]
    fn a_fenced_write_through_a_link_is_refused() {
        let base = std::env::temp_dir().join(format!("sill-write-fence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let inside = base.join("inside");
        let outside = base.join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, inside.join("out")).unwrap();
        let Some(fence) = ruleset(&[&inside]).unwrap() else { return };
        let sh = |script: &str| {
            let mut c = std::process::Command::new("/bin/sh");
            c.arg("-c").arg(script).current_dir(&inside);
            apply(&mut c, &fence);
            c.status().unwrap().success()
        };
        assert!(sh("echo a > a && mkdir d && mv a d/a && rm -r d && echo x > /dev/null"), "writes beneath the folder");
        assert!(!sh("echo b > out/b"), "a write through the link is refused");
        assert!(!outside.join("b").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
