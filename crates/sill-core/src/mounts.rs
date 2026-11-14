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

pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    let mut out = Vec::new();
    for line in text.lines() {
        // mountinfo: id parent major:minor root mount_point opts [optional...] - fstype source superopts
        let Some((pre, post)) = line.split_once(" - ") else { continue };
        let pre: Vec<&str> = pre.split(' ').collect();
        let post: Vec<&str> = post.split(' ').collect();
        if pre.len() < 5 || post.len() < 2 {
            continue;
        }
        out.push(Mount {
            mount_point: PathBuf::from(unescape(pre[4])),
            fstype: post[0].to_string(),
            source: unescape(post[1]),
        });
    }
    out
}

/// mountinfo escapes space, tab, newline and backslash as octal (`\040`).
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn host_mounts() -> Result<Vec<Mount>> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").context("read /proc/self/mountinfo")?;
    Ok(parse_mountinfo(&text))
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
