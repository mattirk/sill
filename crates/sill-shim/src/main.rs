//! sill-shim: the supervisor inside the jail (SPEC §4.3).
//!
//! Started by bwrap as the jail's first process with the control socket inherited as a file
//! descriptor (`--control-fd N`; `--listen PATH` for development). It owns everything that lives in the
//! jail: the agent process and its stdio, later the ptys. The shelf (or the CLI) connects to the socket;
//! each connection starts with one JSON "hello" line naming its role:
//!
//! - `{"role":"control"}` — newline-delimited JSON requests, one reply line each (see [`Request`]);
//! - `{"role":"agent"}`   — after the hello the connection is the agent's ACP byte stream: lines from
//!   the socket go to the agent's stdin, the agent's stdout lines go to the socket. Only one agent
//!   connection at a time; a new one replaces the old. While no one is connected the agent's output
//!   is buffered (bounded; the agent blocks on stdout when the buffer is full), so the shelf can go
//!   away and come back without the agent noticing.
//!
//! Plain std threads: the shim has nothing to do but wait on file descriptors.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The real tree, read-only, as mounted by the jail builder (sill-core `jail::HOST_VIEW`).
const HOST_VIEW: &str = "/.sill/host";
/// Buffered agent output while no shelf is connected.
const OUTBOX_LIMIT: usize = 64 << 20;

#[derive(Deserialize)]
struct Hello {
    role: String,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    /// Start the agent process. Fails if one is running.
    Spawn { argv: Vec<String>, #[serde(default)] env: BTreeMap<String, String>, cwd: Option<PathBuf> },
    Status,
    /// SIGTERM the agent's process group, SIGKILL after `grace_ms`.
    Kill { #[serde(default = "default_grace")] grace_ms: u64 },
    /// Restore a path from the host view (SPEC §4.4 revert).
    Revert { path: PathBuf },
    /// Read a file as the jail sees it (merged view).
    ReadFile { path: PathBuf },
    /// Write a file in the jail (merged view, recorded by the overlay).
    WriteFile { path: PathBuf, content: String },
    /// Open a pty inside the jail and run `argv` on it (the person's shell, SPEC §3.1). The reply
    /// line carries the pty master as an SCM_RIGHTS fd, so the client must read it with recvmsg.
    OpenPty {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default = "default_rows")]
        rows: u16,
    },
}

fn default_cols() -> u16 {
    100
}
fn default_rows() -> u16 {
    30
}

fn default_grace() -> u64 {
    5000
}

#[derive(Serialize)]
struct Reply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    data: serde_json::Map<String, serde_json::Value>,
}

impl Reply {
    fn ok() -> Self {
        Reply { ok: true, error: None, data: Default::default() }
    }
    fn with(mut self, k: &str, v: impl Into<serde_json::Value>) -> Self {
        self.data.insert(k.into(), v.into());
        self
    }
    fn err(e: impl std::fmt::Display) -> Self {
        Reply { ok: false, error: Some(e.to_string()), data: Default::default() }
    }
}

#[derive(Default)]
struct State {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    exit: Option<i32>,
    outbox: VecDeque<String>,
    outbox_bytes: usize,
    /// Bumped for every new agent connection so an old relay thread knows it was replaced.
    relay_generation: u64,
    /// A shelf is attached to the agent stream.
    relay_connected: bool,
    /// Last agent stdio traffic or shelf (dis)connect.
    last_activity: Option<std::time::Instant>,
}

impl State {
    fn touch(&mut self) {
        self.last_activity = Some(std::time::Instant::now());
    }
}

