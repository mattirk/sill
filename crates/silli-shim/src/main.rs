//! silli-shim: the supervisor inside the jail (SPEC §4.3).
//!
//! Started by bwrap as the jail's first process with the control socket inherited as a file
//! descriptor (`--control-fd N`; `--listen PATH` for development). It owns everything that lives in the
//! jail: the agent process and its stdio, later the ptys. The daemon connects to the socket;
//! each connection starts with one JSON "hello" line naming its role:
//!
//! - `{"role":"control"}` — newline-delimited JSON requests, one reply line each (see [`Request`]);
//! - `{"role":"agent"}`   — after the hello the connection is the agent's ACP byte stream: lines from
//!   the socket go to the agent's stdin, the agent's stdout lines go to the socket. Only one agent
//!   connection at a time; a new one replaces the old. While no one is connected the agent's output
//!   is buffered (bounded; the agent blocks on stdout when the buffer is full), so the daemon can go
//!   away and come back without the agent noticing.
//!
//! Plain std threads: the shim has nothing to do but wait on file descriptors.
//!
//! The same binary is `silli-proc` inside the jail (docs/tasks.md §2.4): run under that name (or
//! as `silli-shim proc …`) it is the client of the managed-process service in [`procs`]. And it is
//! `silli-memory` (docs/memory.md §2.3): search, show and transcript over the read-only snapshot of
//! silli's memory at `/.silli/memory/` ([`memory_cli`]; the queries are `silli_memory`'s).

mod activity;
mod bus;
mod forward;
mod images;
mod procs;

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The real tree, read-only, as mounted by the jail builder (silli-core `jail::HOST_VIEW`).
const HOST_VIEW: &str = "/.silli/host";
/// Buffered agent output while no one is connected.
const OUTBOX_LIMIT: usize = 64 << 20;

