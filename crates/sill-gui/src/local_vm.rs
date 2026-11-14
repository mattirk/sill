//! The Mac window's own Linux VM, the box its daemon runs in (docs/sill-stick.md section 9).
//!
//! Without `SILL_BOX`, sill-gui on the Mac starts the VM in this process at launch — on a thread
//! of its own, the window showing how far it is (`phase`) — and talks to its daemon as to any box,
//! over SSH through the VM's local socket. The VM ends with the process; Quit asks the guest to
//! shut down first (`shut_down`).
use std::path::PathBuf;
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
