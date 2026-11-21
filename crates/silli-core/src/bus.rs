//! The private D-Bus session bus of Desktop development (docs/permission-modes.md): a
//! `dbus-daemon` of the jail's own, started inside the jail by the shim (`silli-shim --bus`)
//! before the agent, listening in the jail's runtime directory. Programs that need a bus get
//! one; services they activate run as jailed programs; the person's session and system buses
//! stay out of reach (their addresses are dead, their sockets never bound). This module knows
//! whether the machine has `dbus-daemon` and where the bus listens; the shim runs it.
use std::path::PathBuf;
use std::sync::OnceLock;

/// The socket's name in the jail's runtime directory: where `sd-bus` and GLib look for a user
/// bus even without the address variable.
pub const SOCKET: &str = "bus";

/// The bus socket inside the jail.
pub fn socket_path(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/{SOCKET}"))
}

/// The bus address for the jail's environment.
pub fn address(uid: u32) -> String {
    format!("unix:path={}", socket_path(uid).display())
}

/// What this machine offers for a private bus. Probed once per process.
#[derive(Debug, Clone)]
pub struct Probe {
    pub daemon: Option<PathBuf>,
    /// Why there is no private bus; empty when there is one.
    pub missing: String,
}

impl Probe {
    pub fn available(&self) -> bool {
        self.missing.is_empty()
    }
}

fn probe_now() -> Probe {
    let daemon = crate::desktop::find("dbus-daemon");
    let missing = match &daemon {
        Some(_) => String::new(),
        None => "dbus-daemon is not installed (the private bus needs the dbus package)".to_string(),
    };
    Probe { daemon, missing }
}

pub fn probe() -> &'static Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    PROBE.get_or_init(probe_now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bus_listens_in_the_jail_s_runtime_directory() {
        assert_eq!(socket_path(1000), PathBuf::from("/run/user/1000/bus"));
        assert_eq!(address(1000), "unix:path=/run/user/1000/bus");
    }
}