#[derive(Deserialize)]
struct Hello {
    role: String,
    /// `{"role":"port","port":N}`: relay the connection to `127.0.0.1:N` in the jail, for a port a
    /// running managed process declared (docs/tasks.md §2.5).
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    /// Start the agent process. Fails if one is running.
    Spawn {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Status,
    /// How many frontends the daemon has attached to this session (docs/daemon.md §3.6): the agent
    /// stream is the daemon's and always connected, so this is what "nobody watching" means for
    /// the idle rule; the stream dropping (a dead daemon) still counts as nobody.
    Attached {
        frontends: u32,
    },
    /// SIGTERM the agent's process group, SIGKILL after `grace_ms`.
    Kill {
        #[serde(default = "default_grace")]
        grace_ms: u64,
    },
    /// Restore a path from the host view (SPEC §4.4 revert).
    Revert {
        path: PathBuf,
    },
    /// Open a regular image in the jail and return its read-only fd via SCM_RIGHTS.
    OpenImage {
        path_bytes: Vec<u8>,
    },
    /// Managed processes (docs/tasks.md §2.4): what the jail's `silli-proc` can do, for the daemon.
    ProcList,
    /// What runs in the jail now, classed (`activity.rs`).
    Activity,
    /// Stop a command tree the agent started (its root's pid, as the activity report names it).
    StopCommand {
        pid: u32,
    },
    ProcTail {
        name: String,
        #[serde(default = "default_lines")]
        lines: usize,
    },
    ProcStop {
        name: String,
    },
    ProcStopAll,
    ProcDismiss {
        name: String,
    },
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
fn default_lines() -> usize {
    50
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
    /// A reply object from the process service, as is.
    fn from_value(v: serde_json::Value) -> Self {
        let mut data = match v {
            serde_json::Value::Object(m) => m,
            _ => Default::default(),
        };
        let ok = data.remove("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let error = data.remove("error").and_then(|v| v.as_str().map(str::to_string));
        Reply { ok, error, data }
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
    /// The daemon is attached to the agent stream.
    relay_connected: bool,
    /// Frontends attached through the daemon, as it last said (None: never told — the stream alone
    /// decides, as before the daemon).
    frontends: Option<u32>,
    /// Last agent stdio traffic or daemon (dis)connect.
    last_activity: Option<std::time::Instant>,
}

impl State {
    fn touch(&mut self) {
        self.last_activity = Some(std::time::Instant::now());
    }
}

type Shared = Arc<(Mutex<State>, Condvar)>;

/// `silli-memory …` in the jail: the snapshot the daemon wrote, read-only; exit status 1 on an error,
/// 2 when this jail has no memory (memory off, or a session started before it was turned on).
fn memory_cli(args: &[String]) -> i32 {
    let dir = std::env::var_os("SILLI_MEMORY_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(silli_memory::JAIL_DIR));
    let conn = match silli_memory::open_snapshot(&dir) {
        Ok(Some(c)) => c,
        Ok(None) => {
            eprintln!("silli-memory: no memory here yet ({} has no snapshot: memory is off, or no session has ended since it was turned on)", dir.display());
            return 2;
        }
        Err(e) => {
            eprintln!("silli-memory: {e:#}");
            return 1;
        }
    };
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    match silli_memory::cli(args, &conn, &home, now, |id| silli_memory::snapshot_kept(&conn, id)) {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(e) => {
            eprintln!("silli-memory: {e:#}");
            1
        }
    }
}

mod host_cli;

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `silli-proc …` (the binary under that name in the jail) or `silli-shim proc …`: the client
    let me = std::env::args().next().unwrap_or_default();
    if Path::new(&me).file_name().and_then(|n| n.to_str()) == Some("silli-host") {
        return host_cli::run(&args);
    }
    if Path::new(&me).file_name().and_then(|n| n.to_str()) == Some("silli-mac") {
        return host_cli::run_mac(&args);
    }
    if Path::new(&me).file_name().and_then(|n| n.to_str()) == Some("silli-proc") {
        std::process::exit(procs::cli(args));
    }
    if args.first().map(String::as_str) == Some("proc") {
        std::process::exit(procs::cli(args.split_off(1)));
    }
    if Path::new(&me).file_name().and_then(|n| n.to_str()) == Some("silli-memory") {
        std::process::exit(memory_cli(&args));
    }
    if args.first().map(String::as_str) == Some("memory") {
        std::process::exit(memory_cli(&args[1..]));
    }
    // CLOEXEC alone cannot prevent same-UID children stealing private descriptors with
    // pidfd_getfd or ptrace. Protect the supervisor before any threads or children start.
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable).context("protect shim descriptors")?;
    // --forward 127.0.0.1:3128=/.silli/proxy.sock : TCP → Unix forwarder for the session proxy (SPEC §4.8)
    while let Some(i) = args.iter().position(|a| a == "--forward") {
        if i + 1 >= args.len() {
            bail!("--forward expects ADDR=PATH");
        }
        let spec = args.remove(i + 1);
        args.remove(i);
        let (addr, path) = spec.split_once('=').context("--forward expects ADDR=PATH")?;
        let (addr, path) = (addr.to_string(), PathBuf::from(path));
        std::thread::spawn(move || {
            if let Err(e) = forward::serve(&addr, path) {
                eprintln!("silli-shim: forwarder: {e:#}");
            }
        });
    }
    // --agent-idle SECS : stop the agent process after SECS without traffic and with nobody attached
    // (no agent stream, or the daemon's last `attached` said 0 frontends)
    let mut agent_idle: Option<u64> = None;
    while let Some(i) = args.iter().position(|a| a == "--agent-idle") {
        if i + 1 >= args.len() {
            bail!("--agent-idle expects seconds");
        }
        agent_idle = Some(args.remove(i + 1).parse().context("--agent-idle expects seconds")?);
        args.remove(i);
    }
    // --bus PATH : Desktop development's private D-Bus session bus, a dbus-daemon of this jail's
    // own on PATH, started before anything else can ask for it
    let mut bus: Option<PathBuf> = None;
    while let Some(i) = args.iter().position(|a| a == "--bus") {
        if i + 1 >= args.len() {
            bail!("--bus expects a socket path");
        }
        bus = Some(PathBuf::from(args.remove(i + 1)));
        args.remove(i);
    }
    let listener = match args.as_slice() {
        [flag, fd] if flag == "--control-fd" => {
            let fd: i32 = fd.parse().context("--control-fd expects a number")?;
            // SAFETY: the launcher put a listening Unix socket on this fd before exec.
            let l = unsafe { UnixListener::from_raw_fd(fd) };
            // the launcher cleared close-on-exec so the fd would survive *our* exec; from here on it
            // must not survive anyone else's: the agent or a shell could accept() the daemon's
            // connections and answer as the shim (SPEC §4.11 #2)
            rustix::io::fcntl_setfd(&l, rustix::io::FdFlags::CLOEXEC).context("control fd cloexec")?;
            l
        }
        [flag, path] if flag == "--listen" => {
            let _ = std::fs::remove_file(path);
            UnixListener::bind(path).with_context(|| format!("listen on {path}"))?
        }
        _ => bail!("usage: silli-shim --control-fd N | --listen PATH"),
    };
    if let Some(socket) = &bus {
        bus::start(socket)?;
    }
    eprintln!("silli-shim: ready (pid {})", std::process::id());

    let shared: Shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
    // the managed-process service and its door for the jail
    let procs = procs::Procs::default();
    {
        let p = procs.clone();
        std::thread::spawn(move || {
            if let Err(e) = p.serve(&procs::socket_path()) {
                eprintln!("silli-shim: process service: {e:#}");
            }
        });
    }
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
                eprintln!("silli-shim: accept: {e}");
                if accept_errors >= 10 || e.raw_os_error() == Some(9) {
                    bail!("control socket unusable, giving up");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        let shared = shared.clone();
        let procs = procs.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(conn, shared, procs) {
                eprintln!("silli-shim: connection: {e:#}");
            }
        });
    }
    Ok(())
}

fn handle_connection(conn: UnixStream, shared: Shared, procs: procs::Procs) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut hello = String::new();
    reader.read_line(&mut hello)?;
    let hello: Hello = serde_json::from_str(hello.trim()).context("hello line")?;
    match hello.role.as_str() {
        "control" => control_loop(reader, conn, shared, procs),
        "port" => port_relay(reader, conn, hello.port.context("port role without a port")?, procs),
        "agent" => agent_relay(reader, conn, shared),
        other => bail!("unknown role {other}"),
    }
}