type Shared = Arc<(Mutex<State>, Condvar)>;

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // --forward 127.0.0.1:3128=/.sill/proxy.sock : TCP → Unix forwarder for the session proxy (SPEC §4.8)
    while let Some(i) = args.iter().position(|a| a == "--forward") {
        if i + 1 >= args.len() {
            bail!("--forward expects ADDR=PATH");
        }
        let spec = args.remove(i + 1);
        args.remove(i);
        let (addr, path) = spec.split_once('=').context("--forward expects ADDR=PATH")?;
        let (addr, path) = (addr.to_string(), PathBuf::from(path));
        std::thread::spawn(move || {
            if let Err(e) = forward_tcp_to_unix(&addr, path) {
                eprintln!("sill-shim: forwarder: {e:#}");
            }
        });
    }
    // --agent-idle SECS : stop the agent process after SECS without traffic and without a shelf attached
    let mut agent_idle: Option<u64> = None;
    while let Some(i) = args.iter().position(|a| a == "--agent-idle") {
        if i + 1 >= args.len() {
            bail!("--agent-idle expects seconds");
        }
        agent_idle = Some(args.remove(i + 1).parse().context("--agent-idle expects seconds")?);
        args.remove(i);
    }
    let listener = match args.as_slice() {
        [flag, fd] if flag == "--control-fd" => {
            let fd: i32 = fd.parse().context("--control-fd expects a number")?;
            // SAFETY: the launcher put a listening Unix socket on this fd before exec.
            let l = unsafe { UnixListener::from_raw_fd(fd) };
            // the launcher cleared close-on-exec so the fd would survive *our* exec; from here on it
            // must not survive anyone else's: the agent or a shell could accept() the shelf's
            // connections and answer as the shim (SPEC §4.11 #2)
            rustix::io::fcntl_setfd(&l, rustix::io::FdFlags::CLOEXEC).context("control fd cloexec")?;
            l
        }
        [flag, path] if flag == "--listen" => {
            let _ = std::fs::remove_file(path);
            UnixListener::bind(path).with_context(|| format!("listen on {path}"))?
        }
        _ => bail!("usage: sill-shim --control-fd N | --listen PATH"),
    };
    eprintln!("sill-shim: ready (pid {})", std::process::id());

    let shared: Shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
    if let Some(secs) = agent_idle.filter(|s| *s > 0) {
        let shared = shared.clone();
        std::thread::spawn(move || idle_monitor(shared, std::time::Duration::from_secs(secs)));
    }
    let mut accept_errors = 0;
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => {
                accept_errors = 0;
                c
            }
            Err(e) => {
                accept_errors += 1;
                eprintln!("sill-shim: accept: {e}");
                if accept_errors >= 10 || e.raw_os_error() == Some(9) {
                    bail!("control socket unusable, giving up");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(conn, shared) {
                eprintln!("sill-shim: connection: {e:#}");
            }
        });
    }
    Ok(())
}

fn handle_connection(conn: UnixStream, shared: Shared) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut hello = String::new();
    reader.read_line(&mut hello)?;
    let hello: Hello = serde_json::from_str(hello.trim()).context("hello line")?;
    match hello.role.as_str() {
        "control" => control_loop(reader, conn, shared),
        "agent" => agent_relay(reader, conn, shared),
        other => bail!("unknown role {other}"),
    }
}

// --- control -------------------------------------------------------------------------------------

fn control_loop(reader: BufReader<UnixStream>, mut writer: UnixStream, shared: Shared) -> Result<()> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req = match serde_json::from_str::<Request>(&line) {
            Ok(req) => req,
            Err(e) => {
                let mut out = serde_json::to_string(&Reply::err(format!("bad request: {e}")))?;
                out.push('\n');
                writer.write_all(out.as_bytes())?;
                continue;
            }
        };
        if let Request::OpenPty { argv, env, cwd, cols, rows } = req {
            match open_pty(argv, env, cwd, cols, rows) {
                Ok((master, pid)) => {
                    let mut out = serde_json::to_string(&Reply::ok().with("pid", pid))?;
                    out.push('\n');
                    send_with_fd(&writer, out.as_bytes(), &master)?;
                }
                Err(e) => {
                    let mut out = serde_json::to_string(&Reply::err(format!("{e:#}")))?;
                    out.push('\n');
                    writer.write_all(out.as_bytes())?;
                }
            }
            continue;
        }
        let reply = handle_request(req, &shared);
        let mut out = serde_json::to_string(&reply)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
    }
    Ok(())
}

