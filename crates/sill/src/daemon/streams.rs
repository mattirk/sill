//! One-use byte streams. File descriptors remain on the daemon's host.
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use anyhow::{Context, Result, bail};
use sill_core::frontend::Opened;

enum Endpoint { Pty(File), Socket(UnixStream) }
struct Pending { endpoint: Endpoint, expires: Instant }
#[derive(Default)]
struct Streams {
    pending: HashMap<String, Pending>,
    ptys: HashMap<String, (Arc<File>, u32, bool)>,
}
fn state() -> &'static Mutex<Streams> { static S: OnceLock<Mutex<Streams>> = OnceLock::new(); S.get_or_init(Default::default) }

fn token() -> Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn add(endpoint: Endpoint, pty: Option<(File, u32, bool)>) -> Result<String> {
    let token = token()?;
    let mut s = state().lock().unwrap();
    let expired: Vec<String> = s.pending.iter().filter(|(_, p)| p.expires < Instant::now()).map(|(k, _)| k.clone()).collect();
    for k in expired { s.pending.remove(&k); s.ptys.remove(&k); }
    if let Some((fd, pid, host)) = pty { s.ptys.insert(token.clone(), (Arc::new(fd), pid, host)); }
    s.pending.insert(token.clone(), Pending { endpoint, expires: Instant::now() + Duration::from_secs(60) });
    let expires = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        let mut s = state().lock().unwrap();
        if s.pending.remove(&expires).is_some() { s.ptys.remove(&expires); }
    });
    Ok(token)
}

pub fn open_pty(session: Option<&str>, argv: &[String], env: &BTreeMap<String, String>, cwd: Option<&Path>, cols: u16, rows: u16) -> Result<Opened> {
    let defaults;
    let argv = if argv.is_empty() {
        defaults = vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()), "-l".into()];
        &defaults
    } else { argv };
    let (fd, pid) = if let Some(session) = session {
        let mut control = sill_core::shim::Control::connect(&sill_core::session::socket_path(session)?)?;
        let (fd, pid) = control.open_pty(argv, env, cwd, cols, rows)?;
        (File::from(fd), pid)
    } else {
        host_pty(argv, env, cwd, cols, rows)?
    };
    let token = add(Endpoint::Pty(fd.try_clone()?), Some((fd, pid, session.is_none())))?;
    Ok(Opened { token, pid })
}

fn host_pty(argv: &[String], env: &BTreeMap<String, String>, cwd: Option<&Path>, cols: u16, rows: u16) -> Result<(File, u32)> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use rustix::pty::OpenptFlags;
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    // NOCTTY: under systemd the daemon is a session leader without a terminal, and opening the
    // slave without it makes the shell's terminal the *daemon's* controlling terminal — the child's
    // TIOCSCTTY then takes it, and the hangup that follows kills the daemon with SIGHUP (no log, no
    // core: the host shell "lost the daemon connection", 2026-11-03). The shim and the shelf's
    // bridge open their peers the same way.
    let slave = std::fs::File::from(rustix::pty::ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?);
    size(&master, cols, rows)?;
    let prog = argv.first().context("empty terminal command")?;
    let mut command = Command::new(prog);
    command.args(&argv[1..]).envs(env).stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
    if let Some(cwd) = cwd { command.current_dir(cwd); }
    // SAFETY: only async-signal-safe syscalls are performed between fork and exec.
    unsafe { command.pre_exec(|| {
        rustix::process::setsid().map_err(std::io::Error::from)?;
        rustix::process::ioctl_tiocsctty(rustix::stdio::stdin()).map_err(std::io::Error::from)?;
        Ok(())
    }); }
    let mut child = command.spawn()?;
    let pid = child.id();
    std::thread::Builder::new().name(format!("sill-host-pty-{pid}")).spawn(move || { let _ = child.wait(); })?;
    Ok((File::from(master), pid))
}

