//! Can this Mac, or this PC, run silli's VM? Said in plain words, for someone who has never heard
//! of one.
use std::path::Path;

/// One thing in the way, with what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub what: String,
    pub fix: String,
}

/// The least the VM needs: memory and free disk space in the directory its disks go in.
const MEMORY_GB: u64 = 8;
const DISK_GB: u64 = 10;
/// macOS 13: the framework's EFI boot and Linux guests as silli uses them.
#[cfg(not(windows))]
const MACOS_MAJOR: u32 = 13;

/// Everything in the way of running the VM with its disks in `dir` (empty: all is well).
pub fn check(dir: &Path) -> Vec<Problem> {
    let mut problems = Vec::new();
    #[cfg(windows)]
    {
        if !cfg!(target_arch = "x86_64") {
            problems.push(Problem { what: "Silli needs a PC with a 64-bit Intel or AMD processor.".into(), fix: "Use Silli on such a PC.".into() });
        }
        if let Some((what, fix)) = crate::hcs::unavailable() {
            problems.push(Problem { what, fix });
        }
        if let Some(bytes) = memory_bytes()
            && bytes < MEMORY_GB << 30
        {
            problems.push(Problem {
                what: format!("Silli needs a PC with at least {MEMORY_GB} GB of memory; this one has {} GB.", bytes >> 30),
                fix: "Use Silli on a PC with more memory.".into(),
            });
        }
        if let Some(free) = free_bytes(dir)
            && free < DISK_GB << 30
        {
            problems.push(Problem {
                what: format!("Silli needs {DISK_GB} GB of free disk space; {} GB is free.", free >> 30),
                fix: "Free some space, for example by emptying the Recycle Bin.".into(),
            });
        }
        problems
    }
    #[cfg(not(windows))]
    {
        mac_check(dir, &mut problems);
        problems
    }
}

#[cfg(not(windows))]
fn mac_check(dir: &Path, problems: &mut Vec<Problem>) {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        problems.push(Problem { what: "Silli needs a Mac with Apple silicon (M1 or later).".into(), fix: "Use Silli on such a Mac, or on Linux.".into() });
        return;
    }
    if let Some(major) = macos_major()
        && major < MACOS_MAJOR
    {
        problems.push(Problem {
            what: format!("Silli needs macOS {MACOS_MAJOR} or later; this Mac has macOS {major}."),
            fix: "Update macOS in System Settings → General → Software Update.".into(),
        });
    }
    #[cfg(target_os = "macos")]
    if !crate::apple::supported() {
        problems.push(Problem {
            what: "This Mac does not let apps run virtual machines.".into(),
            fix: "Silli cannot run here; it may be a Mac that is itself virtual.".into(),
        });
    }
    if let Some(bytes) = memory_bytes()
        && bytes < MEMORY_GB << 30
    {
        problems.push(Problem {
            what: format!("Silli needs a Mac with at least {MEMORY_GB} GB of memory; this one has {} GB.", bytes >> 30),
            fix: "Use Silli on a Mac with more memory.".into(),
        });
    }
    if let Some(free) = free_bytes(dir)
        && free < DISK_GB << 30
    {
        problems.push(Problem {
            what: format!("Silli needs {DISK_GB} GB of free disk space; {} GB is free.", free >> 30),
            fix: "Free some space, for example by emptying the Trash.".into(),
        });
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn memory_bytes() -> Option<u64> {
    sysctl_u64(c"hw.memsize")
}

#[cfg(target_os = "macos")]
fn macos_major() -> Option<u32> {
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    // SAFETY: a valid name, and a buffer with its length
    let ok = unsafe { libc::sysctlbyname(c"kern.osproductversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
    if !ok {
        return None;
    }
    let text = std::str::from_utf8(&buf[..len]).ok()?.trim_end_matches('\0');
    text.split('.').next()?.parse().ok()
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut value = 0u64;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: a valid name, and room for a u64
    let ok = unsafe { libc::sysctlbyname(name.as_ptr(), (&mut value as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
    ok.then_some(value)
}

// These queries describe the Mac hosting the VM; Linux has no value.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn memory_bytes() -> Option<u64> {
    None
}

#[cfg(windows)]
#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(status: *mut [u64; 8]) -> i32;
    fn GetDiskFreeSpaceExW(path: *const u16, free: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
}

#[cfg(windows)]
pub(crate) fn memory_bytes() -> Option<u64> {
    // MEMORYSTATUSEX: its length and load in the first eight bytes, then the physical total
    let mut status = [0u64; 8];
    status[0] = 64;
    // SAFETY: a structure of the size it says
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status[1])
}

/// Free space where `dir` is, or where it will be made (its nearest existing ancestor).
#[cfg(windows)]
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let existing = dir.ancestors().find(|p| p.exists())?;
    let path: Vec<u16> = existing.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated path and a number to fill
    (unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) } != 0).then_some(free)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn macos_major() -> Option<u32> {
    None
}

/// Free space where `dir` is, or where it will be made (its nearest existing ancestor).
#[cfg(unix)]
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let existing = dir.ancestors().find(|p| p.exists())?;
    let path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid path and a statvfs to fill
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn this_pc_is_described() {
        // its memory is known, and nothing names Apple
        assert!(super::memory_bytes().is_some_and(|bytes| bytes > 1 << 30));
        assert!(super::check(&std::env::temp_dir()).iter().all(|p| !p.what.contains("Apple")));
    }

    #[cfg(not(windows))]
    #[test]
    fn this_mac_is_described() {
        // the developer's Mac passes; elsewhere the first problem names Apple silicon
        let problems = super::check(&std::env::temp_dir());
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert!(problems.iter().all(|p| !p.what.contains("Apple silicon")));
            assert!(super::memory_bytes().is_some());
            assert!(super::macos_major().is_some_and(|m| m >= 13));
        } else {
            assert!(problems[0].what.contains("Apple silicon"));
        }
    }
}