/// Reply line + the pty master fd in one message (SCM_RIGHTS rides on the first byte).
fn send_with_fd(sock: &UnixStream, bytes: &[u8], fd: &std::os::fd::OwnedFd) -> Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    use std::os::fd::AsFd;
    let fds = [fd.as_fd()];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    control.push(SendAncillaryMessage::ScmRights(&fds));
    sendmsg(sock, &[std::io::IoSlice::new(bytes)], &mut control, SendFlags::empty())?;
    Ok(())
}

/// A pty with `argv` on it: the child gets a new session, the slave as its controlling tty and as
/// stdin/stdout/stderr. Returns (master, pid).
fn open_pty(argv: Vec<String>, env: BTreeMap<String, String>, cwd: Option<PathBuf>, cols: u16, rows: u16) -> Result<(std::os::fd::OwnedFd, u32)> {
    use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt};
    use rustix::termios::{Winsize, tcsetwinsize};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let Some((prog, rest)) = argv.split_first() else { bail!("empty argv") };
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).context("openpt")?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let slave = ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).context("pty peer")?;
    let _ = tcsetwinsize(&slave, Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 });
    let slave_raw = slave.as_raw_fd();
    let mut cmd = Command::new(prog);
    cmd.args(rest).envs(&env).env("TERM", "xterm-256color");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            rustix::process::setsid()?;
            let slave = std::os::fd::BorrowedFd::borrow_raw(slave_raw);
            rustix::process::ioctl_tiocsctty(slave)?;
            for target in 0..3 {
                let mut t = OwnedFd::from_raw_fd(target);
                rustix::io::dup2(slave, &mut t)?;
                std::mem::forget(t);
            }
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("spawn {prog} on pty"))?;
    let pid = child.id();
    drop(slave);
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok((master, pid))
}

fn handle_request(req: Request, shared: &Shared) -> Reply {
    match req {
        Request::Spawn { argv, env, cwd } => match spawn_agent(argv, env, cwd, shared) {
            Ok(pid) => Reply::ok().with("pid", pid),
            Err(e) => Reply::err(format!("{e:#}")),
        },
        Request::Status => {
            let mut st = shared.0.lock().unwrap();
            reap(&mut st);
            Reply::ok()
                .with("running", st.child.is_some())
                .with("pid", st.pid)
                .with("exit", st.exit)
                .with("buffered_lines", st.outbox.len() as u64)
        }
        Request::Kill { grace_ms } => match kill_agent(shared, grace_ms) {
            Ok(code) => Reply::ok().with("exit", code),
            Err(e) => Reply::err(format!("{e:#}")),
        },
        Request::Revert { path } => match revert(&path) {
            Ok(what) => Reply::ok().with("action", what),
            Err(e) => Reply::err(format!("{e:#}")),
        },
        Request::ReadFile { path } => match std::fs::read_to_string(&path) {
            Ok(content) => Reply::ok().with("content", content),
            Err(e) => Reply::err(format!("{}: {e}", path.display())),
        },
        Request::WriteFile { path, content } => match write_file(&path, &content) {
            Ok(()) => Reply::ok(),
            Err(e) => Reply::err(format!("{e:#}")),
        },
        Request::OpenPty { .. } => Reply::err("handled by the control loop"),
    }
}

