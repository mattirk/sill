//! Terminals the daemon owns (docs/protocol.md, terminals). The daemon holds each PTY master and a
//! reader that keeps the last [`api::REPLAY_BYTES`] of output; a frontend attaches through a byte
//! stream, gets that output first and then the live bytes. Closing the stream only detaches: the
//! shell lives until it exits, its jail stops, `terminals.close` hangs it up or the daemon leaves.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sill_proto::rpc::Error;
use sill_proto::sessions::HostPath;
use sill_proto::terminals as api;

/// Output chunks queued for one attached stream. A viewer this far behind is cut off and
/// attaches again (its replay covers what it missed, up to the replay size).
const VIEWER_QUEUE: usize = 256;

struct Viewer {
    id: u64,
    tx: SyncSender<Arc<[u8]>>,
}
struct Terminal {
    info: api::Terminal,
    discarded: u64,
    master: Arc<File>,
    ring: VecDeque<u8>,
    viewers: Vec<Viewer>,
    exited_at: Option<Instant>,
    closing: Arc<AtomicBool>,
}
fn table() -> &'static Mutex<HashMap<String, Terminal>> {
    static T: OnceLock<Mutex<HashMap<String, Terminal>>> = OnceLock::new();
    T.get_or_init(Default::default)
}
fn next_viewer() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn domain(code: &'static str, message: &str) -> anyhow::Error {
    sill_core::sfp::DomainError { code, message: message.into(), details: None }.into()
}
fn not_found() -> anyhow::Error {
    domain("not_found", "terminal not found")
}
fn invalid(message: &str) -> anyhow::Error {
    Error::invalid_params(message).into()
}

/// Forget exited terminals nobody has read for [`api::KEEP_EXITED_SECONDS`].
pub fn reap() {
    let keep = Duration::from_secs(api::KEEP_EXITED_SECONDS);
    table().lock().unwrap().retain(|_, t| t.exited_at.is_none_or(|at| at.elapsed() < keep));
}

pub fn open(a: api::Open) -> Result<api::Terminal> {
    if a.label.len() > 128 || a.label.chars().any(char::is_control) {
        return Err(invalid("label must be at most 128 bytes without control characters"));
    }
    if a.argv.len() > 256 || a.env.len() > 256 {
        return Err(invalid("at most 256 arguments and 256 environment variables"));
    }
    let cwd = a.cwd.as_ref().map(|c| c.to_path().map_err(|e| invalid(&format!("cwd: {e}")))).transpose()?;
    reap();
    if table().lock().unwrap().len() >= api::MAX_TERMINALS {
        return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Capacity, None, "too many terminals; close one first"));
    }
    let defaults;
    let argv = if a.argv.is_empty() {
        defaults = vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()), "-l".into()];
        &defaults
    } else {
        &a.argv
    };
    let id = super::sfp::id();
    let (master, pid) = match &a.session_id {
        Some(session) => {
            let mut control = sill_core::shim::Control::connect(&sill_core::session::socket_path(session)?)?;
            let (fd, pid) = control.open_pty(argv, &a.env, cwd.as_deref(), a.cols, a.rows)?;
            (File::from(fd), pid)
        }
        None => host_pty(&id, argv, &a.env, cwd.as_deref(), a.cols, a.rows)?,
    };
    let info = api::Terminal {
        terminal_id: id.clone(),
        session_id: a.session_id.clone(),
        label: a.label,
        pid,
        cols: a.cols.max(1),
        rows: a.rows.max(1),
        state: "running".into(),
        exit_code: None,
        created_at: sill_core::store::now().to_string(),
        discarded_bytes: "0".into(),
    };
    let master = Arc::new(master);
    let closing = Arc::new(AtomicBool::new(false));
    table().lock().unwrap().insert(
        id.clone(),
        Terminal {
            info: info.clone(),
            discarded: 0,
            master: master.clone(),
            ring: VecDeque::new(),
            viewers: vec![],
            exited_at: None,
            closing: closing.clone(),
        },
    );
    std::thread::Builder::new().name(format!("sill-term-{}", &id[..8])).spawn(move || read_output(id, master, closing))?;
    // who opened what: a shell in a jail and one on the host look the same from inside the jail
    eprintln!(
        "sill daemon: terminal opened {} (pid {pid}, {})",
        match &a.session_id {
            Some(s) => format!("in session {s}"),
            None => "on the host".into(),
        },
        argv[0]
    );
    Ok(info)
}

