//! A VM directory made ready to start: the image put in place, the data disks, the key the app
//! logs in with, the cloud-init seed that gives the guest that key, and the SSH config the
//! window's connection uses. Each piece is made once and kept; a newer image replaces the
//! system disk only, never the data disks.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The data disks' size: sparse files, so only what the guest writes takes space.
const DATA_DISK: u64 = 64 << 30;
/// The account the image's sill runs as, and the name SSH knows the VM by.
pub const USER: &str = "sill";
pub const HOST: &str = "sill-vm";
/// Where the image's sill is.
pub const SILL: &str = "/usr/local/bin/sill";

/// What `prepare` did, for what the window says meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prepared {
    /// Nothing was here: a first start, with the data disks new.
    pub first: bool,
    /// A newer image was put in place of the system disk.
    pub updated: bool,
}

/// Make `dir` ready from the image in `image_dir` (`system.raw` and its `commit`).
pub fn prepare(dir: &Path, image_dir: &Path) -> Result<Prepared> {
    std::fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)?;
    let first = !dir.join("system.raw").exists();
    let updated = place_image(dir, image_dir)? && !first;
    for disk in ["home.raw", "state.raw"] {
        let path = dir.join(disk);
        if !path.exists() {
            std::fs::File::create(&path)?.set_len(DATA_DISK)?;
        }
    }
    let key = dir.join("key");
    if !key.exists() {
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "sill-app", "-f"])
            .arg(&key)
            .status()
            .context("ssh-keygen")?;
        if !made.success() {
            bail!("ssh-keygen could not make the VM's key");
        }
    }
    if !dir.join("seed.iso").exists() {
        seed(dir)?;
    }
    std::fs::write(dir.join("ssh_config"), ssh_config(dir))?;
    Ok(Prepared { first, updated })
}

/// The image's `system.raw` in `dir`, unless the one there is the same build (by `commit`).
/// A clone on APFS: instant, and no space taken until the guest writes.
fn place_image(dir: &Path, image_dir: &Path) -> Result<bool> {
    let source = image_dir.join("system.raw");
    if !source.exists() {
        bail!("no system image at {}", source.display());
    }
    let commit = std::fs::read_to_string(image_dir.join("commit")).unwrap_or_default();
    let placed = dir.join("system.raw");
    let placed_commit = dir.join("system.commit");
    if placed.exists() && std::fs::read_to_string(&placed_commit).unwrap_or_default() == commit {
        return Ok(false);
    }
    let temp = dir.join("system.raw.new");
    let _ = std::fs::remove_file(&temp);
    // std's copy clones the file on APFS (fclonefileat)
    std::fs::copy(&source, &temp).with_context(|| format!("copying {}", source.display()))?;
    std::fs::rename(&temp, &placed)?;
    std::fs::write(&placed_commit, commit)?;
    Ok(true)
}

/// The NoCloud seed: the account's key, and an instance id of this VM's own, so the image's
/// first boot here sets the account up and later images keep it.
fn seed(dir: &Path) -> Result<()> {
    let key = std::fs::read_to_string(dir.join("key.pub"))?;
    let work = dir.join("seed");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    std::fs::write(
        work.join("user-data"),
        format!(
            "#cloud-config\nusers:\n  - name: {USER}\n    shell: /bin/bash\n    sudo: ALL=(ALL) NOPASSWD:ALL\n    ssh_authorized_keys:\n      - {}\n",
            key.trim()
        ),
    )?;
    let instance = format!(
        "sill-app-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    std::fs::write(
        work.join("meta-data"),
        format!("instance-id: {instance}\nlocal-hostname: sill-box\n"),
    )?;
    let iso = dir.join("seed.iso");
    let made = Command::new("hdiutil")
        .args(["makehybrid", "-quiet", "-iso", "-joliet", "-default-volume-name", "cidata", "-o"])
        .arg(&iso)
        .arg(&work)
        .status()
        .context("hdiutil")?;
    std::fs::remove_dir_all(&work)?;
    if !made.success() {
        let _ = std::fs::remove_file(&iso);
        bail!("hdiutil could not make the VM's seed");
    }
    Ok(())
}

/// The window's SSH to its VM: over the VM's local socket (`serve_ssh`), with the VM's key. The
/// guest's host key is not checked: the socket leads into a VM in this very process, and every
/// new image makes new host keys.
fn ssh_config(dir: &Path) -> String {
    let path = |name: &str| quoted(&dir.join(name));
    format!(
        "# Made by sill-gui for its own VM; rewritten at each start.\n\
         Host {HOST}\n\
         \x20 User {USER}\n\
         \x20 IdentityFile {}\n\
         \x20 IdentitiesOnly yes\n\
         \x20 ProxyCommand /usr/bin/nc -U {}\n\
         \x20 StrictHostKeyChecking no\n\
         \x20 UserKnownHostsFile /dev/null\n\
         \x20 LogLevel ERROR\n",
        path("key"),
        path("ssh.sock"),
    )
}

/// A path in double quotes, for SSH's config and the shell it runs ProxyCommand with.
fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('"', "\\\""))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// The VM directory on this Mac.
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/Sill/vm"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_ssh_config_quotes_paths_with_spaces() {
        let config = super::ssh_config(std::path::Path::new("/Users/a/Library/Application Support/Sill/vm"));
        assert!(config.contains("IdentityFile \"/Users/a/Library/Application Support/Sill/vm/key\""));
        assert!(config.contains("ProxyCommand /usr/bin/nc -U \"/Users/a/Library/Application Support/Sill/vm/ssh.sock\""));
        assert!(config.starts_with("# Made by sill-gui"));
        assert!(config.contains("\nHost sill-vm\n"));
    }
}