fn spawn_agent(argv: Vec<String>, env: BTreeMap<String, String>, cwd: Option<PathBuf>, shared: &Shared) -> Result<u32> {
    let (lock, cvar) = &**shared;
    let mut st = lock.lock().unwrap();
    reap(&mut st);
    if st.child.is_some() {
        bail!("agent already running (pid {})", st.pid.unwrap_or(0));
    }
    let Some((prog, rest)) = argv.split_first() else { bail!("empty argv") };
    let mut cmd = Command::new(prog);
    cmd.args(rest)
        .envs(&env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // own process group: `npx …` style launchers re-parent the real agent otherwise
    cmd.process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("spawn {prog}"))?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    st.stdin = child.stdin.take();
    st.pid = Some(pid);
    st.exit = None;
    st.outbox.clear();
    st.outbox_bytes = 0;
    st.child = Some(child);
    st.touch();
    drop(st);
    eprintln!("sill-shim: agent {prog} started, pid {pid}");

    // stderr → our stderr (the unit's journal)
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("agent[{pid}]: {line}");
        }
    });
    // stdout → outbox (bounded)
    let shared2 = shared.clone();
    std::thread::spawn(move || {
        let (lock, cvar) = &*shared2;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let mut st = lock.lock().unwrap();
            while st.outbox_bytes > OUTBOX_LIMIT {
                st = cvar.wait(st).unwrap();
            }
            st.outbox_bytes += line.len() + 1;
            st.outbox.push_back(line);
            st.touch();
            cvar.notify_all();
        }
        let mut st = lock.lock().unwrap();
        reap(&mut st);
        eprintln!("sill-shim: agent pid {pid} stdout closed (exit {:?})", st.exit);
        cvar.notify_all();
    });
    let _ = cvar;
    Ok(pid)
}

/// Collect the exit status if the child has ended (non-blocking).
fn reap(st: &mut State) {
    if let Some(child) = st.child.as_mut()
        && let Ok(Some(status)) = child.try_wait()
    {
        st.exit = Some(status.code().unwrap_or(-1));
        st.child = None;
        st.stdin = None;
    }
}

fn kill_agent(shared: &Shared, grace_ms: u64) -> Result<Option<i32>> {
    use rustix::process::{Pid, Signal, kill_process_group};
    let (lock, cvar) = &**shared;
    let pid = {
        let mut st = lock.lock().unwrap();
        reap(&mut st);
        let Some(pid) = st.pid.filter(|_| st.child.is_some()) else { return Ok(st.exit) };
        st.stdin = None; // EOF first: well-behaved agents exit on it
        pid
    };
    let pgid = Pid::from_raw(pid as i32).context("pid")?;
    let _ = kill_process_group(pgid, Signal::TERM);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(grace_ms);
    loop {
        let mut st = lock.lock().unwrap();
        reap(&mut st);
        if st.child.is_none() {
            cvar.notify_all();
            return Ok(st.exit);
        }
        drop(st);
        if std::time::Instant::now() >= deadline {
            let _ = kill_process_group(pgid, Signal::KILL);
            let mut st = lock.lock().unwrap();
            if let Some(child) = st.child.as_mut() {
                let status = child.wait()?;
                st.exit = Some(status.code().unwrap_or(-9));
                st.child = None;
            }
            cvar.notify_all();
            return Ok(st.exit);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Kill the agent after `timeout` of silence with no shelf attached. The jail stays; the shelf resumes
/// the ACP session with session/load when it comes back (SPEC §4.10 #17).
fn idle_monitor(shared: Shared, timeout: std::time::Duration) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        let idle = {
            let mut st = shared.0.lock().unwrap();
            reap(&mut st);
            st.child.is_some()
                && !st.relay_connected
                && st.last_activity.map(|t| t.elapsed() >= timeout).unwrap_or(false)
        };
        if idle {
            eprintln!("sill-shim: agent idle for {}s with no shelf attached, stopping it", timeout.as_secs());
            if let Err(e) = kill_agent(&shared, 5000) {
                eprintln!("sill-shim: idle stop: {e:#}");
            }
        }
    }
}

// --- agent relay ---------------------------------------------------------------------------------

fn agent_relay(reader: BufReader<UnixStream>, writer: UnixStream, shared: Shared) -> Result<()> {
    let generation = {
        let mut st = shared.0.lock().unwrap();
        st.relay_generation += 1;
        st.relay_connected = true;
        st.touch();
        shared.1.notify_all();
        st.relay_generation
    };
    let sock_for_reader = writer.try_clone()?;
    // outbox → socket
    let shared_w = shared.clone();
    let mut writer_sock = writer;
    let out_thread = std::thread::spawn(move || {
        let (lock, cvar) = &*shared_w;
        loop {
            let line = {
                let mut st = lock.lock().unwrap();
                loop {
                    if st.relay_generation != generation {
                        return;
                    }
                    if let Some(line) = st.outbox.pop_front() {
                        st.outbox_bytes -= line.len() + 1;
                        cvar.notify_all();
                        break line;
                    }
                    if st.child.is_none() && st.stdin.is_none() {
                        // agent gone and nothing left: end the stream
                        let _ = writer_sock.shutdown(std::net::Shutdown::Write);
                        return;
                    }
                    st = cvar.wait(st).unwrap();
                }
            };
            if writer_sock.write_all(line.as_bytes()).is_err() || writer_sock.write_all(b"\n").is_err() {
                return;
            }
        }
    });
    // socket → agent stdin (this thread)
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let mut st = shared.0.lock().unwrap();
        if st.relay_generation != generation {
            break;
        }
        st.touch();
        let Some(stdin) = st.stdin.as_mut() else { break };
        if stdin.write_all(line.as_bytes()).is_err() || stdin.write_all(b"\n").is_err() || stdin.flush().is_err() {
            break;
        }
    }
    let _ = sock_for_reader.shutdown(std::net::Shutdown::Both);
    {
        // wake the writer so it notices the generation (if replaced) or the closed socket
        let mut st = shared.0.lock().unwrap();
        if st.relay_generation == generation {
            st.relay_generation += 1;
            st.relay_connected = false;
            st.touch();
        }
        shared.1.notify_all();
    }
    let _ = out_thread.join();
    Ok(())
}

