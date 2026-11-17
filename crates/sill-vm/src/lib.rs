//! sill's local Linux VM: the box a Mac runs its daemon in (docs/sill-stick.md section 9).
//!
//! A VM directory holds `system.raw` (the image), `home.raw` and `state.raw` (the data disks,
//! `/dev/disk/by-id/virtio-sill-home` and `virtio-sill-state` in the guest), `seed.iso`
//! (cloud-init: the account's key), and what the VM keeps of itself: its EFI variables, its
//! identity, its network address and `console.log`. The VM runs in this process — it ends with it — and is reached
//! over the framework's host–guest socket (vsock), never the guest's network: `Vm::connect` for
//! one port, `serve_ssh` for OpenSSH (`ProxyCommand nc -U <socket>`).
use std::path::PathBuf;

mod requirements;
pub use requirements::{Problem, check};

#[cfg(target_os = "macos")]
pub mod setup;

#[cfg(target_os = "macos")]
mod apple;
#[cfg(target_os = "macos")]
pub use apple::{Vm, serve_ssh};

/// How many folders of the Mac the guest can have at once: one virtiofs device each, made with
/// the VM (`Vm::share_slot`). Each folder has a device of its own because replacing a device's
/// share leaves everything mounted from it stale in the guest (seen 2026-11-12): giving one
/// folder must not take another away from running jails.
pub const SHARE_SLOTS: usize = 16;

/// The virtiofs tag of share slot `slot`.
pub fn share_tag(slot: usize) -> String {
    format!("sill-mac-{slot}")
}

/// The guest's SSH on vsock (systemd's ssh generator binds it when the VM has a vsock device).
pub const SSH_PORT: u32 = 22;

/// The memory of this computer, in bytes (macOS), for choosing the VM's share of it.
pub fn host_memory_bytes() -> Option<u64> {
    requirements::memory_bytes()
}

/// What a VM is made of.
#[derive(Debug, Clone)]
pub struct Config {
    pub dir: PathBuf,
    pub cpus: usize,
    pub memory_gb: u64,
}

impl Config {
    /// Half the Mac's processors and a quarter of its memory, within 2–8 CPUs and 2–8 GB
    /// (docs/sill-stick.md: an 8 GB Mac gives the guest about 3–4 GB at most).
    pub fn sized_for_this_mac(dir: PathBuf) -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get() / 2);
        let memory_gb = requirements::memory_bytes().map_or(2, |bytes| bytes / 4 / (1 << 30));
        Self { dir, cpus: cpus.clamp(2, 8), memory_gb: memory_gb.clamp(2, 8) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    Starting,
    Running,
    Stopping,
    /// Paused, resuming, saving or restoring: not something sill does, but the framework has it.
    Other,
    Error,
}