fn size(fd: impl AsFd, cols: u16, rows: u16) -> Result<()> {
    rustix::termios::tcsetwinsize(fd, rustix::termios::Winsize { ws_col: cols.max(1), ws_row: rows.max(1), ws_xpixel: 0, ws_ypixel: 0 })?;
    Ok(())
}
pub fn resize(token: &str, cols: u16, rows: u16) -> Result<()> {
    let s = state().lock().unwrap();
    size(s.ptys.get(token).context("terminal stream is closed")?.0.as_ref(), cols, rows)
}
pub fn cwd(token: &str) -> Result<Option<PathBuf>> {
    let s = state().lock().unwrap();
    let (_, pid, host) = s.ptys.get(token).context("terminal stream is closed")?;
    if !host { return Ok(None) }
    Ok(std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
}
pub fn port(session: &str, port: u16) -> Result<String> {
    let socket = sill_core::shim::port_stream(&sill_core::session::socket_path(session)?, port)?;
    add(Endpoint::Socket(socket), None)
}

pub fn login(agent: sill_core::agents::Agent, home: PathBuf, method: String) -> Result<String> {
    let (reader, mut writer) = UnixStream::pair()?;
    let token = add(Endpoint::Socket(reader), None)?;
    std::thread::Builder::new().name(format!("sill-login-{}", agent.id)).spawn(move || {
        let result = sill_core::login::run(&agent, &home, &method, |line| {
            let value = match line {
                sill_core::login::Line::Status(text) => serde_json::json!({"status":text}),
                sill_core::login::Line::Stderr(text) => serde_json::json!({"stderr":text}),
            };
            let _ = writeln!(writer, "{value}");
        });
        let value = match result { Ok(()) => serde_json::json!({"done":true}), Err(e) => serde_json::json!({"error":format!("{e:#}")}) };
        let _ = writeln!(writer, "{value}");
    })?;
    Ok(token)
}

pub fn install(entry: sill_core::registry::Entry, home: PathBuf) -> Result<String> {
    let (reader, mut writer) = UnixStream::pair()?;
    let token = add(Endpoint::Socket(reader), None)?;
    std::thread::Builder::new().name(format!("sill-install-{}", entry.id)).spawn(move || {
        let result = sill_core::registry::install(&home, &entry, &mut |line| {
            let _ = writeln!(writer, "{}", serde_json::json!({"line":line}));
        });
        let result = match result {
            Ok(agent) => serde_json::json!({"agent":agent}),
            Err(error) => serde_json::json!({"error":format!("{error:#}")}),
        };
        let _ = writeln!(writer, "{result}");
    })?;
    Ok(token)
}

pub fn claim(token: &str, mut reader: BufReader<UnixStream>) -> Result<()> {
    let pending = state().lock().unwrap().pending.remove(token).context("unknown or already used stream token")?;
    if pending.expires < Instant::now() { state().lock().unwrap().ptys.remove(token); bail!("stream token expired"); }
    struct Cleanup(String);
    impl Drop for Cleanup { fn drop(&mut self) { state().lock().unwrap().ptys.remove(&self.0); } }
    let _cleanup = Cleanup(token.into());
    let mut writer = reader.get_ref().try_clone()?;
    let socket = reader.get_ref().try_clone()?;
    match pending.endpoint {
        Endpoint::Socket(mut endpoint) => {
            let mut input = endpoint.try_clone()?;
            let copy = std::thread::spawn(move || { let _ = std::io::copy(&mut reader, &mut input); let _ = input.shutdown(std::net::Shutdown::Write); });
            let _ = std::io::copy(&mut endpoint, &mut writer);
            let _ = socket.shutdown(std::net::Shutdown::Both);
            let _ = copy.join();
        },
        Endpoint::Pty(mut endpoint) => {
            use std::sync::atomic::{AtomicBool, Ordering};
            let stop = Arc::new(AtomicBool::new(false));
            let done = stop.clone();
            let mut input = endpoint.try_clone()?;
            let copy = std::thread::spawn(move || { let _ = std::io::copy(&mut reader, &mut input); done.store(true, Ordering::Release); });
            let mut bytes = [0u8; 16384];
            while !stop.load(Ordering::Acquire) {
                let mut fds = [rustix::event::PollFd::new(&endpoint, rustix::event::PollFlags::IN)];
                let timeout = rustix::event::Timespec { tv_sec: 0, tv_nsec: 100_000_000 };
                match rustix::event::poll(&mut fds, Some(&timeout)) { Ok(0) => continue, Ok(_) => {}, Err(_) => break }
                match endpoint.read(&mut bytes) { Ok(0) | Err(_) => break, Ok(n) => if writer.write_all(&bytes[..n]).is_err() { break } }
            }
            let _ = socket.shutdown(std::net::Shutdown::Both);
            let _ = copy.join();
        },
    }
    Ok(())
}