/// The terminal's one reader: into the replay ring and every attached stream, until the PTY ends.
fn read_output(id: String, master: Arc<File>, closing: Arc<AtomicBool>) {
    let mut bytes = [0u8; 16384];
    while !closing.load(Ordering::Acquire) {
        let mut fds = [rustix::event::PollFd::new(&*master, rustix::event::PollFlags::IN)];
        let timeout = rustix::event::Timespec { tv_sec: 0, tv_nsec: 200_000_000 };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(_) => break,
        }
        let n = match (&*master).read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut all = table().lock().unwrap();
        let Some(t) = all.get_mut(&id) else { return };
        t.ring.extend(&bytes[..n]);
        if t.ring.len() > api::REPLAY_BYTES {
            let mut cut = t.ring.len() - api::REPLAY_BYTES;
            // start the replay on a line, not inside an escape sequence or a UTF-8 character
            if let Some(nl) = t.ring.iter().skip(cut).take(4096).position(|&b| b == b'\n') {
                cut += nl + 1;
            }
            t.ring.drain(..cut);
            t.discarded += cut as u64;
            t.info.discarded_bytes = t.discarded.to_string();
        }
        let chunk: Arc<[u8]> = Arc::from(&bytes[..n]);
        t.viewers.retain(|v| match v.tx.try_send(chunk.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                eprintln!("sill daemon: terminal {} viewer fell behind; cut off", &id[..8]);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
    let mut all = table().lock().unwrap();
    if let Some(t) = all.get_mut(&id) {
        t.info.state = "exited".into();
        t.exited_at = Some(Instant::now());
        t.viewers.clear();
    }
}

fn host_pty(id: &str, argv: &[String], env: &BTreeMap<String, String>, cwd: Option<&Path>, cols: u16, rows: u16) -> Result<(File, u32)> {
    use rustix::pty::OpenptFlags;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    // NOCTTY: under systemd the daemon is a session leader without a terminal, and opening the
    // slave without it makes the shell's terminal the *daemon's* controlling terminal — the child's
    // TIOCSCTTY then takes it, and the hangup that follows kills the daemon with SIGHUP (no log, no
    // core: the host shell "lost the daemon connection", 2026-11-03). The shim opens its
    // peers the same way.
    let slave = File::from(rustix::pty::ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?);
    size(&master, cols, rows)?;
    let prog = argv.first().context("empty terminal command")?;
    let mut command = Command::new(prog);
    command.args(&argv[1..]).envs(env).stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // SAFETY: only async-signal-safe syscalls are performed between fork and exec.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid().map_err(std::io::Error::from)?;
            rustix::process::ioctl_tiocsctty(rustix::stdio::stdin()).map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    let id = id.to_string();
    std::thread::Builder::new().name(format!("sill-host-pty-{pid}")).spawn(move || {
        let st = child.wait();
        eprintln!("sill daemon: host terminal pid {pid} ended ({})", st.as_ref().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()));
        if let Some(t) = table().lock().unwrap().get_mut(&id) {
            t.info.exit_code = st.ok().and_then(|s| s.code());
        }
    })?;
    Ok((File::from(master), pid))
}

fn size(fd: impl AsFd, cols: u16, rows: u16) -> Result<()> {
    rustix::termios::tcsetwinsize(fd, rustix::termios::Winsize { ws_col: cols.max(1), ws_row: rows.max(1), ws_xpixel: 0, ws_ypixel: 0 })?;
    Ok(())
}

/// Serve one claimed attach stream: the replay and then live output out, keystrokes in. Ends
/// when the viewer closes, falls behind, or the terminal ends or is closed.
pub fn serve(id: &str, mut reader: BufReader<UnixStream>) -> Result<()> {
    let socket = reader.get_ref().try_clone()?;
    let mut out = socket.try_clone()?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Arc<[u8]>>(VIEWER_QUEUE);
    let viewer = next_viewer();
    let master = {
        let mut all = table().lock().unwrap();
        let t = all.get_mut(id).ok_or_else(not_found)?;
        // the replay and the subscription under one lock: no byte is missed or doubled between them
        let replay: Vec<u8> = t.ring.iter().copied().collect();
        if !replay.is_empty() {
            let _ = tx.try_send(Arc::from(replay));
        }
        if t.exited_at.is_none() {
            if t.viewers.len() >= api::MAX_VIEWERS {
                return Err(sill_core::sfp::DomainError::limit(sill_proto::rpc::LimitScope::Capacity, None, "too many viewers on this terminal"));
            }
            t.viewers.push(Viewer { id: viewer, tx });
        }
        t.master.clone()
    };
    let writer = std::thread::Builder::new().name("sill-term-out".into()).spawn(move || {
        for chunk in rx {
            if out.write_all(&chunk).is_err() {
                break;
            }
        }
        // the terminal ended, closed or cut this viewer off: end the stream both ways
        let _ = out.shutdown(std::net::Shutdown::Both);
    })?;
    let mut bytes = [0u8; 16384];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if (&*master).write_all(&bytes[..n]).is_err() {
                    break;
                }
            }
        }
    }
    if let Some(t) = table().lock().unwrap().get_mut(id) {
        t.viewers.retain(|v| v.id != viewer);
    }
    let _ = socket.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
    Ok(())
}

