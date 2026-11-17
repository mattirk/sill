//! The private D-Bus session bus of Desktop development (docs/permission-modes.md): a
//! `dbus-daemon` of this jail's own, started here before the agent on the socket the launcher
//! named (`--bus PATH`, the same path the jail's `DBUS_SESSION_BUS_ADDRESS` gives). It runs
//! as a jailed program like everything else here: the services it activates (the standard
//! service directories, so installed desktop services and the agent's own `~/.local/share`
//! ones) are jailed too, and it ends with the jail. The person's buses are never involved:
//! the jail has no socket of theirs and dead addresses for both.
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// How long dbus-daemon may take to listen.
const READY: Duration = Duration::from_secs(10);

/// The bus configuration: the session defaults (`/usr/share/dbus-1/session.conf`) without the
/// host's local includes, listening on one socket. Written next to the socket.
fn config(socket: &Path) -> String {
    format!(
        "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\"\n \
         \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
         <!-- sill: the private session bus of one jail -->\n\
         <busconfig>\n  \
           <type>session</type>\n  \
           <keep_umask/>\n  \
           <listen>unix:path={}</listen>\n  \
           <auth>EXTERNAL</auth>\n  \
           <standard_session_servicedirs/>\n  \
           <policy context=\"default\">\n    \
             <allow send_destination=\"*\" eavesdrop=\"true\"/>\n    \
             <allow eavesdrop=\"true\"/>\n    \
             <allow own=\"*\"/>\n  \
           </policy>\n  \
           <limit name=\"service_start_timeout\">30000</limit>\n\
         </busconfig>\n",
        socket.display()
    )
}

/// Start the bus on `socket` and return once it answers; an error when it cannot.
pub fn start(socket: &Path) -> Result<()> {
    let dir = socket.parent().context("the bus socket needs a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let _ = std::fs::remove_file(socket);
    let conf = dir.join("sill-bus.conf");
    std::fs::File::create(&conf).and_then(|mut f| f.write_all(config(socket).as_bytes())).with_context(|| format!("writing {}", conf.display()))?;
    let mut cmd = Command::new("dbus-daemon");
    cmd.arg("--nofork").arg("--nopidfile").arg(format!("--config-file={}", conf.display())).stdin(Stdio::null()).stdout(Stdio::null());
    // the agent's own service files live under its HOME; dbus-daemon looks under XDG_DATA_HOME
    // or, failing that, the passwd home, which is not this HOME in every setup
    if std::env::var_os("XDG_DATA_HOME").is_none() {
        if let Some(home) = std::env::var_os("HOME") {
            cmd.env("XDG_DATA_HOME", Path::new(&home).join(".local/share"));
        }
    }
    let mut child = cmd.spawn().context("starting dbus-daemon for the private bus")?;
    let deadline = Instant::now() + READY;
    loop {
        if UnixStream::connect(socket).is_ok() {
            break;
        }
        if let Some(status) = child.try_wait()? {
            bail!("dbus-daemon for the private bus exited at start ({status})");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!("dbus-daemon for the private bus did not listen within {} s", READY.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let pid = child.id();
    crate::activity::bus_started(pid);
    eprintln!("sill-shim: private bus: dbus-daemon pid {pid} on {}", socket.display());
    std::thread::spawn(move || match child.wait() {
        Ok(status) => eprintln!("sill-shim: private bus: dbus-daemon pid {pid} ended ({status})"),
        Err(e) => eprintln!("sill-shim: private bus: waiting for dbus-daemon: {e}"),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_listens_on_the_named_socket_only() {
        let c = config(Path::new("/run/user/1000/bus"));
        assert!(c.contains("<listen>unix:path=/run/user/1000/bus</listen>"), "{c}");
        assert_eq!(c.matches("<listen>").count(), 1);
        assert!(c.contains("<standard_session_servicedirs/>"), "services activate, jailed");
        assert!(!c.contains("<include"), "none of the host's local bus configuration");
    }
}
