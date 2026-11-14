//! The Mac window's own Linux VM, the box its daemon runs in (docs/sill-stick.md section 9).
//!
//! Without `SILL_BOX`, sill-gui on the Mac starts the VM in this process at launch — on a thread
//! of its own, the window showing how far it is (`phase`) — and talks to its daemon as to any box,
//! over SSH through the VM's local socket. The VM ends with the process; Quit asks the guest to
//! shut down first (`shut_down`). Settings can also shut it down, start it and restart it
//! (`stop`, `start`, `restart`), choose its processors and memory for its next start
//! (`set_resources`, kept in `resources.json`), and show what it is (`info`; `vm_ui.rs`).
//!
//! A folder of the Mac the person pins is shared with the VM and appears there at its Mac path
//! (`expose`), so the VM, its jails and the person see one path; the folders are kept in
//! `folders.json` by share slot and shared again at each start. Settings can unshare one
//! (`unshare`): its slot stays empty, so no other folder moves to another slot.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use sill_client::Connector;
use sill_vm::setup;

pub use crate::vm_ui::Phase;
use crate::vm_ui::{Info, Resources};

struct Shared {
    phase: Mutex<Phase>,
    changed: Condvar,
    vm: Mutex<Option<Arc<sill_vm::Vm>>>,
    /// The thread carrying SSH to the guest; it holds the VM until a connection finds it stopped.
    serve: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// What the running VM was started with: processors and GB of memory.
    running: Mutex<Option<(usize, u64)>>,
    /// A start, stop or restart under way: one at a time.
    acting: Mutex<()>,
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
    SHARED.get_or_init(|| Shared {
        phase: Mutex::new(Phase::Checking),
        changed: Condvar::new(),
        vm: Mutex::new(None),
        serve: Mutex::new(None),
        running: Mutex::new(None),
        acting: Mutex::new(()),
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
            let Some(shared) = SHARED.get() else { return };
            let _acting = shared.acting.lock().unwrap();
            if let Err(said) = run(&dir) {
                set(Phase::Failed(said));
            }
        });
    if spawned.is_err() {
        set(Phase::Failed("Sill could not start its VM.".into()));
    }
}

fn run(dir: &std::path::Path) -> Result<(), String> {
    set(Phase::Checking);
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
    let config = config(dir);
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
    let serve = sill_vm::serve_ssh(vm.clone(), &dir.join("ssh.sock"))
        .map_err(|e| format!("Sill could not reach its VM: {e:#}"))?;
    if let Some(shared) = SHARED.get() {
        *shared.vm.lock().unwrap() = Some(vm.clone());
        *shared.serve.lock().unwrap() = Some(serve);
        *shared.running.lock().unwrap() = Some((config.cpus, config.memory_gb));
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

/// The VM's make: sized for this Mac, with what Settings chose in its place.
fn config(dir: &Path) -> sill_vm::Config {
    let mut config = sill_vm::Config::sized_for_this_mac(dir.to_path_buf());
    let chosen = load_resources(dir);
    let (max_cpus, max_memory) = limits();
    if let Some(cpus) = chosen.cpus {
        config.cpus = cpus.clamp(2, max_cpus);
    }
    if let Some(memory) = chosen.memory_gb {
        config.memory_gb = memory.clamp(2, max_memory);
    }
    config
}

/// The most the VM may be given: every processor, half the Mac's memory.
fn limits() -> (usize, u64) {
    let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
    let memory = sill_vm::host_memory_bytes().map_or(2, |bytes| bytes / 2 / (1 << 30));
    (cpus.max(2), memory.max(2))
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
        Phase::Stopping => ("Shutting down Sill's VM…".into(), false),
        Phase::Stopped => ("Sill's VM is shut down. Start it again in Settings.".into(), true),
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
            .wait_timeout_while(phase, READY_WAIT, |phase| phase.moving())
            .unwrap();
        match &*phase {
            Phase::Ready => {}
            Phase::Failed(said) => anyhow::bail!("{said}"),
            Phase::Stopped => anyhow::bail!("Sill's VM is shut down"),
            _ => anyhow::bail!("Sill's VM is still starting"),
        }
        drop(phase);
        sill_proto::Client::connect()
    })
}

