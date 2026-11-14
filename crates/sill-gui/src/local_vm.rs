//! The Mac window's own Linux VM, the box its daemon runs in (docs/sill-stick.md section 9).
//!
//! Without `SILL_BOX`, sill-gui on the Mac starts the VM in this process at launch — on a thread
//! of its own, the window showing how far it is (`phase`) — and talks to its daemon as to any box,
//! over SSH through the VM's local socket. The VM ends with the process; Quit asks the guest to
//! shut down first (`shut_down`).
//!
//! A folder of the Mac the person pins is shared with the VM and appears there at its Mac path
//! (`expose`), so the VM, its jails and the person see one path; the folders are kept in
//! `folders.json` and shared again at each start.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use sill_client::Connector;
use sill_vm::setup;

/// How far the VM is, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Checking,
    /// Making the data disks and the key: the first start on this Mac.
    SettingUp,
    /// A newer system image was put in place.
    Updating,
    Starting,
    Ready,
    /// In plain words, for the person; what to do about it included.
    Failed(String),
}

struct Shared {
    phase: Mutex<Phase>,
    changed: Condvar,
    vm: Mutex<Option<Arc<sill_vm::Vm>>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
/// The VM's directory on this Mac.
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// How long a connection waits for the VM before it fails as any unreachable box would.
const READY_WAIT: Duration = Duration::from_secs(300);

/// The window uses its own VM: on the Mac, unless `SILL_BOX` names another box or it is the demo.
pub fn wanted(demo: bool) -> bool {
    !demo && std::env::var_os("SILL_BOX").is_none()
}

/// Choose the VM as this process's box and start it on a thread of its own.
pub fn begin() {
    let shared = SHARED.get_or_init(|| Shared {
        phase: Mutex::new(Phase::Checking),
        changed: Condvar::new(),
        vm: Mutex::new(None),
    });
    let Some(dir) = setup::default_dir() else {
        set(Phase::Failed("Sill cannot find your home folder.".into()));
        return;
    };
    let _ = DIR.set(dir.clone());
    sill_proto::transport::choose(
        "This Mac",
        sill_proto::transport::BoxTarget {
            host: setup::HOST.into(),
            binary: setup::SILL.into(),
            ssh_config: Some(dir.join("ssh_config")),
        },
    );
    let spawned = std::thread::Builder::new()
        .name("sill-gui-vm".into())
        .spawn(move || {
            if let Err(said) = run(&dir) {
                set(Phase::Failed(said));
            }
        });
    if spawned.is_err() {
        set(Phase::Failed("Sill could not start its VM.".into()));
    }
    let _ = shared;
}

fn run(dir: &std::path::Path) -> Result<(), String> {
    let problems = sill_vm::check(dir);
    if !problems.is_empty() {
        return Err(problems
            .iter()
            .map(|p| format!("{} {}", p.what, p.fix))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let image = image_dir().ok_or("Sill's system image is missing from the app.")?;
    if !dir.join("system.raw").exists() {
        set(Phase::SettingUp);
    }
    let prepared = setup::prepare(dir, &image).map_err(|e| format!("Sill could not set up: {e:#}"))?;
    if prepared.updated {
        set(Phase::Updating);
    }
    set(Phase::Starting);
    let config = sill_vm::Config::sized_for_this_mac(dir.to_path_buf());
    let vm = sill_vm::Vm::start(&config).map_err(|e| {
        let said = format!("{e:#}");
        if said.contains("entitlement") {
            "This copy of Sill is not allowed to run virtual machines (it is not signed for it)."
                .to_string()
        } else {
            format!("Sill's VM did not start: {said}")
        }
    })?;
    let vm = Arc::new(vm);
    sill_vm::serve_ssh(vm.clone(), &dir.join("ssh.sock"))
        .map_err(|e| format!("Sill could not reach its VM: {e:#}"))?;
    if let Some(shared) = SHARED.get() {
        *shared.vm.lock().unwrap() = Some(vm.clone());
    }
    vm.wait_for_ssh(Duration::from_secs(180))
        .map_err(|e| format!("Sill's VM started but does not answer: {e:#}"))?;
    // the Mac folders pinned before, back at their paths before any session starts
    if let Err(e) = sync_folders(dir, &vm) {
        eprintln!("sill-gui: Mac folders: {e}");
    }
    set(Phase::Ready);
    Ok(())
}

fn set(phase: Phase) {
    if let Some(shared) = SHARED.get() {
        *shared.phase.lock().unwrap() = phase;
        shared.changed.notify_all();
    }
}

/// How far the VM is; `None` when the window does not use its own VM.
pub fn phase() -> Option<Phase> {
    SHARED.get().map(|shared| shared.phase.lock().unwrap().clone())
}

/// What the window says while it waits for its VM, or `None` once there is nothing to say.
pub fn waiting_text() -> Option<(String, bool)> {
    Some(match phase()? {
        Phase::Checking | Phase::Starting => ("Starting Sill…".into(), false),
        Phase::SettingUp => ("Setting up Sill for the first time…".into(), false),
        Phase::Updating => ("Updating Sill…".into(), false),
        Phase::Ready => return None,
        Phase::Failed(said) => (said, true),
    })
}

/// Connect to the VM's daemon once the VM answers; until then the caller waits here.
pub fn connector() -> Connector {
    Arc::new(|| {
        let shared = SHARED.get().ok_or_else(|| anyhow::anyhow!("no VM"))?;
        let phase = shared.phase.lock().unwrap();
        let (phase, _) = shared
            .changed
            .wait_timeout_while(phase, READY_WAIT, |phase| {
                !matches!(phase, Phase::Ready | Phase::Failed(_))
            })
            .unwrap();
        match &*phase {
            Phase::Ready => {}
            Phase::Failed(said) => anyhow::bail!("{said}"),
            _ => anyhow::bail!("Sill's VM is still starting"),
        }
        drop(phase);
        sill_proto::Client::connect()
    })
}

/// Shut the VM down: the guest is asked first and given a little while, then stopped.
pub fn shut_down() {
    let Some(shared) = SHARED.get() else { return };
    let vm = shared.vm.lock().unwrap().take();
    if let Some(vm) = vm {
        let _ = sill_proto::boxlink::disconnect(setup::HOST);
        let _ = vm.shut_down(Duration::from_secs(15));
    }
}

/// Where the system image is: `SILL_VM_IMAGE`, the app bundle's resources, or a build of it in
/// the repository the program was built in (`packaging/mac-image.sh`, `dist/mac-image`).
fn image_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SILL_VM_IMAGE") {
        return Some(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().ok()?;
    let has_image = |dir: &std::path::Path| dir.join("system.raw").exists();
    if let Some(resources) = exe.parent().and_then(|macos| macos.parent()).map(|c| c.join("Resources/vm"))
        && has_image(&resources)
    {
        return Some(resources);
    }
    exe.ancestors()
        .map(|dir| dir.join("dist/mac-image"))
        .find(|dir| has_image(dir))
}

/// Make the Mac folder `path` appear at the same path in the VM, for pinning it there; it stays
/// shared from then on. Runs SSH: call it off the window's thread.
pub fn expose(path: &Path) -> Result<PathBuf, String> {
    let (Some(dir), Some(shared)) = (DIR.get(), SHARED.get()) else {
        return Err("Sill's VM is not in use.".into());
    };
    let vm = shared.vm.lock().unwrap().clone().ok_or("Sill's VM is not running yet.")?;
    let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !mac_folder(&path) {
        return Err(format!(
            "{} cannot be pinned: only folders in a user's folder (/Users) or on a disk (/Volumes) can.",
            path.display()
        ));
    }
    let mut folders = load_folders(dir);
    // inside a folder already shared, it is in the VM already
    if !folders.iter().any(|f| path.starts_with(f)) {
        if folders.len() >= sill_vm::SHARE_SLOTS {
            return Err(format!(
                "Sill can share up to {} folders of this Mac with its VM.",
                sill_vm::SHARE_SLOTS
            ));
        }
        folders.push(path.clone());
        save_folders(dir, &folders).map_err(|e| format!("Sill could not save its folders: {e}"))?;
        // the new folder's own slot: the others stay as they are
        let slot = folders.len() - 1;
        vm.share_slot(slot, Some(&path))
            .map_err(|e| format!("Sill could not share {}: {e:#}", path.display()))?;
    }
    mount_folders(dir, &folders)?;
    Ok(path)
}

/// A folder of a Mac user's or on a Mac volume: nothing that could cover the guest's own.
fn mac_folder(path: &Path) -> bool {
    let mut parts = path.components().skip(1);
    matches!(parts.next(), Some(std::path::Component::Normal(top)) if top == "Users" || top == "Volumes")
        && parts.next().is_some()
}

fn load_folders(dir: &Path) -> Vec<PathBuf> {
    std::fs::read(dir.join("folders.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_folders(dir: &Path, folders: &[PathBuf]) -> std::io::Result<()> {
    let temp = dir.join("folders.json.new");
    std::fs::write(&temp, serde_json::to_vec_pretty(folders)?)?;
    std::fs::rename(temp, dir.join("folders.json"))
}

/// At the VM's start: every kept folder in its slot (by its place in the list), then mounted.
fn sync_folders(dir: &Path, vm: &sill_vm::Vm) -> Result<(), String> {
    let folders = load_folders(dir);
    for (slot, path) in folders.iter().enumerate().take(sill_vm::SHARE_SLOTS) {
        let present = path.is_dir().then_some(path.as_path());
        vm.share_slot(slot, present)
            .map_err(|e| format!("Sill could not share {}: {e:#}", path.display()))?;
    }
    mount_folders(dir, &folders)
}

/// The guest's root helper, sent with each use (`packaging/mac/guest/sill-mac-folders`).
const MOUNTER: &str = include_str!("../../../packaging/mac/guest/sill-mac-folders");

/// Mount each shared folder at its Mac path inside the VM, as root over the VM's SSH.
fn mount_folders(dir: &Path, folders: &[PathBuf]) -> Result<(), String> {
    let lines: String = folders
        .iter()
        .enumerate()
        .take(sill_vm::SHARE_SLOTS)
        .filter(|(_, path)| path.is_dir())
        .map(|(slot, path)| format!("{slot}\t{}\n", path.display()))
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    // the script travels in the command (base64, no quoting to get wrong), the folders on stdin
    let remote = format!(
        "f=$(mktemp) && echo {} | base64 -d > \"$f\" && sudo sh \"$f\"; s=$?; rm -f \"$f\"; exit $s",
        base64(MOUNTER.as_bytes())
    );
    let mut child = std::process::Command::new("ssh")
        .arg("-F")
        .arg(dir.join("ssh_config"))
        .args(["-T", "-o", "BatchMode=yes", setup::HOST])
        .arg(remote)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    use std::io::Write;
    let _ = child.stdin.take().map(|mut stdin| stdin.write_all(lines.as_bytes()));
    let out = child.wait_with_output().map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Sill could not put the folders in place in its VM: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn only_folders_of_mac_users_and_volumes_are_shared() {
        assert!(super::mac_folder(Path::new("/Users/someone/GIT/sill")));
        assert!(super::mac_folder(Path::new("/Volumes/Backup")));
        for refused in ["/Users", "/usr/local", "/private/tmp", "/opt/homebrew", "/"] {
            assert!(!super::mac_folder(Path::new(refused)), "{refused}");
        }
    }

    #[test]
    fn base64_is_standard() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"sill\n"), "c2lsbAo=");
    }
}
