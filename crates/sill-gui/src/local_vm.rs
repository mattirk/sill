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
//!
//! The VM's clock is the Mac's: set at each start and after each sleep of the Mac (`watch_clock`),
//! since the VM does not run while the Mac sleeps and its clock stays behind by that long.
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
        sill_proto::transport::BoxTarget { host: setup::HOST.into(), binary: setup::SILL.into(), ssh_config: Some(dir.join("ssh_config")) },
    );
    let spawned = std::thread::Builder::new().name("sill-gui-vm".into()).spawn(move || {
        let Some(shared) = SHARED.get() else { return };
        let _acting = shared.acting.lock().unwrap();
        if let Err(said) = run(&dir) {
            set(Phase::Failed(said));
        }
    });
    if spawned.is_err() {
        set(Phase::Failed("Sill could not start its VM.".into()));
    }
    // for as long as the process: the VM may be stopped and started again from Settings
    let _ = std::thread::Builder::new().name("sill-gui-vm-clock".into()).spawn(watch_clock);
}

fn run(dir: &std::path::Path) -> Result<(), String> {
    set(Phase::Checking);
    let problems = sill_vm::check(dir);
    if !problems.is_empty() {
        return Err(problems.iter().map(|p| format!("{} {}", p.what, p.fix)).collect::<Vec<_>>().join("\n"));
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
            "This copy of Sill is not allowed to run virtual machines (it is not signed for it).".to_string()
        } else {
            format!("Sill's VM did not start: {said}")
        }
    })?;
    let vm = Arc::new(vm);
    let serve = sill_vm::serve_ssh(vm.clone(), &dir.join("ssh.sock")).map_err(|e| format!("Sill could not reach its VM: {e:#}"))?;
    if let Some(shared) = SHARED.get() {
        *shared.vm.lock().unwrap() = Some(vm.clone());
        *shared.serve.lock().unwrap() = Some(serve);
        *shared.running.lock().unwrap() = Some((config.cpus, config.memory_gb));
    }
    vm.wait_for_ssh(Duration::from_secs(180)).map_err(|e| format!("Sill's VM started but does not answer: {e:#}"))?;
    // the Mac folders pinned before, back at their paths before any session starts
    if let Err(e) = sync_folders(dir, &vm) {
        eprintln!("sill-gui: Mac folders: {e}");
    }
    // sill-mac: the Mac's side of Host control's Mac operations, for the daemon in the VM
    if let Err(e) = serve_mac(dir, &vm) {
        eprintln!("sill-gui: Mac operations: {e}");
    }
    // the Mac's time, should the VM have started with another
    if let Err(e) = sync_clock(dir) {
        eprintln!("sill-gui: the VM's clock: {e}");
    }
    // the person's git identity, so commits made in the VM (agents' in their jails too) are theirs
    if let Err(e) = share_git_identity(dir) {
        eprintln!("sill-gui: git identity: {e}");
    }
    // last, so it mostly overlaps the steps above: sessions start once the guest has its network
    if let Err(e) = wait_for_network(dir) {
        eprintln!("sill-gui: the VM's network: {e}");
    }
    set(Phase::Ready);
    Ok(())
}

/// How long a start waits for the guest's network before it goes on without it.
const NETWORK_WAIT_SECS: u32 = 30;

/// Wait until the guest's network is up. SSH over the VM's socket answers before it is, and an
/// agent's first request failed for it (docs/sill-stick.md section 9). macOS's own DHCP gives the
/// guest its address whether the Mac is online or not, so this measures the VM, not the Internet;
/// past the limit the start goes on and an agent's request fails as on any box without network.
fn wait_for_network(dir: &Path) -> Result<(), String> {
    let remote = format!(
        "/usr/lib/systemd/systemd-networkd-wait-online --quiet --any --ipv4 \
         --operational-state=routable --timeout={NETWORK_WAIT_SECS}"
    );
    guest(dir, &remote, "").map_err(|said| if said.is_empty() { format!("no network after {NETWORK_WAIT_SECS} s") } else { said })
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
        let (phase, _) = shared.changed.wait_timeout_while(phase, READY_WAIT, |phase| phase.moving()).unwrap();
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

/// Ctrl-C in the terminal, `kill`, logging out: the guest is shut down cleanly before the
/// process ends. Without it the VM went with the process at once, and what the guest had not
/// yet written to its disks was lost (2026-11-12: a commit in a session's workspace came back
/// as an empty object file after `run-gui.sh` was stopped with Ctrl-C).
pub fn shut_down_on_signals() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ASKED: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_signal(_: libc::c_int) {
        ASKED.store(true, Ordering::Relaxed);
    }
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: an async-signal-safe handler that only stores a flag
        unsafe { libc::signal(signal, on_signal as *const () as libc::sighandler_t) };
    }
    let _ = std::thread::Builder::new().name("sill-gui-signals".into()).spawn(|| {
        while !ASKED.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("sill-gui: shutting the VM down");
        shut_down();
        std::process::exit(0);
    });
}

