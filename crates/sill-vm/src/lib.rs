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

/// The virtiofs tag the person's Mac folders reach the guest under (`Vm::share`).
pub const SHARE_TAG: &str = "sill-mac";

/// A folder of this Mac shared with the guest, by `name` under the share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    pub name: String,
    pub path: PathBuf,
    pub read_only: bool,
}

/// The guest's SSH on vsock (systemd's ssh generator binds it when the VM has a vsock device).
pub const SSH_PORT: u32 = 22;

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
        Self {
            dir,
            cpus: cpus.clamp(2, 8),
            memory_gb: memory_gb.clamp(2, 8),
        }
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
