//! Exposed ports (docs/tasks.md §2.5): a managed process listening on a port inside the jail,
//! reachable from the person's browser on the host's loopback. The shelf listens on
//! `127.0.0.1:<port>` and relays every connection through the shim (`{"role":"port"}` on the control
//! socket), which connects to `127.0.0.1:<port>` inside — only for a port a running managed process
//! declared. Nothing new is reachable from inside the jail; the bridge lives as long as the
//! [`Bridge`] value (the view drops it when the process ends).

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

pub struct Bridge {
    /// Where it is on the host: the jail's number when free, else one the kernel picked.
    pub host_port: u16,
    stop: Arc<AtomicBool>,
}

impl Bridge {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Listen on the host's loopback for `jail_port` of the session whose shim socket is `socket`.
pub fn expose(socket: PathBuf, jail_port: u16) -> Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", jail_port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .context("listen on the host's loopback")?;
    let host_port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name(format!("sill-port-{jail_port}"))
        .spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let socket = socket.clone();
                        std::thread::spawn(move || {
                            let _ = conn.set_nonblocking(false);
                            relay(conn, &socket, jail_port);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(100)),
                    Err(_) => break,
                }
            }
        })?;
    Ok(Bridge { host_port, stop })
}

/// One browser connection ⇄ the shim ⇄ the process inside.
fn relay(host: std::net::TcpStream, socket: &std::path::Path, port: u16) {
    let Ok(jail) = sill_core::shim::port_stream(socket, port) else { return };
    let (Ok(mut host_r), Ok(mut jail_r)) = (host.try_clone(), jail.try_clone()) else { return };
    let (mut host_w, mut jail_w) = (host, jail);
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut host_r, &mut jail_w);
        let _ = jail_w.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut jail_r, &mut host_w);
    let _ = host_w.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
}

/// `3m12s`, `1h04m`, `2d`.
pub fn uptime(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn uptime_reads_naturally() {
        assert_eq!(super::uptime(42), "42s");
        assert_eq!(super::uptime(192), "3m12s");
        assert_eq!(super::uptime(3840), "1h04m");
        assert_eq!(super::uptime(200_000), "2d");
    }
}