/// Shut the VM down at Quit: the guest is asked first and given a little while, then stopped.
pub fn shut_down() {
    // the agents' Mac programs end with Sill (the person, 2026-11-13)
    crate::mac_procs::stop_all();
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
    // the agents' Mac programs end with the VM their sessions ran in, as they do when Sill quits
    // (the person, 2026-11-13); with the VM down nothing could see or stop them
    crate::mac_procs::stop_all();
    // every Host control activation ends with the daemon, and the next daemon numbers its
    // generations from 1 again: what the person switched on before must not match those
    forget_grants();
    let vm = shared.vm.lock().unwrap().take();
    let mut result = Ok(());
    if let Some(vm) = vm {
        let _ = sill_proto::boxlink::disconnect(setup::HOST);
        result = vm.shut_down(Duration::from_secs(15)).map_err(|e| format!("Sill's VM did not stop: {e:#}"));
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
    std::fs::read(dir.join("resources.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// What Settings shows of the VM: read from its directory, a few file sizes.
pub fn info() -> Option<Info> {
    use std::os::unix::fs::MetadataExt;
    let (dir, shared) = (DIR.get()?, SHARED.get()?);
    // the space a disk takes on the Mac: they are sparse, so less than their size
    let used = |name: &str| std::fs::metadata(dir.join(name)).ok().map(|m| m.blocks() * 512);
    let automatic = sill_vm::Config::sized_for_this_mac(dir.clone());
    let image = std::fs::read_to_string(dir.join("system.commit")).ok().map(|c| c.trim().chars().take(12).collect::<String>()).filter(|c| !c.is_empty());
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
        folders: load_folders(dir).into_iter().enumerate().filter_map(|(slot, path)| path.map(|p| (slot, p.clone(), p.is_dir()))).collect(),
        slots: sill_vm::SHARE_SLOTS,
    })
}

/// Show the VM's folder in the Finder.
pub fn reveal() -> Result<(), String> {
    let dir = DIR.get().ok_or("Sill's VM is not in use.")?;
    std::process::Command::new("/usr/bin/open").arg(dir).spawn().map(drop).map_err(|e| format!("Could not open {}: {e}", dir.display()))
}

/// Where the system image is: `SILL_VM_IMAGE`, the app bundle's resources (`Resources/vm`,
/// compressed), or a build of it in
/// the repository the program was built in (`packaging/mac-image.sh`, `dist/mac-image`).
fn image_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SILL_VM_IMAGE") {
        return Some(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().ok()?;
    let has_image = |dir: &std::path::Path| dir.join("system.raw").exists() || dir.join("system.raw.gz").exists();
    if let Some(resources) = exe.parent().and_then(|macos| macos.parent()).map(|c| c.join("Resources/vm"))
        && has_image(&resources)
    {
        return Some(resources);
    }
    exe.ancestors().map(|dir| dir.join("dist/mac-image")).find(|dir| has_image(dir))
}

/// Make the Mac folder `path` appear at the same path in the VM, for pinning it there; it stays
/// shared from then on. Runs SSH: call it off the window's thread.
pub fn expose(path: &Path) -> Result<PathBuf, String> {
    let (Some(dir), Some(shared)) = (DIR.get(), SHARED.get()) else {
        return Err("Sill's VM is not in use.".into());
    };
    // after a start, stop or restart under way: the folders are read and mounted at a start
    let _acting = shared.acting.lock().unwrap();
    let vm = shared.vm.lock().unwrap().clone().ok_or("Sill's VM is not running.")?;
    let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !mac_folder(&path) {
        return Err(format!("{} cannot be shared: only folders in a user's folder (/Users) or on a disk (/Volumes) can.", path.display()));
    }
    let mut folders = load_folders(dir);
    // inside a folder already shared, it is in the VM already
    if !folders.iter().flatten().any(|f| path.starts_with(f)) {
        let slot = free_slot(&folders).ok_or_else(|| format!("Sill can share up to {} folders of this Mac with its VM.", sill_vm::SHARE_SLOTS))?;
        if slot == folders.len() {
            folders.push(Some(path.clone()));
        } else {
            folders[slot] = Some(path.clone());
        }
        save_folders(dir, &folders).map_err(|e| format!("Sill could not save its folders: {e}"))?;
        // the new folder's own slot: the others stay as they are
        vm.share_slot(slot, Some(&path)).map_err(|e| format!("Sill could not share {}: {e:#}", path.display()))?;
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
    // after a start, stop or restart under way: the folders are read and mounted at a start
    let _acting = shared.acting.lock().unwrap();
    let mut folders = load_folders(dir);
    let slot = folders.iter().position(|f| f.as_deref() == Some(path)).ok_or_else(|| format!("{} is not shared.", path.display()))?;
    let vm = shared.vm.lock().unwrap().clone();
    if let Some(vm) = vm {
        // in the guest first, so nothing there is left reading a share that went
        if phase() == Some(Phase::Ready) {
            unmount_folder(dir, path)?;
        }
        vm.share_slot(slot, None).map_err(|e| format!("Sill could not unshare {}: {e:#}", path.display()))?;
    }
    folders[slot] = None;
    save_folders(dir, &trimmed(folders)).map_err(|e| format!("Sill could not save its folders: {e}"))
}

/// The first empty share slot, or the next one after the last if there is room.
fn free_slot(folders: &[Option<PathBuf>]) -> Option<usize> {
    folders.iter().position(Option::is_none).or((folders.len() < sill_vm::SHARE_SLOTS).then_some(folders.len()))
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
    matches!(parts.next(), Some(std::path::Component::Normal(top)) if top == "Users" || top == "Volumes") && parts.next().is_some()
}

/// The shared folders by share slot, `null` for an empty one (a plain list of paths reads too).
fn load_folders(dir: &Path) -> Vec<Option<PathBuf>> {
    std::fs::read(dir.join("folders.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
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
        vm.share_slot(slot, present).map_err(|e| format!("Sill could not share {}: {e:#}", path.as_deref().unwrap_or(Path::new("")).display()))?;
    }
    mount_folders(dir, &folders)
}

/// Mount each shared folder at its Mac path inside the VM: the image's root helper
/// (`packaging/mac/guest/sill-mac-folders`), the folders on its stdin.
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
    as_root(dir, "mount", &lines).map_err(|said| format!("Sill could not put the folders in place in its VM: {}", older_image(said)))
}

/// Who the person's commits are by on the Mac: git's own answer (`user.name` and `user.email`,
/// else what git makes of the account), read as values only.
fn mac_git_identity() -> Option<(String, String)> {
    let out = std::process::Command::new("/usr/bin/git").args(["var", "GIT_AUTHOR_IDENT"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    // "Name <email> 1700000000 +0200"
    let ident = String::from_utf8_lossy(&out.stdout);
    let (name, rest) = ident.trim().split_once(" <")?;
    let (email, _) = rest.split_once('>')?;
    (!name.trim().is_empty() && !email.trim().is_empty()).then(|| (name.trim().to_string(), email.trim().to_string()))
}

/// Give the VM's account the person's git identity, at every start: the VM has no account
/// details git could make one from, so a commit there failed with no author (the person,
/// 2026-11-13). Written to the VM user's global config, which the jails see through their home.
fn share_git_identity(dir: &Path) -> Result<(), String> {
    let (name, email) = mac_git_identity().ok_or("git on the Mac has no identity to share")?;
    if name.contains('\n') || email.contains('\n') {
        return Err("the identity spans lines".into());
    }
    let remote = "IFS= read -r n && IFS= read -r e && git config --global user.name \"$n\" && git config --global user.email \"$e\"";
    guest(dir, remote, &format!("{name}\n{email}\n"))
}

/// How often the Mac's two clocks are compared, and how far apart they may move in that time
/// before it counts as a sleep (or the person setting the Mac's clock).
const CLOCK_CHECK: Duration = Duration::from_secs(5);
const CLOCK_SLACK: Duration = Duration::from_secs(2);

/// Whether the Mac's clock moved otherwise than its running time did: `wall` is how far the clock
/// went (`None`: backwards), `ran` how long the Mac ran meanwhile. The running time stands still
/// while the Mac sleeps, so a sleep shows as the clock ahead of it; a clock set by hand or by the
/// network shows either way.
fn clock_moved(wall: Option<Duration>, ran: Duration) -> bool {
    wall.is_none_or(|wall| wall.abs_diff(ran) > CLOCK_SLACK)
}

/// Give the VM the Mac's time whenever the Mac's clock moved without the VM: after a sleep, when
/// the VM's clock is behind by the length of it. Every session's timer then showed the sleep (the
/// window counts from the daemon's times by the Mac's clock), and an agent's sign-in was refused,
/// its token long expired by any clock but the VM's (the person, 2026-11-16, after a night's
/// sleep: 11 hours). The VM's own time service corrects it only when it next asks the network.
/// Asked again at the next check until the VM took it; an image without the request is told once.
fn watch_clock() {
    let (mut wall, mut ran) = (std::time::SystemTime::now(), std::time::Instant::now());
    let (mut owed, mut told) = (false, false);
    loop {
        std::thread::sleep(CLOCK_CHECK);
        let (wall_now, ran_now) = (std::time::SystemTime::now(), std::time::Instant::now());
        owed |= clock_moved(wall_now.duration_since(wall).ok(), ran_now.duration_since(ran));
        (wall, ran) = (wall_now, ran_now);
        if phase() != Some(Phase::Ready) {
            // a VM not up gets the time as it starts (`run`)
            owed = false;
        }
        if !owed {
            continue;
        }
        let Some(dir) = DIR.get() else { return };
        match sync_clock(dir) {
            Ok(()) => owed = false,
            Err(said) => {
                if !told {
                    eprintln!("sill-gui: the VM's clock: {said}");
                    told = true;
                }
                // an image without the request never takes it; anything else may pass
                owed = !said.contains("older than this Sill");
            }
        }
    }
}

/// Set the VM's clock to the Mac's, by the guest's root helper (which leaves a clock within two
/// seconds alone).
fn sync_clock(dir: &Path) -> Result<(), String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_| "the Mac's clock is before 1970".to_string())?.as_secs();
    as_root(dir, "clock", &format!("{now}\n")).map_err(older_image)
}

/// Unmount a shared folder's Mac path in the guest, if it is mounted there; the path on stdin.
fn unmount_folder(dir: &Path, path: &Path) -> Result<(), String> {
    as_root(dir, "unmount", &format!("{}\n", path.display()))
        .map_err(|said| format!("Sill could not unmount {} in its VM: {}", path.display(), older_image(said)))
}

/// What the helper's absence means: an image from before it was part of the image (2026-11-13),
/// when the VM's account had sudo and the helper travelled with each use.
fn older_image(said: String) -> String {
    // no helper there (sh and bash say it differently), or root's SSH never given the key
    let no_helper = said.contains("sill-mac-folders: not found") || said.contains("sill-mac-folders: No such file");
    // or a helper from before it set the clock (2026-11-16)
    let no_request = said.contains("sill-mac-folders: mount or unmount");
    if no_helper || no_request || said.contains("Permission denied") {
        format!("{said} (this VM's system image is older than this Sill: build the image again, packaging/mac-image.sh)")
    } else {
        said
    }
}

/// Run the guest's root helper (`sill-mac-folders`) with `request`, `input` on its stdin. The
/// VM's account has no sudo: root's SSH lets the VM's key in for that helper alone
/// (`setup::user_data`), so `request` only chooses between what the helper does.
fn as_root(dir: &Path, request: &str, input: &str) -> Result<(), String> {
    ssh(dir, Some("root"), request, input)
}

/// Run `remote` in the guest as the VM's account, `input` on its stdin; why it failed, if it did.
fn guest(dir: &Path, remote: &str, input: &str) -> Result<(), String> {
    ssh(dir, None, remote, input)
}

/// Over the VM's SSH, as `user` or the VM's account.
fn ssh(dir: &Path, user: Option<&str>, remote: &str, input: &str) -> Result<(), String> {
    let mut command = std::process::Command::new("ssh");
    command.arg("-F").arg(dir.join("ssh_config")).args(["-T", "-o", "BatchMode=yes"]);
    if let Some(user) = user {
        command.args(["-l", user]);
    }
    let mut child = command
        .arg(setup::HOST)
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
    fn a_sleep_or_a_set_clock_moves_the_clock_and_plain_running_does_not() {
        use std::time::Duration;
        let s = Duration::from_secs;
        assert!(!super::clock_moved(Some(s(5)), s(5)));
        assert!(!super::clock_moved(Some(Duration::from_millis(6500)), s(5)), "a late wake-up of the thread");
        assert!(super::clock_moved(Some(s(11 * 3600)), s(5)), "the Mac slept");
        assert!(super::clock_moved(Some(s(1)), s(5)), "set back a little");
        assert!(super::clock_moved(None, s(5)), "set back past the last check");
        let old = super::older_image("sill-mac-folders: mount or unmount".into());
        assert!(old.contains("older than this Sill"), "{old}");
    }

    #[test]
    fn an_unshared_folder_leaves_its_slot_empty_for_the_next() {
        let a = Some(PathBuf::from("/Users/a/one"));
        let c = Some(PathBuf::from("/Users/a/three"));
        let folders = vec![a.clone(), None, c.clone()];
        assert_eq!(super::free_slot(&folders), Some(1));
        assert_eq!(super::free_slot(std::slice::from_ref(&a)), Some(1));
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

// --- sill-mac: the Mac's side of Host control's Mac operations (docs/sill-mac.md) -------------

/// Where the daemon in the VM asks for the Mac: the VM's socket device, this port.
const MAC_PORT: u32 = 7695;
/// The largest request line accepted.
const MAC_REQUEST_MAX: u64 = 64 << 10;

/// At each VM start: a new secret for the daemon, written into its runtime directory (which jails
/// never see), and the port the daemon reaches the Mac on. A request without the secret is
/// refused: vsock is not confined by the jail's network namespace, so a jailed process could
/// reach the port itself.
fn serve_mac(dir: &Path, vm: &sill_vm::Vm) -> Result<(), String> {
    let mut secret = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut secret)).map_err(|e| format!("no randomness: {e}"))?;
    let secret: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    let mut child = std::process::Command::new("ssh")
        .arg("-F")
        .arg(dir.join("ssh_config"))
        .args(["-T", "-o", "BatchMode=yes", setup::HOST])
        .arg("umask 077 && mkdir -p \"$XDG_RUNTIME_DIR/sill\" && cat > \"$XDG_RUNTIME_DIR/sill/mac.token\"")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    {
        use std::io::Write;
        let _ = child.stdin.take().map(|mut stdin| stdin.write_all(secret.as_bytes()));
    }
    let out = child.wait_with_output().map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!("the daemon's secret could not be written: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let secret = Arc::new(secret);
    watch_mac_processes();
    vm.listen(
        MAC_PORT,
        Arc::new(move |stream| {
            let secret = secret.clone();
            if let Err(e) = answer_mac(stream, &secret) {
                eprintln!("sill-gui: Mac operation: {e}");
            }
        }),
    )
    .map_err(|e| format!("{e:#}"))
}

/// One request from the daemon: checked, done, answered (a JSON line, then the picture's bytes).
fn answer_mac(stream: std::os::unix::net::UnixStream, secret: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    // Absolute deadlines stop slow senders/readers from retaining a worker indefinitely.
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    let read_until = std::time::Instant::now() + Duration::from_secs(5);
    let mut stream = stream;
    let mut line = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let left = read_until.checked_duration_since(std::time::Instant::now()).ok_or("Mac request timed out")?;
        stream.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("incomplete Mac request".into());
        }
        let end = chunk[..n].iter().position(|b| *b == b'\n');
        line.extend_from_slice(&chunk[..end.map_or(n, |i| i + 1)]);
        if line.len() as u64 > MAC_REQUEST_MAX {
            return Err("Mac request too large".into());
        }
        if end.is_some() {
            break;
        }
    }
    let line = std::str::from_utf8(&line).map_err(|e| e.to_string())?;
    let (reply, picture) = match mac_request(line, secret, deadline) {
        Ok(done) => done,
        Err(said) => (serde_json::json!({"error": said}), None),
    };
    let mut head = reply;
    if let Some(bytes) = &picture {
        head["bytes"] = serde_json::json!(bytes.len());
    }
    let mut bytes = serde_json::to_vec(&head).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if let Some(picture) = picture {
        bytes.extend_from_slice(&picture);
    }
    let until = std::time::Instant::now() + Duration::from_secs(3);
    let mut pending = bytes.as_slice();
    while !pending.is_empty() {
        let left = until.checked_duration_since(std::time::Instant::now()).ok_or("Mac reply timed out")?;
        stream.set_write_timeout(Some(left)).map_err(|e| e.to_string())?;
        let n = stream.write(pending).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("Mac reply disconnected".into());
        }
        pending = &pending[n..];
    }
    Ok(())
}

fn mac_request(line: &str, secret: &str, deadline: std::time::Instant) -> Result<(serde_json::Value, Option<Vec<u8>>), String> {
    use serde_json::{Value, json};
    let request: Value = serde_json::from_str(line).map_err(|_| "not a request".to_string())?;
    let given = request["token"].as_str().unwrap_or_default();
    // the same time whatever the secret's first wrong byte
    if given.len() != secret.len() || given.bytes().zip(secret.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return Err("refused: not Sill's daemon".into());
    }
    let session = request["session"].as_str().ok_or("no session")?;
    let generation = request["generation"].as_u64().ok_or("no generation")?;
    let args = &request["args"];
    match request["op"].as_str() {
        Some("view") => granted(session, generation, "mac_view")?,
        Some("input") => {
            granted(session, generation, "mac_input")?;
            return mac_input(session, generation, args, deadline).map(|result| (json!({"result": result}), None));
        }
        Some("commands") => {
            granted(session, generation, "mac_commands")?;
            // a working directory inside a folder shared with the VM (the person, 2026-11-13)
            let shared: Vec<PathBuf> = DIR.get().map(|dir| load_folders(dir).into_iter().flatten().collect()).unwrap_or_default();
            return crate::mac_procs::handle(session, args, &shared).map(|result| (json!({"result": result}), None));
        }
        _ => return Err("unknown Mac operation".into()),
    }
    if let Some(list) = args["list"].as_str() {
        let result = match list {
            "displays" => json!({"displays": crate::mac_screen::displays()}),
            "windows" => json!({
                "windows": crate::mac_screen::windows(),
                "titles": crate::mac_screen::allowed(),
            }),
            _ => return Err("list is \"displays\" or \"windows\"".into()),
        };
        return Ok((json!({"result": result}), None));
    }
    let target = if let Some(id) = args["display"].as_u64() {
        crate::mac_screen::Target::Display(u32::try_from(id).map_err(|_| "no such display")?)
    } else if let Some(id) = args["window"].as_u64() {
        crate::mac_screen::Target::Window(u32::try_from(id).map_err(|_| "no such window")?)
    } else {
        return Err("a picture of {\"display\":ID} or {\"window\":ID}".into());
    };
    let (about, place) = locate(&target)?;
    let (png, width, height) = crate::mac_screen::capture(&target)?;
    // a frame to aim input in: what the picture showed, where, at what size
    let frame = new_frame_id();
    FRAMES.lock().unwrap().retain(|_, f| f.taken.elapsed() < crate::mac_input::FRAME_LIFETIME);
    FRAMES.lock().unwrap().insert(
        frame.clone(),
        crate::mac_input::Frame {
            session: session.into(),
            generation,
            target,
            x: place.0,
            y: place.1,
            width: place.2,
            height: place.3,
            pixel_width: width,
            pixel_height: height,
            taken: std::time::Instant::now(),
        },
    );
    // the frame: what the picture shows and how its pixels map to the Mac's desktop, for the
    // input that comes after this slice
    let mut result = about;
    result["frame"] = json!(frame);
    result["width"] = json!(width);
    result["height"] = json!(height);
    result["captured"] = json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    Ok((json!({"result": result}), Some(png)))
}

/// Frames of the pictures taken, by id: what input is aimed in.
static FRAMES: Mutex<std::collections::BTreeMap<String, crate::mac_input::Frame>> = Mutex::new(std::collections::BTreeMap::new());

fn new_frame_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the person switched on in this window, by session: the Mac operations of the last
/// Activate and the generation the daemon gave it. The daemon runs in the VM, where anything
/// outside a jail can talk to it, so its word alone does not open the Mac (security review,
/// 2026-11-13): an activation this window did not make is refused here.
static TICKED: Mutex<std::collections::BTreeMap<String, Ticked>> = Mutex::new(std::collections::BTreeMap::new());

#[derive(Default)]
struct Ticked {
    /// The operations of an Activate still on its way to the daemon.
    asked: Option<Vec<String>>,
    /// The activation the person made.
    granted: Option<Granted>,
}

struct Granted {
    generation: u64,
    /// When the daemon made it: a daemon started again numbers its generations from 1 again, so
    /// the generation alone could match an activation made after a restart from inside the VM.
    since: Option<String>,
    operations: Vec<String>,
}

/// The person pressed Activate for `session` with these operations.
pub fn person_asked(session: &str, operations: &[String]) {
    TICKED.lock().unwrap().entry(session.into()).or_default().asked = Some(operations.to_vec());
}

/// The daemon answered the window's own request: `active` carries the activation's generation and
/// when it was made, `None` is a revocation; `failed` leaves what was granted before as it was.
pub fn person_answered(session: &str, active: Option<(u64, Option<String>)>, failed: bool) {
    let mut ticked = TICKED.lock().unwrap();
    let Some(entry) = ticked.get_mut(session) else { return };
    let asked = entry.asked.take();
    if failed {
        return;
    }
    match (active, asked) {
        (Some((generation, since)), Some(operations)) => entry.granted = Some(Granted { generation, since, operations }),
        // an answer to something else than an Activate of this window: what stands, stands
        (Some(_), None) => {}
        (None, _) => {
            ticked.remove(session);
        }
    }
}

/// The VM stops: every activation and the frames aimed under them go with its daemon.
fn forget_grants() {
    TICKED.lock().unwrap().clear();
    FRAMES.lock().unwrap().clear();
}

/// The person switched `operation` on for `session` in this window: the activation of this
/// generation, made at `since`.
fn ticked(session: &str, generation: u64, since: Option<&str>, operation: &str) -> bool {
    TICKED
        .lock()
        .unwrap()
        .get(session)
        .and_then(|t| t.granted.as_ref())
        .is_some_and(|g| g.generation == generation && g.since.as_deref() == since && g.operations.iter().any(|op| op == operation))
}

/// The session's Host control is active at this generation with `operation` granted: by the
/// person in this window (`TICKED`) and in the daemon's answer (it checked too).
fn granted(session: &str, generation: u64, operation: &str) -> Result<(), String> {
    let not_ticked = || format!("refused: {operation} was not switched on for this session in Sill's window at this generation");
    // nothing switched on here at this generation: refused without asking the daemon
    if !TICKED.lock().unwrap().get(session).and_then(|t| t.granted.as_ref()).is_some_and(|g| g.generation == generation) {
        return Err(not_ticked());
    }
    let state: sill_proto::host_control::State = sill_proto::Client::connect()
        .and_then(|client| client.call_within("_sill/host_control.state", serde_json::json!({"session_id": session}), Duration::from_secs(2)))
        .map_err(|e| format!("the grant could not be checked: {e:#}"))?;
    if !state.active || state.generation != generation {
        return Err("refused: stale Host control generation".into());
    }
    if !state.operations.iter().any(|op| op == operation) {
        return Err(format!("refused: {operation} is not granted"));
    }
    if !ticked(session, generation, state.since.as_deref(), operation) {
        return Err(not_ticked());
    }
    Ok(())
}

/// A place on the desktop in points: x, y, width, height.
type Place = (f64, f64, f64, f64);

/// A target's description and its place on the desktop, now.
fn locate(target: &crate::mac_screen::Target) -> Result<(serde_json::Value, Place), String> {
    use crate::mac_screen::Target;
    match *target {
        Target::Display(id) => {
            let d = crate::mac_screen::displays()
                .into_iter()
                .find(|d| d.id == id)
                .ok_or_else(|| format!("the Mac has no display {id}: `sill-mac displays` lists them"))?;
            let place = (d.x, d.y, d.width, d.height);
            Ok((serde_json::json!({"display": d}), place))
        }
        Target::Window(id) => {
            let w = crate::mac_screen::windows()
                .into_iter()
                .find(|w| w.id == id)
                .ok_or_else(|| format!("the Mac has no window {id} on screen: `sill-mac windows` lists them"))?;
            let place = (w.x, w.y, w.width, w.height);
            Ok((serde_json::json!({"window": w}), place))
        }
    }
}

/// The actions of one input request, in order, each checked first: the grant, the person's own
/// input, the frame's target still where it was. A stop says how far it got; nothing is repeated.
fn mac_input(session: &str, generation: u64, args: &serde_json::Value, deadline: std::time::Instant) -> Result<serde_json::Value, String> {
    use crate::mac_input as input;
    use serde_json::json;
    if !input::allowed() {
        return Err(input::NOT_ALLOWED.into());
    }
    let actions = args["actions"].as_array().filter(|a| !a.is_empty() && a.len() <= 32).ok_or("1 to 32 actions")?;
    let frame = match args["frame"].as_str() {
        Some(id) => {
            let frame = FRAMES.lock().unwrap().get(id).cloned().ok_or("no such frame: take a picture with `sill-mac screenshot` and aim in its frame")?;
            if frame.session != session || frame.generation != generation {
                return Err("that frame belongs to another session or grant: take a new picture".into());
            }
            if frame.taken.elapsed() > input::FRAME_LIFETIME {
                return Err("that frame is over two minutes old: take a new picture".into());
            }
            Some(frame)
        }
        None => None,
    };
    let _driving = input::drive().ok_or("another session is using the Mac's mouse and keyboard")?;
    // the person's own input, watched from the first request on
    input::watch();
    if input::person_active() {
        return Err("paused: the person is using the Mac's mouse or keyboard; take a new picture in a few seconds".into());
    }
    let began = input::since_now();
    let total = actions.len();
    let stop = |done: usize, why: &str| Ok(json!({"done": done, "of": total, "interrupted": why}));
    for (done, action) in actions.iter().enumerate() {
        if std::time::Instant::now() >= deadline {
            return stop(done, "input deadline reached");
        }
        if done > 0 && granted(session, generation, "mac_input").is_err() {
            return stop(done, "Host control changed or was revoked");
        }
        if input::person_since(began) {
            return stop(done, "the person used the mouse or keyboard");
        }
        // Reserve a second for bounded click/drag/key actions and their release events.
        // Check after grant RPCs too: a delayed reply must never authorize late input.
        if deadline.saturating_duration_since(std::time::Instant::now()) < Duration::from_secs(1) {
            return stop(done, "input deadline reached; take a new picture before continuing");
        }
        let aimed = |p: &serde_json::Value| -> Result<input::Point, String> {
            let frame = frame.as_ref().ok_or("an action with a point needs a frame")?;
            // the target is still where the picture showed it
            let (_, now) = locate(&frame.target)?;
            if (now.0 - frame.x).abs() > 0.5 || (now.1 - frame.y).abs() > 0.5 || (now.2 - frame.width).abs() > 0.5 || (now.3 - frame.height).abs() > 0.5 {
                return Err("the target moved or changed size since the picture: take a new one".into());
            }
            let x = p[0].as_f64().ok_or("a point is [x, y]")?;
            let y = p[1].as_f64().ok_or("a point is [x, y]")?;
            if deadline.saturating_duration_since(std::time::Instant::now()) < Duration::from_secs(1) {
                return Err("input deadline reached".into());
            }
            frame.to_desktop(x, y)
        };
        let outcome: Result<(), String> = (|| {
            if let Some(p) = action.get("move") {
                input::move_to(aimed(p)?);
            } else if let Some(p) = action.get("click") {
                let right = action["button"].as_str() == Some("right");
                let count = action["count"].as_u64().unwrap_or(1) as u32;
                input::click(aimed(p)?, right, count, began, deadline)?;
            } else if let Some(ends) = action.get("drag") {
                input::drag(aimed(&ends[0])?, aimed(&ends[1])?, began, deadline)?;
            } else if let Some(p) = action.get("scroll") {
                let at = aimed(p)?;
                input::scroll(at, action["dy"].as_f64().unwrap_or(0.0), action["dx"].as_f64().unwrap_or(0.0));
            } else if let Some(text) = action.get("type").and_then(|t| t.as_str()) {
                if text.len() > 1024 || text.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
                    return Err("text is at most 1 KiB, without control characters".into());
                }
                input::type_text(text, began, deadline)?;
            } else if let Some(combination) = action.get("key").and_then(|k| k.as_str()) {
                input::key(combination)?;
            } else if let Some(ms) = action.get("wait").and_then(|w| w.as_u64()) {
                // ends early if the person takes over: the next action's check stops the rest
                input::wait(ms.min(2000), began, deadline)?;
            } else {
                return Err("an action is move, click, drag, scroll, type, key or wait".into());
            }
            Ok(())
        })();
        if let Err(said) = outcome {
            return stop(done, &said);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Ok(json!({"done": total, "of": total}))
}

/// Every few seconds: a session whose Mac processes still run but whose `mac_commands` grant has
/// gone (revoked, activated anew, the session ended, the daemon restarted) has them stopped. Started once.
fn watch_mac_processes() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("sill-gui-mac-procs".into()).spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(3));
                for session in crate::mac_procs::sessions_running() {
                    let state: Result<sill_proto::host_control::State, _> =
                        sill_proto::Client::connect().and_then(|client| client.call("_sill/host_control.state", serde_json::json!({"session_id": session})));
                    // the daemon unreachable is not a revocation: only a clear answer stops them
                    if let Ok(state) = state
                        && (!state.active
                            || !state.operations.iter().any(|op| op == "mac_commands")
                            || !ticked(&session, state.generation, state.since.as_deref(), "mac_commands"))
                    {
                        crate::mac_procs::stop_session(&session);
                    }
                }
            }
        });
    });
}