fn info(id: &str) -> Result<api::Terminal> {
    table().lock().unwrap().get(id).map(|t| t.info.clone()).ok_or_else(not_found)
}

pub fn list(session: Option<&str>) -> api::Terminals {
    reap();
    let all = table().lock().unwrap();
    let mut terminals: Vec<api::Terminal> =
        all.values().filter(|t| session.is_none_or(|s| t.info.session_id.as_deref() == Some(s))).map(|t| t.info.clone()).collect();
    terminals.sort_by(|a, b| (&a.created_at, &a.terminal_id).cmp(&(&b.created_at, &b.terminal_id)));
    api::Terminals { terminals }
}

pub fn resize(a: api::Resize) -> Result<()> {
    let mut all = table().lock().unwrap();
    let t = all.get_mut(&a.terminal_id).ok_or_else(not_found)?;
    if t.exited_at.is_some() {
        return Ok(());
    }
    size(&*t.master, a.cols, a.rows)?;
    (t.info.cols, t.info.rows) = (a.cols.max(1), a.rows.max(1));
    Ok(())
}

pub fn cwd(id: &str) -> Result<api::Cwd> {
    let t = info(id)?;
    if t.session_id.is_some() || t.state != "running" {
        return Ok(api::Cwd { cwd: None });
    }
    Ok(api::Cwd { cwd: std::fs::read_link(format!("/proc/{}/cwd", t.pid)).ok().map(|p| HostPath::from_path(&p)) })
}

/// Hang up: drop the daemon's master (the reader drops its own within its poll interval), which
/// sends SIGHUP to the terminal's session; attached streams end.
pub fn close(id: &str) -> Result<()> {
    let t = table().lock().unwrap().remove(id).ok_or_else(not_found)?;
    t.closing.store(true, Ordering::Release);
    Ok(())
}

/// A deleted session's terminals: its jail is gone, and so is any reason to show them.
pub fn forget_session(session: &str) {
    table().lock().unwrap().retain(|_, t| {
        let keep = t.info.session_id.as_deref() != Some(session);
        if !keep {
            t.closing.store(true, Ordering::Release);
        }
        keep
    });
}

pub fn handle(method: &str, params: Value) -> Option<Result<Value>> {
    fn args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
        serde_json::from_value(v).map_err(|e| invalid(&e.to_string()))
    }
    let key = |params: Value| args::<api::TerminalKey>(params).map(|k| k.terminal_id);
    Some(match method {
        "_sill/terminals.open" => (|| {
            let a: api::Open = args(params)?;
            if let Some(s) = &a.session_id
                && !sill_core::session::valid_id(s)
            {
                return Err(invalid("invalid session id"));
            }
            Ok(json!(open(a)?))
        })(),
        "_sill/terminals.list" => args::<api::List>(params).map(|a| json!(list(a.session_id.as_deref()))),
        "_sill/terminals.get" => key(params).and_then(|id| info(&id)).map(|t| json!(t)),
        "_sill/terminals.attach" => key(params).and_then(|id| {
            info(&id)?;
            Ok(json!(api::Stream { token: super::streams::terminal(&id)? }))
        }),
        "_sill/terminals.resize" => args::<api::Resize>(params).and_then(resize).map(|()| json!({})),
        "_sill/terminals.cwd" => key(params).and_then(|id| cwd(&id)).map(|c| json!(c)),
        "_sill/terminals.close" => key(params).and_then(|id| close(&id)).map(|()| json!({})),
        _ => return None,
    })
}
