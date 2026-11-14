//! Exposed ports (docs/tasks.md §2.5): a managed process listening on a port inside the jail,
//! reachable from the person's browser on the host's loopback. The shelf listens on
//! `127.0.0.1:<port>` and relays every connection through the daemon
//! byte stream, which connects to `127.0.0.1:<port>` inside — only for a port a running managed process
//! declared. Nothing new is reachable from inside the jail; the bridge lives as long as the
//! [`Bridge`] value (the view drops it when the process ends).

use std::net::TcpListener;
use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

pub struct Bridge {
    /// Where it is on the host: the jail's number when free, else one the kernel picked.
    pub host_port: u16,
    stop: Arc<AtomicBool>,
    active: Arc<Mutex<Vec<Weak<Connection>>>>,
}

struct Connection {
    socket: std::net::TcpStream,
    interrupt: Arc<dyn Fn() + Send + Sync>,
}
impl Connection {
    fn close(&self) { let _ = self.socket.shutdown(std::net::Shutdown::Both); (self.interrupt)(); }
}

impl Bridge {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for connection in self.active.lock().unwrap().drain(..).filter_map(|c| c.upgrade()) { connection.close(); }
    }
}

/// Listen on this frontend's loopback for a managed port on the selected box.
pub fn expose(session: String, jail_port: u16) -> Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", jail_port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .context("listen on the host's loopback")?;
    let host_port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let active: Arc<Mutex<Vec<Weak<Connection>>>> = Arc::default();
    let connections = active.clone();
    std::thread::Builder::new()
        .name(format!("sill-port-{jail_port}"))
        .spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((conn, _)) => {
                        let session = session.clone();
                        let flag = flag.clone();
                        let connections = connections.clone();
                        std::thread::spawn(move || {
                            let _ = conn.set_nonblocking(false);
                            relay(conn, &session, jail_port, &flag, &connections);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(100))
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok(Bridge { host_port, stop, active })
}

/// One browser connection ⇄ the shim ⇄ the process inside.
fn relay(host: std::net::TcpStream, session: &str, port: u16, stopped: &AtomicBool, active: &Mutex<Vec<Weak<Connection>>>) {
    let token: Result<String> = super::remote::request(sill_core::frontend::Request::OpenPort { session: session.into(), port });
    let Ok(token) = token else { return };
    let Ok(stream) = sill_proto::transport::open_stream(&token) else { return };
    let Ok(mut host_r) = host.try_clone() else { (stream.interrupt)(); return };
    let Ok(socket) = host.try_clone() else { (stream.interrupt)(); return };
    let connection = Arc::new(Connection { socket, interrupt: stream.interrupt.clone() });
    {
        let mut all = active.lock().unwrap();
        all.retain(|c| c.strong_count() > 0);
        all.push(Arc::downgrade(&connection));
    }
    if stopped.load(Ordering::Acquire) { connection.close(); return; }
    let mut host_w = host;
    let mut remote_r = stream.reader;
    let mut remote_w = stream.writer;
    let half_close = stream.shutdown_write;
    let input = std::thread::spawn(move || {
        let _ = std::io::copy(&mut host_r, &mut remote_w);
        half_close();
    });
    let _ = std::io::copy(&mut remote_r, &mut host_w);
    connection.close();
    let _ = input.join();
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