// --- proxy forwarder -----------------------------------------------------------------------------

/// The jail's network namespace can't reach the host's loopback; a Unix socket path crosses it.
fn forward_tcp_to_unix(listen: &str, socket: PathBuf) -> Result<()> {
    let l = std::net::TcpListener::bind(listen).with_context(|| format!("listen on {listen}"))?;
    for conn in l.incoming() {
        let Ok(conn) = conn else { continue };
        let socket = socket.clone();
        std::thread::spawn(move || {
            let Ok(unix) = UnixStream::connect(&socket) else { return };
            let (mut a, mut b) = (conn, unix);
            let (Ok(mut a2), Ok(mut b2)) = (a.try_clone(), b.try_clone()) else { return };
            let t = std::thread::spawn(move || {
                let _ = std::io::copy(&mut a2, &mut b2);
                let _ = b2.shutdown(std::net::Shutdown::Write);
            });
            let _ = std::io::copy(&mut b, &mut a);
            let _ = a.shutdown(std::net::Shutdown::Write);
            let _ = t.join();
        });
    }
    Ok(())
}

// --- files ---------------------------------------------------------------------------------------

fn host_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{} is not absolute", path.display());
    }
    Ok(Path::new(HOST_VIEW).join(path.strip_prefix("/").unwrap()))
}

/// Make `path` identical to the host's version: copy it back, or remove it if the host doesn't have it.
fn revert(path: &Path) -> Result<&'static str> {
    let host = host_path(path)?;
    match host.symlink_metadata() {
        Err(_) => match path.symlink_metadata() {
            Err(_) => Ok("absent"),
            Ok(md) if md.is_dir() => {
                std::fs::remove_dir_all(path)?;
                Ok("removed")
            }
            Ok(_) => {
                std::fs::remove_file(path)?;
                Ok("removed")
            }
        },
        Ok(hmd) if hmd.file_type().is_symlink() => {
            let target = std::fs::read_link(&host)?;
            let _ = std::fs::remove_file(path);
            std::os::unix::fs::symlink(target, path)?;
            Ok("restored")
        }
        Ok(hmd) if hmd.is_dir() => {
            if path.symlink_metadata().map(|m| !m.is_dir()).unwrap_or(false) {
                std::fs::remove_file(path)?;
            }
            std::fs::create_dir_all(path)?;
            std::fs::set_permissions(path, hmd.permissions())?;
            Ok("restored")
        }
        Ok(hmd) => {
            if path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
                std::fs::remove_dir_all(path)?;
            }
            std::fs::copy(&host, path).with_context(|| format!("copy {} back", path.display()))?;
            std::fs::set_permissions(path, hmd.permissions())?;
            Ok("restored")
        }
    }
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))
}