/// Shut the VM down at Quit: the guest is asked first and given a little while, then stopped.
pub fn shut_down() {
    let Some(shared) = SHARED.get() else { return };
    let vm = shared.vm.lock().unwrap().take();
    if let Some(vm) = vm {
        let _ = sill_proto::boxlink::disconnect(setup::HOST);
        let _ = vm.shut_down(Duration::from_secs(15));
    }
}

/// Shut the VM down from Settings, and let go of it so it can start again: the guest is asked
/// first. Blocks: call it off the window's thread.
pub fn stop() -> Result<(), String> {
    let (Some(dir), Some(shared)) = (DIR.get(), SHARED.get()) else {
        return Err("Sill's VM is not in use.".into());
    };
    let _acting = shared.acting.lock().unwrap();
    stop_held(dir, shared)
}

fn stop_held(dir: &Path, shared: &Shared) -> Result<(), String> {
    set(Phase::Stopping);
    let vm = shared.vm.lock().unwrap().take();
    let mut result = Ok(());
    if let Some(vm) = vm {
        let _ = sill_proto::boxlink::disconnect(setup::HOST);
        result = vm
            .shut_down(Duration::from_secs(15))
            .map_err(|e| format!("Sill's VM did not stop: {e:#}"));
        drop(vm);
    }
    // the SSH thread lets go of the VM, and with it the VM's disks and lock, at a connection
    // that finds it stopped and held by nothing else: knock until it has, for a while
    let serve = shared.serve.lock().unwrap().take();
    if let Some(serve) = serve {
        for _ in 0..50 {
            if serve.is_finished() {
                break;
            }
            let _ = std::os::unix::net::UnixStream::connect(dir.join("ssh.sock"));
            std::thread::sleep(Duration::from_millis(100));
        }
        if serve.is_finished() {
            let _ = serve.join();
        } else if result.is_ok() {
            result = Err("Sill's VM stopped but is still held; quit Sill and open it again.".into());
        }
    }
    *shared.running.lock().unwrap() = None;
    match result {
        Ok(()) => {
            set(Phase::Stopped);
            Ok(())
        }
        Err(said) => {
            set(Phase::Failed(said.clone()));
            Err(said)
        }
    }
}

/// Start the VM after it was shut down or failed; returns once it answers, or why it did not.
/// Blocks: call it off the window's thread.
pub fn start() -> Result<(), String> {
    let (Some(dir), Some(shared)) = (DIR.get(), SHARED.get()) else {
        return Err("Sill's VM is not in use.".into());
    };
    let _acting = shared.acting.lock().unwrap();
    start_held(dir, shared)
}

fn start_held(dir: &Path, shared: &Shared) -> Result<(), String> {
    if shared.vm.lock().unwrap().is_some() {
        // a VM that failed after it was made (it did not answer): let go of it first
        stop_held(dir, shared)?;
    }
    run(dir).inspect_err(|said| set(Phase::Failed(said.clone())))
}

/// Shut the VM down and start it again: the processors and memory chosen apply.
pub fn restart() -> Result<(), String> {
    let (Some(dir), Some(shared)) = (DIR.get(), SHARED.get()) else {
        return Err("Sill's VM is not in use.".into());
    };
    let _acting = shared.acting.lock().unwrap();
    stop_held(dir, shared)?;
    start_held(dir, shared)
}

/// Keep the processors and memory for the VM's next start.
pub fn set_resources(chosen: Resources) -> Result<(), String> {
    let dir = DIR.get().ok_or("Sill's VM is not in use.")?;
    let temp = dir.join("resources.json.new");
    let bytes = serde_json::to_vec_pretty(&chosen).map_err(|e| e.to_string())?;
    std::fs::write(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, dir.join("resources.json")))
        .map_err(|e| format!("Sill could not save the VM's settings: {e}"))
}

