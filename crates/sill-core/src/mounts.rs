//! The host mount table (`/proc/self/mountinfo`), reduced to what the jail builder needs: which
//! mount points carry real, persistent filesystems. Overlayfs never looks into a sub-mount of its lower
//! directory, so every such mount point below an overlaid directory needs its own overlay (SPEC §4.2).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub mount_point: PathBuf,
    pub fstype: String,
    pub source: String,
}

/// Filesystem types that hold persistent data worth overlaying. Pseudo filesystems, tmpfs, FUSE
/// mounts (GVfs, sshfs — not reliably overlayable, usually per-session anyway) and the overlay type
/// itself are left out.
const REAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "ntfs", "ntfs3", "jfs", "zfs",
    "bcachefs", "nfs", "nfs4", "cifs", "iso9660", "udf",
];

/// Persistent filesystems overlayfs refuses as a lower layer: their names compare
/// case-insensitively (dentry hash/compare operations), and the overlay mount fails with EINVAL —
/// every jail would fail to start (2026-11-06: a policy without `hidden_mounts` and a vfat `/boot`).
/// They are bound read-only instead: visible, never writable, never recorded.
const NO_OVERLAY_FS: &[&str] = &["vfat", "msdos", "exfat", "ntfs3", "iso9660"];

/// Can this mount be an overlay lower layer?
pub fn overlayable(m: &Mount) -> bool {
    !NO_OVERLAY_FS.contains(&m.fstype.as_str())
}

pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    parse_mountinfo_bytes(text.as_bytes())
}

/// Kernel mountinfo contains raw filename bytes as well as octal escapes.
fn parse_mountinfo_bytes(text: &[u8]) -> Vec<Mount> {
    use std::os::unix::ffi::OsStringExt;
    let mut out = Vec::new();
    for line in text.split(|b| *b == b'\n') {
        let Some(separator) = line.windows(3).position(|s| s == b" - ") else { continue };
        let pre: Vec<_> = line[..separator].split(|b| *b == b' ').collect();
        let post: Vec<_> = line[separator + 3..].split(|b| *b == b' ').collect();
        if pre.len() < 5 || post.len() < 2 { continue; }
        out.push(Mount {
            mount_point: std::ffi::OsString::from_vec(unescape_bytes(pre[4])).into(),
            fstype: String::from_utf8_lossy(post[0]).into_owned(),
            source: String::from_utf8_lossy(&unescape_bytes(post[1])).into_owned(),
        });
    }
    out
}

/// mountinfo escapes space, tab, newline and backslash as octal (`\\040`).
fn unescape_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i+1..i+4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            let value = (bytes[i+1] - b'0') as u16 * 64 + (bytes[i+2] - b'0') as u16 * 8 + (bytes[i+3] - b'0') as u16;
            if value <= 255 { out.push(value as u8); i += 4; continue; }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

pub fn host_mounts() -> Result<Vec<Mount>> {
    let bytes = std::fs::read("/proc/self/mountinfo").context("read /proc/self/mountinfo")?;
    Ok(parse_mountinfo_bytes(&bytes))
}

/// Mounts with a persistent filesystem, excluding `/` itself, sorted by path so parents precede children.
/// When one path is mounted several times the last entry wins (that is what is visible).
pub fn real_mounts(all: &[Mount]) -> Vec<Mount> {
    let mut by_path: std::collections::BTreeMap<PathBuf, Mount> = Default::default();
    for m in all {
        if m.mount_point == Path::new("/") {
            continue;
        }
        if REAL_FS.contains(&m.fstype.as_str()) {
            by_path.insert(m.mount_point.clone(), m.clone());
        } else {
            // a pseudo/tmpfs mounted over a real mount hides it
            by_path.remove(&m.mount_point);
        }
    }
    by_path.into_values().collect()
}

/// Is `path` itself a mount point (any filesystem type)?
pub fn is_mount_point(all: &[Mount], path: &Path) -> bool {
    all.iter().any(|m| m.mount_point == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_and_octal_mount_paths_preserve_filename_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let mounts = parse_mountinfo_bytes(b"1 0 8:1 / /raw/\xff rw - ext4 /dev/a rw\n2 0 8:2 / /octal/\\377 rw - ext4 /dev/b rw\n");
        assert_eq!(mounts[0].mount_point.as_os_str().as_bytes(), b"/raw/\xff");
        assert_eq!(mounts[1].mount_point.as_os_str().as_bytes(), b"/octal/\xff");
    }

    #[test]
    fn parses_and_filters() {
        let text = "\
25 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
30 25 0:5 / /dev rw,nosuid shared:2 - devtmpfs udev rw
40 25 8:1 / /boot rw,relatime shared:3 - ext4 /dev/sda1 rw
41 40 8:3 / /boot/efi rw - vfat /dev/sda3 rw
50 25 0:30 / /tmp rw - tmpfs tmpfs rw
60 25 8:16 / /mnt/with\\040space rw - ext4 /dev/sdb1 rw
";
        let all = parse_mountinfo(text);
        assert_eq!(all.len(), 6);
        assert_eq!(all[5].mount_point, PathBuf::from("/mnt/with space"));
        let real = real_mounts(&all);
        let points: Vec<_> = real.iter().map(|m| m.mount_point.to_string_lossy().into_owned()).collect();
        assert_eq!(points, vec!["/boot", "/boot/efi", "/mnt/with space"]);
        assert!(is_mount_point(&all, Path::new("/tmp")));
    }
}
