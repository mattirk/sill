//! VTE consumes a local PTY. Its raw slave relays bytes to the PTY owned by the daemon host.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use sill_core::frontend::{Opened, Request};

pub struct PtyBridge {
    pub token: String,
    master: Option<OwnedFd>,
    interrupt: Arc<dyn Fn() + Send + Sync>,
    closed: Arc<AtomicBool>,
}

impl PtyBridge {
    pub fn open(session: Option<&str>, argv: &[String], env: &BTreeMap<String, String>, cwd: Option<&Path>, cols: u16, rows: u16) -> Result<Self> {
        let opened: Opened = super::remote::request(Request::OpenPty {
            session: session.map(str::to_string), argv: argv.into(), env: env.clone(), cwd: cwd.map(Path::to_path_buf), cols, rows,
        })?;
        let stream = sill_proto::transport::open_stream(&opened.token)?;
        Self::bridge(opened, stream, cols, rows)
    }

    fn bridge(opened: Opened, stream: sill_proto::transport::RawStream, cols: u16, rows: u16) -> Result<Self> {
        use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt};
        use rustix::termios::{OptionalActions, Winsize, tcgetattr, tcsetattr, tcsetwinsize};
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).context("local terminal bridge")?;
        grantpt(&master)?;
        unlockpt(&master)?;
        let slave = ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
        let mut attrs = tcgetattr(&slave)?;
        attrs.make_raw();
        tcsetattr(&slave, OptionalActions::Now, &attrs)?;
        tcsetwinsize(&slave, Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 })?;
        let mut local_read = std::fs::File::from(slave);
        let mut local_write = local_read.try_clone()?;
        let mut remote_read = stream.reader;
        let mut remote_write = stream.writer;
        let interrupt = stream.interrupt;
        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        let stop = interrupt.clone();
        std::thread::Builder::new().name("sill-terminal-input".into()).spawn(move || {
            use rustix::event::{poll, PollFd, PollFlags, Timespec};
            let timeout = Timespec { tv_sec: 0, tv_nsec: 200_000_000 };
            let mut bytes = [0u8; 32 * 1024];
            while !flag.load(Ordering::Acquire) {
                let ready = poll(&mut [PollFd::new(&local_read, PollFlags::IN)], Some(&timeout));
                match ready {
                    Ok(0) => continue,
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => break,
                    Ok(_) => {},
                }
                match local_read.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => if remote_write.write_all(&bytes[..n]).and_then(|()| remote_write.flush()).is_err() { break; },
                }
            }
            flag.store(true, Ordering::Release);
            stop();
        })?;
        let flag = closed.clone();
        let stop = interrupt.clone();
        std::thread::Builder::new().name("sill-terminal-output".into()).spawn(move || {
            let mut bytes = [0u8; 32 * 1024];
            loop {
                match remote_read.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => if local_write.write_all(&bytes[..n]).is_err() { break },
                }
            }
            flag.store(true, Ordering::Release);
            stop();
        })?;
        Ok(Self { token: opened.token, master: Some(master), interrupt, closed })
    }

    pub fn closed(&self) -> bool { self.closed.load(Ordering::Acquire) }

    pub fn take_fd(&mut self) -> OwnedFd { self.master.take().expect("a terminal bridge's fd is taken once") }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let _: serde_json::Value = super::remote::request(Request::ResizePty { token: self.token.clone(), cols, rows })?;
        Ok(())
    }

    pub fn cwd(&self) -> Result<Option<PathBuf>> { super::remote::request(Request::PtyCwd { token: self.token.clone() }) }
}

impl Drop for PtyBridge {
    fn drop(&mut self) { self.closed.store(true, Ordering::Release); (self.interrupt)(); }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    #[test]
    fn terminal_bridge_preserves_raw_bytes_and_closes_on_remote_eof() {
        let (pipe, mut remote) = UnixStream::pair().unwrap();
        remote.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let close = pipe.try_clone().unwrap();
        let stream = sill_proto::transport::RawStream {
            reader: Box::new(pipe.try_clone().unwrap()), writer: Box::new(pipe),
            interrupt: Arc::new(move || { let _ = close.shutdown(std::net::Shutdown::Both); }),
            shutdown_write: Arc::new(|| {}),
        };
        let mut bridge = PtyBridge::bridge(Opened { token: "test".into(), pid: 1 }, stream, 80, 24).unwrap();
        let mut terminal = std::fs::File::from(bridge.take_fd());
        let bytes = [b'\r', b'\n', 3, 0xff];
        terminal.write_all(&bytes).unwrap();
        let mut received = [0; 4];
        remote.read_exact(&mut received).unwrap();
        assert_eq!(received, bytes);
        remote.write_all(&bytes).unwrap();
        let timeout = Timespec { tv_sec: 2, tv_nsec: 0 };
        assert!(poll(&mut [PollFd::new(&terminal, PollFlags::IN)], Some(&timeout)).unwrap() > 0);
        terminal.read_exact(&mut received).unwrap();
        assert_eq!(received, bytes);
        drop(remote);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !bridge.closed() && std::time::Instant::now() < deadline { std::thread::sleep(std::time::Duration::from_millis(10)); }
        assert!(bridge.closed(), "remote EOF was not observed");
        let mut fds = [PollFd::new(&terminal, PollFlags::IN)];
        assert!(poll(&mut fds, Some(&timeout)).unwrap() > 0);
        assert!(fds[0].revents().contains(PollFlags::HUP), "local PTY slave was left alive");
    }
}