fn load_resources(dir: &Path) -> Resources {
    std::fs::read(dir.join("resources.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// What Settings shows of the VM: read from its directory, a few file sizes.
pub fn info() -> Option<Info> {
    use std::os::unix::fs::MetadataExt;
    let (dir, shared) = (DIR.get()?, SHARED.get()?);
    // the space a disk takes on the Mac: they are sparse, so less than their size
    let used = |name: &str| std::fs::metadata(dir.join(name)).ok().map(|m| m.blocks() * 512);
    let automatic = sill_vm::Config::sized_for_this_mac(dir.clone());
    let image = std::fs::read_to_string(dir.join("system.commit"))
        .ok()
        .map(|c| c.trim().chars().take(12).collect::<String>())
        .filter(|c| !c.is_empty());
    Some(Info {
        dir: dir.clone(),
        image,
        system_used: used("system.raw"),
        home_used: used("home.raw"),
        state_used: used("state.raw"),
        data_size: std::fs::metadata(dir.join("home.raw")).ok().map(|m| m.len()),
        running: *shared.running.lock().unwrap(),
        chosen: load_resources(dir),
        automatic: (automatic.cpus, automatic.memory_gb),
        max: limits(),
        folders: load_folders(dir)
            .into_iter()
            .enumerate()
            .filter_map(|(slot, path)| path.map(|p| (slot, p.clone(), p.is_dir())))
            .collect(),
        slots: sill_vm::SHARE_SLOTS,
    })
}

/// Show the VM's folder in the Finder.
pub fn reveal() -> Result<(), String> {
    let dir = DIR.get().ok_or("Sill's VM is not in use.")?;
    std::process::Command::new("/usr/bin/open")
        .arg(dir)
        .spawn()
        .map(drop)
        .map_err(|e| format!("Could not open {}: {e}", dir.display()))
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
    let vm = shared.vm.lock().unwrap().clone().ok_or("Sill's VM is not running.")?;
    let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !mac_folder(&path) {
        return Err(format!(
            "{} cannot be shared: only folders in a user's folder (/Users) or on a disk (/Volumes) can.",
            path.display()
        ));
    }
    let mut folders = load_folders(dir);
    // inside a folder already shared, it is in the VM already
    if !folders.iter().flatten().any(|f| path.starts_with(f)) {
        let slot = free_slot(&folders).ok_or_else(|| {
            format!(
                "Sill can share up to {} folders of this Mac with its VM.",
                sill_vm::SHARE_SLOTS
            )
        })?;
        if slot == folders.len() {
            folders.push(Some(path.clone()));
        } else {
            folders[slot] = Some(path.clone());
        }
        save_folders(dir, &folders).map_err(|e| format!("Sill could not save its folders: {e}"))?;
        // the new folder's own slot: the others stay as they are
        vm.share_slot(slot, Some(&path))
            .map_err(|e| format!("Sill could not share {}: {e:#}", path.display()))?;
    }
    mount_folders(dir, &folders)?;
    Ok(path)
}

/// Stop sharing `path` with the VM: unmounted in the guest, its slot emptied and left empty.
/// Sessions that pin something in it lose it. Runs SSH: call it off the window's thread.
pub fn unshare(path: &Path) -> Result<(), String> {
    let (Some(dir), Some(shared)) = (DIR.get(), SHARED.get()) else {
        return Err("Sill's VM is not in use.".into());
    };
    let mut folders = load_folders(dir);
    let slot = folders
        .iter()
        .position(|f| f.as_deref() == Some(path))
        .ok_or_else(|| format!("{} is not shared.", path.display()))?;
    let vm = shared.vm.lock().unwrap().clone();
    if let Some(vm) = vm {
        // in the guest first, so nothing there is left reading a share that went
        if phase() == Some(Phase::Ready) {
            unmount_folder(dir, path)?;
        }
        vm.share_slot(slot, None)
            .map_err(|e| format!("Sill could not unshare {}: {e:#}", path.display()))?;
    }
    folders[slot] = None;
    save_folders(dir, &trimmed(folders)).map_err(|e| format!("Sill could not save its folders: {e}"))
}

/// The first empty share slot, or the next one after the last if there is room.
fn free_slot(folders: &[Option<PathBuf>]) -> Option<usize> {
    folders
        .iter()
        .position(Option::is_none)
        .or((folders.len() < sill_vm::SHARE_SLOTS).then_some(folders.len()))
}

/// Without the empty slots at the end.
fn trimmed(mut folders: Vec<Option<PathBuf>>) -> Vec<Option<PathBuf>> {
    while folders.last().is_some_and(Option::is_none) {
        folders.pop();
    }
    folders
}

/// A folder of a Mac user's or on a Mac volume: nothing that could cover the guest's own.
fn mac_folder(path: &Path) -> bool {
    let mut parts = path.components().skip(1);
    matches!(parts.next(), Some(std::path::Component::Normal(top)) if top == "Users" || top == "Volumes")
        && parts.next().is_some()
}

/// The shared folders by share slot, `null` for an empty one (a plain list of paths reads too).
fn load_folders(dir: &Path) -> Vec<Option<PathBuf>> {
    std::fs::read(dir.join("folders.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_folders(dir: &Path, folders: &[Option<PathBuf>]) -> std::io::Result<()> {
    let temp = dir.join("folders.json.new");
    std::fs::write(&temp, serde_json::to_vec_pretty(folders)?)?;
    std::fs::rename(temp, dir.join("folders.json"))
}

/// At the VM's start: every kept folder in its slot, then mounted.
fn sync_folders(dir: &Path, vm: &sill_vm::Vm) -> Result<(), String> {
    let folders = load_folders(dir);
    for (slot, path) in folders.iter().enumerate().take(sill_vm::SHARE_SLOTS) {
        let present = path.as_deref().filter(|p| p.is_dir());
        if present.is_none() && path.is_none() {
            continue;
        }
        vm.share_slot(slot, present).map_err(|e| {
            format!("Sill could not share {}: {e:#}", path.as_deref().unwrap_or(Path::new("")).display())
        })?;
    }
    mount_folders(dir, &folders)
}

/// The guest's root helper, sent with each use (`packaging/mac/guest/sill-mac-folders`).
const MOUNTER: &str = include_str!("../../../packaging/mac/guest/sill-mac-folders");

/// Mount each shared folder at its Mac path inside the VM, as root over the VM's SSH.
fn mount_folders(dir: &Path, folders: &[Option<PathBuf>]) -> Result<(), String> {
    let lines: String = folders
        .iter()
        .enumerate()
        .take(sill_vm::SHARE_SLOTS)
        .filter_map(|(slot, path)| Some((slot, path.as_ref()?)))
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
    guest(dir, &remote, &lines)
        .map_err(|said| format!("Sill could not put the folders in place in its VM: {said}"))
}

/// Unmount a shared folder's Mac path in the guest, if it is mounted there; the path on stdin.
fn unmount_folder(dir: &Path, path: &Path) -> Result<(), String> {
    let remote = "IFS= read -r p && case \"$p\" in /Users/?*|/Volumes/?*) ;; *) exit 2 ;; esac \
                  && if findmnt -n --mountpoint \"$p\" >/dev/null 2>&1; then sudo umount -l -- \"$p\"; fi";
    guest(dir, remote, &format!("{}\n", path.display()))
        .map_err(|said| format!("Sill could not unmount {} in its VM: {said}", path.display()))
}

/// Run `remote` in the guest over the VM's SSH, `input` on its stdin; why it failed, if it did.
fn guest(dir: &Path, remote: &str, input: &str) -> Result<(), String> {
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
    let _ = child.stdin.take().map(|mut stdin| stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
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
    use std::path::{Path, PathBuf};

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

    #[test]
    fn an_unshared_folder_leaves_its_slot_empty_for_the_next() {
        let a = Some(PathBuf::from("/Users/a/one"));
        let c = Some(PathBuf::from("/Users/a/three"));
        let folders = vec![a.clone(), None, c.clone()];
        assert_eq!(super::free_slot(&folders), Some(1));
        assert_eq!(super::free_slot(&[a.clone()]), Some(1));
        assert_eq!(super::trimmed(vec![a.clone(), None, None]), vec![a.clone()]);
        let full = vec![a.clone(); sill_vm::SHARE_SLOTS];
        assert_eq!(super::free_slot(&full), None);
    }

    #[test]
    fn the_earlier_plain_list_of_folders_still_reads() {
        let dir = std::env::temp_dir().join(format!("sill-vm-folders-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("folders.json"), r#"["/Users/a/one", "/Users/a/two"]"#).unwrap();
        assert_eq!(super::load_folders(&dir).len(), 2);
        super::save_folders(&dir, &[None, Some(PathBuf::from("/Users/a/two"))]).unwrap();
        assert_eq!(super::load_folders(&dir), vec![None, Some(PathBuf::from("/Users/a/two"))]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
