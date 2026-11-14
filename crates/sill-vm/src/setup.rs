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
    private_disks(dir)?;
    Ok(Prepared { first, updated })
}

/// The image in `dir` as `system.raw`, unless the one there is the same build (by `commit`):
/// cloned from `image_dir/system.raw` (a build of it, instant on APFS), or unpacked from
/// `image_dir/system.raw.gz` (the app's: a sparse 16 GB file would not survive being copied out
/// of a disk image), writing only what is not zeros.
fn place_image(dir: &Path, image_dir: &Path) -> Result<bool> {
    let raw = image_dir.join("system.raw");
    let packed = image_dir.join("system.raw.gz");
    if !raw.exists() && !packed.exists() {
        bail!("no system image in {}", image_dir.display());
    }
    let commit = std::fs::read_to_string(image_dir.join("commit")).unwrap_or_default();
    let placed = dir.join("system.raw");
    let placed_commit = dir.join("system.commit");
    if placed.exists() && std::fs::read_to_string(&placed_commit).unwrap_or_default() == commit {
        return Ok(false);
    }
    let temp = dir.join("system.raw.new");
    let _ = std::fs::remove_file(&temp);
    if raw.exists() {
        // std's copy clones the file on APFS (fclonefileat)
        std::fs::copy(&raw, &temp).with_context(|| format!("copying {}", raw.display()))?;
    } else {
        unpack_sparse(&packed, &temp).with_context(|| format!("unpacking {}", packed.display()))?;
    }
    std::fs::rename(&temp, &placed)?;
    std::fs::write(&placed_commit, commit)?;
    Ok(true)
}

/// Gunzip `from` into `to`, leaving zeros as holes: the image's free space takes none. The file
/// is sized first (`from`'s `.size` beside it, when there) and only the 4 KB blocks with data are
/// written into it: APFS fills a gap of up to 16 MB that a growing file skips, and writing
/// whole megabytes would store the zeros between data (measured 2026-11-12: 1.8 GB on disk for
/// the 1.7 GB image this way, 2.4 GB by seeking past megabytes of zeros).
fn unpack_sparse(from: &Path, to: &Path) -> Result<()> {
    use std::io::Read;
    use std::os::unix::fs::FileExt;
    const CHUNK: usize = 1 << 20;
    const BLOCK: usize = 4096;
    let mut input = flate2::read::GzDecoder::new(std::io::BufReader::new(std::fs::File::open(from)?));
    let output = std::fs::File::create(to)?;
    let size_file = from.with_extension("gz.size");
    if let Some(size) = std::fs::read_to_string(&size_file).ok().and_then(|s| s.trim().parse::<u64>().ok()) {
        output.set_len(size)?;
    }
    let mut buf = vec![0u8; CHUNK];
    let mut offset: u64 = 0;
    loop {
        // a whole chunk, unless the stream ends
        let mut filled = 0;
        while filled < CHUNK {
            match input.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            break;
        }
        for (i, block) in buf[..filled].chunks(BLOCK).enumerate() {
            if block.iter().any(|b| *b != 0) {
                output.write_all_at(block, offset + (i * BLOCK) as u64)?;
            }
        }
        offset += filled as u64;
    }
    output.set_len(offset)?;
    output.sync_all()?;
    Ok(())
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

/// The disks hold everything in the VM: the owner's alone, also those made before this was
/// set (they were 0644, kept from other accounts only by the directory's own mode).
fn private_disks(dir: &Path) -> Result<()> {
    for disk in ["system.raw", "home.raw", "state.raw"] {
        let path = dir.join(disk);
        if path.exists() {
            set_mode(&path, 0o600)?;
        }
    }
    Ok(())
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
    fn a_packed_image_unpacks_whole_with_its_zeros_as_holes() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("sill-vm-unpack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // data, 64 MB of zeros, data (APFS fills gaps of 16 MB or less, keeps 32 MB and more
        // as holes: measured 2026-11-12)
        let mut image = b"start".to_vec();
        image.resize(64 << 20, 0);
        image.extend_from_slice(b"end");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&image).unwrap();
        std::fs::write(dir.join("system.raw.gz"), gz.finish().unwrap()).unwrap();
        std::fs::write(dir.join("system.raw.gz.size"), image.len().to_string()).unwrap();
        super::unpack_sparse(&dir.join("system.raw.gz"), &dir.join("out.raw")).unwrap();
        assert_eq!(std::fs::read(dir.join("out.raw")).unwrap(), image);
        use std::os::unix::fs::MetadataExt;
        let taken = std::fs::metadata(dir.join("out.raw")).unwrap().blocks() * 512;
        assert!(taken < 8 << 20, "{taken} bytes on disk for a 64 MB file mostly zeros");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_disks_become_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sill-vm-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // one made before, readable by all; one not there yet
        std::fs::write(dir.join("home.raw"), b"disk").unwrap();
        std::fs::set_permissions(dir.join("home.raw"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(dir.join("system.raw"), b"disk").unwrap();
        super::private_disks(&dir).unwrap();
        for disk in ["home.raw", "system.raw"] {
            let mode = std::fs::metadata(dir.join(disk)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{disk}");
        }
        assert!(!dir.join("state.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_ssh_config_quotes_paths_with_spaces() {
        let config = super::ssh_config(std::path::Path::new("/Users/a/Library/Application Support/Sill/vm"));
        assert!(config.contains("IdentityFile \"/Users/a/Library/Application Support/Sill/vm/key\""));
        assert!(config.contains("ProxyCommand /usr/bin/nc -U \"/Users/a/Library/Application Support/Sill/vm/ssh.sock\""));
        assert!(config.starts_with("# Made by sill-gui"));
        assert!(config.contains("\nHost sill-vm\n"));
    }
}