#[cfg(test)]
mod grant_tests {
    #[test]
    fn the_mac_acts_only_on_what_the_person_switched_on_in_this_window() {
        let s = "ticked-test";
        let at = Some("1794560000");
        assert!(!super::ticked(s, 1, at, "mac_view"), "the daemon's word alone is not enough");
        super::person_asked(s, &["mac_view".into()]);
        assert!(!super::ticked(s, 1, at, "mac_view"), "not before the daemon answered");
        super::person_answered(s, Some((4, at.map(String::from))), false);
        assert!(super::ticked(s, 4, at, "mac_view"));
        assert!(!super::ticked(s, 4, at, "mac_commands"), "only what was switched on");
        assert!(!super::ticked(s, 5, at, "mac_view"), "an activation made elsewhere has another generation");
        assert!(!super::ticked(s, 4, Some("1794560999"), "mac_view"), "the same generation from a daemon started again is another activation");
        // a failed change leaves the grant; a revocation ends it
        super::person_asked(s, &["mac_view".into(), "mac_commands".into()]);
        super::person_answered(s, None, true);
        assert!(super::ticked(s, 4, at, "mac_view") && !super::ticked(s, 4, at, "mac_commands"));
        super::person_answered(s, None, false);
        assert!(!super::ticked(s, 4, at, "mac_view"));
        // the VM stopping forgets every grant
        super::person_asked(s, &["mac_view".into()]);
        super::person_answered(s, Some((6, at.map(String::from))), false);
        super::forget_grants();
        assert!(!super::ticked(s, 6, at, "mac_view"));
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn slow_sender_cannot_extend_the_request_deadline() {
        let (mut peer, stream) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || answer_mac(stream, "test-secret"));
        // Progress more often than the socket timeout still cannot extend the total budget.
        let sender = std::thread::spawn(move || {
            for _ in 0..80 {
                if peer.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(7));
        sender.join().unwrap();
    }

    #[test]
    fn oversized_request_is_rejected_before_parsing() {
        let (mut peer, stream) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || answer_mac(stream, "test-secret"));
        let _ = peer.write_all(&vec![b' '; MAC_REQUEST_MAX as usize + 1]);
        assert!(worker.join().unwrap().unwrap_err().contains("too large"));
    }
}