// --- control -------------------------------------------------------------------------------------

fn control_loop(reader: BufReader<UnixStream>, mut writer: UnixStream, shared: Shared, procs: procs::Procs) -> Result<()> {
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
        if let Request::OpenImage { path_bytes } = req {
            match images::open_image(&path_bytes) {
                Ok(fd) => send_with_fd(&writer, b"{\"ok\":true}\n", &fd)?,
                Err(e) => {
                    let reply = Reply::err(e).with("errno", e.raw_os_error());
                    writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                }
            }
            continue;
        }
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
        let reply = handle_request(req, &shared, &procs);
        let mut out = serde_json::to_string(&reply)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod revert_tests {
    use super::*;

    /// A folder bound from the host (here: the repository's own `target/`, never an overlay) is
    /// not the session's to revert: nothing in it is removed.
    #[test]
    fn revert_refuses_a_folder_that_is_not_in_the_overlay() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("silli-shim-revert-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("index"), "the clone's index").unwrap();
        std::fs::write(dir.join("sub/f"), "kept").unwrap();
        let e = revert(&dir.join("index")).unwrap_err();
        assert!(format!("{e:#}").contains("not in the session's overlay"), "{e:#}");
        assert!(dir.join("index").exists());
        // nor below a directory being removed
        let (parent, name) = parent_and_name(&dir).unwrap();
        assert!(remove_at(&parent, &name).is_err());
        assert!(dir.join("sub/f").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod image_control_tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn image_control_passes_binary_fds_and_preserves_read_errors() {
        let dir = std::env::temp_dir().join(format!("silli-image-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.jpg");
        let bytes = b"\xff\xd8\x00\x80\xfe";
        std::fs::write(&path, bytes).unwrap();
        let socket = dir.join("shim.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut hello = String::new();
            reader.read_line(&mut hello).unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(&hello).unwrap()["role"], "control");
            let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
            control_loop(reader, stream, shared, procs::Procs::default()).unwrap();
        });
        let mut control = silli_core::shim::Control::connect(&socket).unwrap();
        let mut image = control.open_image(&path).unwrap();
        let mut actual = Vec::new();
        image.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, bytes);
        assert!(rustix::io::fcntl_getfd(&image).unwrap().contains(rustix::io::FdFlags::CLOEXEC));
        let error = control.open_image(&dir.join("missing.jpg")).unwrap_err();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::NotFound);
        // An error must leave the control stream usable.
        assert!(control.open_image(&path).is_ok());
        drop(control);
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// Reply line + a descriptor (SCM_RIGHTS rides on the first byte).
fn send_with_fd(sock: &UnixStream, bytes: &[u8], fd: &std::os::fd::OwnedFd) -> Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
    use std::os::fd::AsFd;
    let fds = [fd.as_fd()];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    control.push(SendAncillaryMessage::ScmRights(&fds));
    let written = sendmsg(sock, &[std::io::IoSlice::new(bytes)], &mut control, SendFlags::empty())?;
    // A stream send can be partial; only the first send carries the descriptor.
    let mut socket = sock;
    socket.write_all(&bytes[written..])?;
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
    // the person's terminal leads its own session: what runs in it is theirs, not the agent's
    activity::terminal_started(pid);
    drop(slave);
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok((master, pid))
}

fn handle_request(req: Request, shared: &Shared, procs: &procs::Procs) -> Reply {
    match req {
        Request::ProcList => Reply::from_value(procs.handle(procs::Op::List)),
        Request::Activity => {
            let agent = shared.0.lock().unwrap().pid;
            let report = activity::report(agent, &procs.running_pids());
            match serde_json::to_value(report) {
                Ok(value) => Reply::ok().with("activity", value),
                Err(e) => Reply::err(e.to_string()),
            }
        }
        Request::StopCommand { pid } => {
            let agent = shared.0.lock().unwrap().pid;
            match activity::stop_command(pid, agent, &procs.running_pids()) {
                Ok(()) => Reply::ok(),
                Err(e) => Reply::err(e),
            }
        }
        Request::ProcTail { name, lines } => Reply::from_value(procs.handle(procs::Op::Tail { name, lines })),
        Request::ProcStop { name } => Reply::from_value(procs.handle(procs::Op::Stop { name })),
        Request::ProcStopAll => Reply::from_value(procs.handle(procs::Op::StopAll)),
        Request::ProcDismiss { name } => Reply::from_value(procs.handle(procs::Op::Dismiss { name })),
        Request::Spawn { argv, env, cwd } => match spawn_agent(argv, env, cwd, shared) {
            Ok(pid) => Reply::ok().with("pid", pid),
            Err(e) => Reply::err(format!("{e:#}")),
        },
        Request::Attached { frontends } => {
            let mut st = shared.0.lock().unwrap();
            st.frontends = Some(frontends);
            st.touch();
            Reply::ok()
        }
        Request::Status => {
            let mut st = shared.0.lock().unwrap();
            reap(&mut st);
            Reply::ok()
                .with("running", st.child.is_some())
                .with("frontends", st.frontends)
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
        Request::OpenPty { .. } | Request::OpenImage { .. } => Reply::err("handled by the control loop"),
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
    cmd.args(rest).envs(&env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
    eprintln!("silli-shim: agent {prog} started, pid {pid}");

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
        eprintln!("silli-shim: agent pid {pid} stdout closed (exit {:?})", st.exit);
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
            // SIGKILL the group and the leader itself, then poll: never a blocking wait() while
            // holding the lock — every other control request (status, spawn, the daemon's startup)
            // queues on that lock, and an agent that somehow survives would wedge the whole shim
            let _ = kill_process_group(pgid, Signal::KILL);
            let _ = rustix::process::kill_process(pgid, Signal::KILL);
            let hard = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                let mut st = lock.lock().unwrap();
                reap(&mut st);
                if st.child.is_none() {
                    cvar.notify_all();
                    return Ok(st.exit);
                }
                drop(st);
                if std::time::Instant::now() >= hard {
                    eprintln!("silli-shim: agent pid {pid} did not die after SIGKILL; giving up on it");
                    bail!("agent pid {pid} did not exit after SIGKILL");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Kill the agent after `timeout` of silence with nobody attached — no agent stream, or the daemon
/// said its frontends are gone. The jail stays; the next attach starts the agent again and resumes
/// the ACP session with session/load (SPEC §4.10 #17).
fn idle_monitor(shared: Shared, timeout: std::time::Duration) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        let idle = {
            let mut st = shared.0.lock().unwrap();
            reap(&mut st);
            st.child.is_some() && (!st.relay_connected || st.frontends == Some(0)) && st.last_activity.map(|t| t.elapsed() >= timeout).unwrap_or(false)
        };
        if idle {
            eprintln!("silli-shim: agent idle for {}s with nobody attached, stopping it", timeout.as_secs());
            if let Err(e) = kill_agent(&shared, 5000) {
                eprintln!("silli-shim: idle stop: {e:#}");
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

// --- exposed ports --------------------------------------------------------------------------------

/// The daemon's connection for an exposed port: one line says yes or no, then bytes both ways between the
/// connection and `127.0.0.1:port` inside. Only a port a running managed process declared.
fn port_relay(mut reader: BufReader<UnixStream>, mut writer: UnixStream, port: u16, procs: procs::Procs) -> Result<()> {
    let declared = procs.list().iter().any(|p| p.port == Some(port) && p.state == procs::ProcState::Running);
    if !declared {
        writer.write_all(format!("{}\n", serde_json::to_string(&Reply::err(format!("no running managed process declared port {port}")))?).as_bytes())?;
        return Ok(());
    }
    let tcp = match std::net::TcpStream::connect(("127.0.0.1", port)) {
        Ok(t) => t,
        Err(e) => {
            writer.write_all(format!("{}\n", serde_json::to_string(&Reply::err(format!("port {port} inside the jail: {e}")))?).as_bytes())?;
            return Ok(());
        }
    };
    writer.write_all(format!("{}\n", serde_json::to_string(&Reply::ok())?).as_bytes())?;
    let (mut tcp_r, mut tcp_w) = (tcp.try_clone()?, tcp);
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut tcp_w);
        let _ = tcp_w.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut tcp_r, &mut writer);
    let _ = writer.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
    Ok(())
}

// --- files ---------------------------------------------------------------------------------------
//
// Revert and write work on the agent's tree: every path component is opened without following a
// symlink and the final name is handled relative to its directory, so a link the agent planted
// cannot steer the write into a pin or a `direct` tree (security.md S11, SPEC §4.11 #10).

fn host_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{} is not absolute", path.display());
    }
    Ok(Path::new(HOST_VIEW).join(path.strip_prefix("/").unwrap()))
}

/// `path` as its parent directory (opened with no symlink on the way) and its final name.
fn parent_and_name(path: &Path) -> Result<(std::os::fd::OwnedFd, std::ffi::OsString)> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, open, openat2};
    if !path.is_absolute() {
        bail!("{} is not absolute", path.display());
    }
    let name = match path.file_name() {
        Some(n) if n != ".." && n != "." => n.to_os_string(),
        _ => bail!("{}: no file name", path.display()),
    };
    let parent = path.parent().with_context(|| format!("{}: no parent", path.display()))?;
    let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = open("/", flags, Mode::empty())?;
    let rel = parent.strip_prefix("/").unwrap_or(parent);
    if rel.as_os_str().is_empty() {
        return Ok((root, name));
    }
    let dir = openat2(&root, rel, flags, Mode::empty(), ResolveFlags::NO_SYMLINKS | ResolveFlags::BENEATH)
        .with_context(|| format!("{}: not reachable without following a symlink (use the real path)", parent.display()))?;
    Ok((dir, name))
}

/// What sits at `name` in `dir`, not followed.
fn type_at(dir: &impl std::os::fd::AsFd, name: &std::ffi::OsStr) -> Option<rustix::fs::FileType> {
    use rustix::fs::{AtFlags, FileType, RawMode, statat};
    statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).ok().map(|st| FileType::from_raw_mode(st.st_mode as RawMode))
}

/// overlayfs's `f_type` (linux/magic.h).
const OVERLAYFS_SUPER_MAGIC: i64 = 0x794c_7630;

/// `dir` is on the session's overlay. A revert only ever undoes the overlay: a directory that is
/// not on it is a bind of a host folder (a live pin, a workspace's clone, docs/parallel-coding.md
/// §12), where a revert would write the host itself — it deleted a workspace clone's `.git/index`
/// through the clone's bind once (2026-11-09).
fn on_overlay(dir: &impl std::os::fd::AsFd, what: &Path) -> Result<()> {
    let fs = rustix::fs::fstatfs(dir)?;
    if fs.f_type as i64 != OVERLAYFS_SUPER_MAGIC {
        bail!("{}: not in the session's overlay (a folder bound from the host); nothing reverted", what.display());
    }
    Ok(())
}

/// Remove `name` in `dir`, a tree or a single entry, never following a link, nor into a folder
/// bound from the host.
fn remove_at(dir: &impl std::os::fd::AsFd, name: &std::ffi::OsStr) -> Result<()> {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, unlinkat};
    match type_at(dir, name) {
        None => Ok(()),
        Some(FileType::Directory) => {
            let sub = openat(dir, name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
            on_overlay(&sub, Path::new(name))?;
            let names: Vec<std::ffi::OsString> = Dir::read_from(&sub)?
                .filter_map(Result::ok)
                .map(|e| <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(e.file_name().to_bytes()).to_os_string())
                .filter(|n| n != "." && n != "..")
                .collect();
            for n in names {
                remove_at(&sub, &n)?;
            }
            unlinkat(dir, name, AtFlags::REMOVEDIR)?;
            Ok(())
        }
        Some(_) => Ok(unlinkat(dir, name, AtFlags::empty())?),
    }
}

/// Make `path` identical to the host's version: copy it back, or remove it if the host doesn't have it.
fn revert(path: &Path) -> Result<&'static str> {
    use rustix::fs::{FileType, Mode, OFlags, RawMode, ResolveFlags, fchmod, fstat, mkdirat, openat, openat2, symlinkat};
    let host = host_path(path)?;
    let (dir, name) = parent_and_name(path)?;
    on_overlay(&dir, path)?;
    let cur = type_at(&dir, &name);
    let Ok(hmd) = host.symlink_metadata() else {
        if cur.is_none() {
            return Ok("absent");
        }
        remove_at(&dir, &name)?;
        return Ok("removed");
    };
    let mode = Mode::from_raw_mode((hmd.permissions().mode() & 0o7777) as RawMode);
    if hmd.file_type().is_symlink() {
        remove_at(&dir, &name)?;
        symlinkat(std::fs::read_link(&host)?, &dir, &name)?;
        return Ok("restored");
    }
    if hmd.is_dir() {
        if cur != Some(FileType::Directory) {
            remove_at(&dir, &name)?;
            mkdirat(&dir, &name, mode)?;
        }
        let d = openat(&dir, &name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
        fchmod(&d, mode)?;
        return Ok("restored");
    }
    if cur == Some(FileType::Directory) {
        remove_at(&dir, &name)?;
    }
    let mut from = std::fs::File::open(&host).with_context(|| format!("open the host's {}", path.display()))?;
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = openat2(&dir, &name, flags, mode, ResolveFlags::NO_SYMLINKS | ResolveFlags::BENEATH)
        .with_context(|| format!("open {} for writing (no symlinks)", path.display()))?;
    if FileType::from_raw_mode(fstat(&fd)?.st_mode as RawMode) != FileType::RegularFile {
        bail!("{}: not a regular file", path.display());
    }
    let mut to = std::fs::File::from(fd);
    std::io::copy(&mut from, &mut to).with_context(|| format!("copy {} back", path.display()))?;
    fchmod(&to, mode)?;
    Ok("restored")
}
