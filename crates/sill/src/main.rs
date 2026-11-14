//! `sill` command line. M0: drive the jail builder (`sill jail …`) and one development session
//! (`sill dev …`) by hand; the shelf and the real session commands come after.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use sill_core::acp::{self, AutoPermission, Event};
use sill_core::jail::{self, JailSpec};
use sill_core::ledger;
use sill_core::mounts;
use sill_core::policy::Policy;
use sill_core::shim;

mod shelf;

#[derive(Parser)]
#[command(name = "sill", version, about = "agent shelf")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build and run jails by hand (M0)
    Jail {
        #[command(subcommand)]
        cmd: JailCmd,
    },
    /// One development session: jail + shim + agent, driven step by step (M0)
    Dev {
        #[command(subcommand)]
        cmd: DevCmd,
    },
    /// Session launcher, runs inside the session's systemd user unit (SPEC §4.7): binds the shim and
    /// proxy sockets, starts bwrap with the shim inside, serves the proxy, exits when the jail ends
    Launch {
        /// session id (sockets under $XDG_RUNTIME_DIR/sill/<id>)
        #[arg(long)]
        session: String,
        #[command(flatten)]
        jail: JailArgs,
        /// sill-shim binary to bind into the jail (default: next to this executable)
        #[arg(long)]
        shim: Option<PathBuf>,
    },
    /// Serve a session's network proxy (SPEC §4.8) on an inherited listening socket
    Proxy {
        /// listening Unix socket fd (inherited from the launcher)
        #[arg(long, default_value_t = 3)]
        fd: i32,
        /// bind this socket path instead of using an inherited fd (testing)
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(short, long, default_value = "default")]
        policy: String,
    },
    /// Open the shelf (GTK); single instance per desktop session
    Shelf {
        /// working directory of the main session (default: current directory)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// start without showing the window (autostart); `sill toggle` shows it
        #[arg(long)]
        hidden: bool,
    },
    /// Show/hide the running shelf (bind this to a hotkey)
    Toggle,
    /// Show a policy as sill reads it
    Policy {
        /// name (searched in ~/.config/sill/policies, /etc/sill/policies, built-in) or path
        #[arg(default_value = "default")]
        name: String,
    },
}

#[derive(Subcommand)]
enum JailCmd {
    /// Print the bwrap command line for a jail
    Plan(RunArgs),
    /// Run a command in a jail (default: an interactive shell)
    Run(RunArgs),
    /// Copy one changed path from the session's upper tree to the host (SPEC §4.4 keep)
    Keep {
        #[command(flatten)]
        jail: JailArgs,
        /// host path to keep (as listed by `changes`)
        path: PathBuf,
        /// keep setuid/setgid bits
        #[arg(long)]
        allow_special: bool,
    },
    /// List what a jail's session has changed against the host (reads the upper trees)
    Changes {
        #[command(flatten)]
        jail: JailArgs,
        /// also list entries matching the policy's ledger_ignore patterns
        #[arg(long)]
        all: bool,
        /// print a unified diff for this host path
        #[arg(long)]
        diff: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DevCmd {
    /// Start the jail with sill-shim inside, detached; sockets under $XDG_RUNTIME_DIR/sill/dev
    Start {
        #[command(flatten)]
        jail: JailArgs,
        /// sill-shim binary to bind into the jail (default: next to this executable)
        #[arg(long)]
        shim: Option<PathBuf>,
    },
    /// Shim and agent status
    Status,
    /// Run one prompt in a fresh ACP session (starts the agent if needed)
    Prompt {
        text: String,
        /// agent command inside the jail
        #[arg(long, default_value = "claude-agent-acp")]
        agent: String,
        /// working directory for the agent (default: the jail's cwd given at start)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// kill a running agent first
        #[arg(long)]
        fresh: bool,
        /// answer permission requests with reject instead of allow-once
        #[arg(long)]
        reject: bool,
        /// print thoughts and raw updates too
        #[arg(short, long)]
        verbose: bool,
    },
    /// Restore a path inside the jail from the host (revert)
    Revert { path: PathBuf },
    /// Stop the agent and the jail
    Stop,
}

#[derive(Args, Clone)]
struct JailArgs {
    /// policy name or path
    #[arg(short, long, default_value = "default")]
    policy: String,
    /// session state dir (upper/work trees go below it)
    #[arg(short, long)]
    state_dir: Option<PathBuf>,
    /// accept a state dir inside an overlaid tree (development only)
    #[arg(long)]
    unsafe_state: bool,
    /// working directory inside the jail
    #[arg(short, long)]
    cwd: Option<PathBuf>,
    /// share the host network namespace (until the proxy exists)
    #[arg(long)]
    net: bool,
    /// extra environment, KEY=VALUE
    #[arg(short, long)]
    env: Vec<String>,
    /// host path of a running session proxy socket to bind at /.sill/proxy.sock (testing)
    #[arg(long)]
    proxy: Option<PathBuf>,
    /// bind this sill-shim binary at /.sill/shim (testing)
    #[arg(long)]
    shim_bin: Option<PathBuf>,
    /// dev start: detach with setsid instead of a systemd user unit
    #[arg(long)]
    no_unit: bool,
}

#[derive(Args, Clone)]
struct RunArgs {
    #[command(flatten)]
    jail: JailArgs,
    /// command (after --); default: $SHELL -l
    #[arg(last = true)]
    command: Vec<OsString>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Policy { name } => {
            let p = Policy::load(&name, &home()?)?;
            print!("{}", toml::to_string_pretty(&p)?);
            Ok(())
        }
        Cmd::Jail { cmd } => jail_cmd(cmd),
        Cmd::Dev { cmd } => dev_cmd(cmd),
        Cmd::Launch { session, jail, shim } => launch(&session, jail, shim),
        Cmd::Shelf { cwd, hidden } => shelf::run(cwd, hidden),
        Cmd::Toggle => shelf::toggle(),
        Cmd::Proxy { fd, socket, policy } => {
            let policy = Policy::load(&policy, &home()?)?;
            let listener = match socket {
                Some(path) => {
                    let _ = std::fs::remove_file(&path);
                    UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?
                }
                // SAFETY: the launcher put a listening Unix socket on this fd before exec.
                None => unsafe { UnixListener::from_raw_fd(fd) },
            };
            let rules = sill_core::proxy::Rules::from(&policy.network);
            eprintln!("sill proxy: mode {:?}, {} allow patterns", rules.mode, rules.allow.len());
            sill_core::proxy::serve(listener, rules, |line| eprintln!("sill proxy: {line}"))
        }
    }
}

// --- jail ----------------------------------------------------------------------------------------

fn jail_cmd(cmd: JailCmd) -> Result<()> {
    match cmd {
        JailCmd::Plan(a) => {
            let spec = build_spec(a.jail, a.command)?;
            let plan = spec.plan()?;
            for w in &plan.warnings {
                eprintln!("warning: {w}");
            }
            println!("# layers:");
            for l in &plan.layers {
                println!("#   {}  upper={}", l.host.display(), l.upper.display());
            }
            println!("{}", jail::render(&plan.argv));
            Ok(())
        }
        JailCmd::Keep { mut jail, path, allow_special } => {
            jail.unsafe_state = true;
            let spec = build_spec(jail, vec!["true".into()])?;
            let plan = spec.plan()?;
            let mut skip = plan.masked.clone();
            skip.extend(plan.direct.iter().cloned());
            let opts = ledger::ScanOptions { session_start: None, ignore: spec.policy.paths.ledger_ignore.clone(), skip };
            let changes = ledger::scan(&plan.layers, &opts)?;
            let path = std::path::absolute(path)?;
            let Some(c) = changes.iter().find(|c| c.path == path) else { bail!("{} is not changed", path.display()) };
            if c.conflict {
                bail!("{} changed on the host since the session started; not keeping", path.display());
            }
            let what = sill_core::keep::keep(c, &sill_core::keep::KeepOptions { expected_hash: None, allow_special })?;
            println!("{}: {what}", path.display());
            Ok(())
        }
        JailCmd::Changes { mut jail, all, diff } => {
            jail.unsafe_state = true; // reading only; the plan's state check is for running
            let spec = build_spec(jail, vec!["true".into()])?;
            let plan = spec.plan()?;
            print_changes(&spec, &plan, all, diff)
        }
        JailCmd::Run(a) => {
            let spec = build_spec(a.jail, a.command)?;
            let plan = spec.plan()?;
            for w in &plan.warnings {
                eprintln!("sill: warning: {w}");
            }
            jail::prepare(&plan)?;
            let err = std::process::Command::new(&plan.argv[0]).args(&plan.argv[1..]).exec();
            bail!("exec bwrap: {err}")
        }
    }
}

fn print_changes(spec: &JailSpec, plan: &jail::JailPlan, all: bool, diff: Option<PathBuf>) -> Result<()> {
    let mut skip = plan.masked.clone();
    skip.extend(plan.direct.iter().cloned());
    let opts = ledger::ScanOptions { session_start: None, ignore: spec.policy.paths.ledger_ignore.clone(), skip };
    let changes = ledger::scan(&plan.layers, &opts)?;
    if let Some(path) = diff {
        let path = std::path::absolute(path)?;
        let Some(c) = changes.iter().find(|c| c.path == path) else { bail!("{} is not changed", path.display()) };
        let host = if path.exists() { path.as_os_str().to_owned() } else { "/dev/null".into() };
        let upper = if c.kind == ledger::Kind::Deleted { "/dev/null".into() } else { c.upper.as_os_str().to_owned() };
        let st = std::process::Command::new("diff")
            .args(["-u", "--label"])
            .arg(format!("host:{}", path.display()))
            .arg("--label")
            .arg(format!("jail:{}", path.display()))
            .arg(host)
            .arg(upper)
            .status()?;
        std::process::exit(st.code().unwrap_or(2));
    }
    for c in &changes {
        if c.ignored && !all {
            continue;
        }
        println!(
            "{}{} {:>9}  {}{}",
            c.kind.letter(),
            if c.conflict { '!' } else { ' ' },
            c.size,
            c.path.display(),
            if c.ignored { "  (ignored)" } else { "" }
        );
    }
    let hidden = changes.iter().filter(|c| c.ignored).count();
    if hidden > 0 && !all {
        eprintln!("({hidden} ignored entries not shown; --all lists them)");
    }
    Ok(())
}

// --- dev session ---------------------------------------------------------------------------------

const DEV: &str = "dev";

fn dev_cmd(cmd: DevCmd) -> Result<()> {
    let rt = shim::runtime_dir(DEV)?;
    let socket = rt.join("shim.sock");
    match cmd {
        DevCmd::Start { jail, shim: shim_bin } => {
            std::fs::create_dir_all(&rt)?;
            if socket.exists() && shim::Control::connect(&socket).is_ok() {
                bail!("a dev session is already running ({})", socket.display());
            }
            let _ = std::fs::remove_file(&socket);
            if !jail.no_unit {
                return start_unit(DEV, jail, shim_bin);
            }
            let shim_bin = match shim_bin {
                Some(p) => p,
                None => std::env::current_exe()?.with_file_name("sill-shim"),
            };
            if !shim_bin.is_file() {
                bail!("sill-shim not found at {}", shim_bin.display());
            }
            let use_proxy = !jail.net;
            let mut command: Vec<OsString> = vec!["/.sill/shim".into(), "--control-fd".into(), "3".into()];
            if use_proxy {
                command.extend(["--forward".into(), format!("127.0.0.1:{}={}", jail::PROXY_PORT, jail::PROXY_SOCKET).into()]);
            }
            let mut spec = build_spec(jail, command)?;
            let idle = spec.policy.limits.agent_idle_minutes as u64 * 60;
            spec.command.extend(["--agent-idle".into(), idle.to_string().into()]);
            spec.extra_args = vec!["--ro-bind".into(), shim_bin.clone().into(), "/.sill/shim".into()];
            let proxy_socket = rt.join("proxy.sock");
            if use_proxy {
                let _ = std::fs::remove_file(&proxy_socket);
                let listener = UnixListener::bind(&proxy_socket).with_context(|| format!("bind {}", proxy_socket.display()))?;
                let log = std::fs::File::create(rt.join("proxy.log"))?;
                let mut cmd = std::process::Command::new(std::env::current_exe()?);
                cmd.args(["proxy", "--fd", "3", "--policy", &spec.policy.name]);
                let child = spawn_detached(cmd, listener.as_fd().as_raw_fd(), log)?;
                std::fs::write(rt.join("proxy.pid"), child.id().to_string())?;
                spec.proxy_socket = Some(proxy_socket);
            }
            let plan = spec.plan()?;
            for w in &plan.warnings {
                eprintln!("sill: warning: {w}");
            }
            jail::prepare(&plan)?;
            std::fs::write(rt.join("cwd"), spec.cwd.to_string_lossy().as_bytes())?;
            std::fs::write(rt.join("state_dir"), spec.state_dir.to_string_lossy().as_bytes())?;

            let listener = UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
            let log = std::fs::File::create(rt.join("jail.log"))?;
            let mut cmd = std::process::Command::new(&plan.argv[0]);
            cmd.args(&plan.argv[1..]);
            let child = spawn_detached(cmd, listener.as_fd().as_raw_fd(), log)?;
            std::fs::write(rt.join("bwrap.pid"), child.id().to_string())?;
            drop(listener); // the jail owns the listening end now
            println!("dev session started: bwrap pid {}, socket {}", child.id(), socket.display());
            println!("state dir {}, log {}", spec.state_dir.display(), rt.join("jail.log").display());
            Ok(())
        }
        DevCmd::Status => {
            let pid = std::fs::read_to_string(rt.join("bwrap.pid")).ok();
            println!("bwrap pid: {}", pid.as_deref().unwrap_or("-"));
            let mut c = shim::Control::connect(&socket)?;
            let s = c.status()?;
            println!(
                "agent: {} pid={:?} exit={:?} buffered_lines={}",
                if s.running { "running" } else { "not running" },
                s.pid,
                s.exit,
                s.buffered_lines
            );
            Ok(())
        }
        DevCmd::Prompt { text, agent, cwd, fresh, reject, verbose } => {
            let mut c = shim::Control::connect(&socket)?;
            let cwd = match cwd {
                Some(c) => c,
                None => PathBuf::from(std::fs::read_to_string(rt.join("cwd")).context("dev session not started")?),
            };
            if fresh {
                c.kill(3000)?;
            }
            let st = c.status()?;
            if !st.running {
                let argv = shell_words(&agent);
                let pid = c.spawn(&argv, &BTreeMap::new(), Some(&cwd))?;
                eprintln!("agent started in the jail, pid {pid}");
            }
            let stream = shim::agent_stream(&socket)?;
            let auto = if reject { AutoPermission::Reject } else { AutoPermission::AllowOnce };
            use std::io::Write;
            acp::run_prompt(stream, &cwd, &text, auto, move |ev| {
                let mut out = std::io::stdout().lock();
                match ev {
                    Event::Text(t) => {
                        let _ = out.write_all(t.as_bytes());
                        let _ = out.flush();
                    }
                    Event::Thought(t) if verbose => {
                        let _ = write!(out, "\x1b[2m{t}\x1b[0m");
                    }
                    Event::ToolCall { title, status, .. } => {
                        let _ = writeln!(out, "\n\x1b[36m⚙ {title} [{status}]\x1b[0m");
                    }
                    Event::ToolCallUpdate { id, status, title } if verbose => {
                        let _ = writeln!(out, "\x1b[36m  ↳ {id} {status:?} {title:?}\x1b[0m");
                    }
                    Event::PermissionRequest { title, reply, .. } => {
                        // unreachable with AllowOnce/Reject; answered here only for completeness
                        let _ = writeln!(out, "\n\x1b[33m? {title} → cancelled\x1b[0m");
                        let _ = reply.send(None);
                    }
                    Event::Other(s) if verbose => {
                        let _ = writeln!(out, "\x1b[2m· {s}\x1b[0m");
                    }
                    Event::Stopped(r) => {
                        let _ = writeln!(out, "\n\x1b[2m[stop: {r:?}]\x1b[0m");
                    }
                    Event::Ended(Some(e)) => {
                        let _ = writeln!(out, "\x1b[31m{e}\x1b[0m");
                    }
                    _ => {}
                }
            })?;
            Ok(())
        }
        DevCmd::Revert { path } => {
            let mut c = shim::Control::connect(&socket)?;
            let path = std::path::absolute(path)?;
            println!("{}: {}", path.display(), c.revert(&path)?);
            Ok(())
        }
        DevCmd::Stop => {
            if let Ok(mut c) = shim::Control::connect(&socket) {
                match c.kill(5000) {
                    Ok(code) => println!("agent stopped (exit {code:?})"),
                    Err(e) => eprintln!("agent: {e}"),
                }
            }
            let unit = format!("sill-{DEV}.service");
            if std::process::Command::new("systemctl").args(["--user", "is-active", "--quiet", &unit]).status().map(|s| s.success()).unwrap_or(false) {
                let st = std::process::Command::new("systemctl").args(["--user", "stop", &unit]).status()?;
                println!("unit {unit} stopped ({st})");
            }
            // legacy setsid sessions: bwrap ignores SIGTERM, so kill the whole process group it leads
            // (setsid made it a session leader; its pgid == its pid), which takes the shim with it
            for (name, file) in [("bwrap", "bwrap.pid"), ("proxy", "proxy.pid")] {
                if let Ok(pid) = std::fs::read_to_string(rt.join(file)) {
                    if let Ok(pid) = pid.trim().parse::<i32>()
                        && let Some(pid) = rustix::process::Pid::from_raw(pid)
                    {
                        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                        println!("{name} process group {} killed", pid.as_raw_nonzero());
                    }
                    let _ = std::fs::remove_file(rt.join(file));
                }
            }
            let _ = std::fs::remove_file(&socket);
            let _ = std::fs::remove_file(rt.join("proxy.sock"));
            Ok(())
        }
    }
}

/// `sill launch`: the process that owns a session on the host side, inside its systemd user unit.
fn launch(session: &str, jail: JailArgs, shim_bin: Option<PathBuf>) -> Result<()> {
    let rt = shim::runtime_dir(session)?;
    std::fs::create_dir_all(&rt)?;
    let shim_bin = match shim_bin {
        Some(p) => p,
        None => std::env::current_exe()?.with_file_name("sill-shim"),
    };
    if !shim_bin.is_file() {
        bail!("sill-shim not found at {}", shim_bin.display());
    }
    let use_proxy = !jail.net;
    let mut command: Vec<OsString> = vec!["/.sill/shim".into(), "--control-fd".into(), "3".into()];
    if use_proxy {
        command.extend(["--forward".into(), format!("127.0.0.1:{}={}", jail::PROXY_PORT, jail::PROXY_SOCKET).into()]);
    }
    let mut spec = build_spec(jail, command)?;
    let idle = spec.policy.limits.agent_idle_minutes as u64 * 60;
    spec.command.extend(["--agent-idle".into(), idle.to_string().into()]);
    spec.extra_args = vec!["--ro-bind".into(), shim_bin.into(), "/.sill/shim".into()];

    let shim_sock = rt.join("shim.sock");
    let proxy_sock = rt.join("proxy.sock");
    let _ = std::fs::remove_file(&shim_sock);
    let _ = std::fs::remove_file(&proxy_sock);
    let mut proxy_thread = None;
    if use_proxy {
        let listener = UnixListener::bind(&proxy_sock).with_context(|| format!("bind {}", proxy_sock.display()))?;
        let rules = sill_core::proxy::Rules::from(&spec.policy.network);
        eprintln!("sill launch: proxy mode {:?}, {} allow patterns", rules.mode, rules.allow.len());
        proxy_thread = Some(std::thread::spawn(move || sill_core::proxy::serve(listener, rules, |l| eprintln!("proxy: {l}"))));
        spec.proxy_socket = Some(proxy_sock);
    }
    let plan = spec.plan()?;
    for w in &plan.warnings {
        eprintln!("sill launch: warning: {w}");
    }
    jail::prepare(&plan)?;
    std::fs::write(rt.join("cwd"), spec.cwd.to_string_lossy().as_bytes())?;
    std::fs::write(rt.join("state_dir"), spec.state_dir.to_string_lossy().as_bytes())?;

    let listener = UnixListener::bind(&shim_sock).with_context(|| format!("bind {}", shim_sock.display()))?;
    let listener_fd = listener.as_fd().as_raw_fd();
    let mut cmd = std::process::Command::new(&plan.argv[0]);
    cmd.args(&plan.argv[1..]).stdin(std::process::Stdio::null());
    // SAFETY: only dup2/fcntl between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let src = std::os::fd::BorrowedFd::borrow_raw(listener_fd);
            if listener_fd == 3 {
                rustix::io::fcntl_setfd(src, rustix::io::FdFlags::empty())?;
            } else {
                let mut fd3 = OwnedFd::from_raw_fd(3);
                rustix::io::dup2(src, &mut fd3)?;
                std::mem::forget(fd3);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("spawn bwrap")?;
    drop(listener);
    eprintln!("sill launch: jail started (bwrap pid {}), sockets in {}", child.id(), rt.display());
    let status = child.wait()?;
    eprintln!("sill launch: jail ended ({status})");
    let _ = std::fs::remove_file(&shim_sock);
    let _ = std::fs::remove_file(rt.join("proxy.sock"));
    drop(proxy_thread); // the proxy thread dies with the process
    Ok(())
}

/// Start a session as a transient systemd user unit (sill-core `session`).
fn start_unit(session: &str, jail: JailArgs, shim_bin: Option<PathBuf>) -> Result<()> {
    let opts = sill_core::session::StartOptions {
        session: session.to_string(),
        policy: jail.policy.clone(),
        cwd: match jail.cwd { Some(c) => c, None => std::env::current_dir()? },
        state_dir: jail.state_dir.clone(),
        unsafe_state: jail.unsafe_state,
        share_net: jail.net,
        env: jail.env.clone(),
        shim_bin,
    };
    let sock = sill_core::session::ensure_started(&opts, &home()?, &std::env::current_exe()?)?;
    println!("session {session} started as unit {}.service; socket {}", sill_core::session::unit_name(session), sock.display());
    println!("logs: journalctl --user -u {} -f", sill_core::session::unit_name(session));
    Ok(())
}

/// Spawn `cmd` in its own session with `listener_fd` on fd 3 (close-on-exec cleared) and stdout/stderr
/// on `log`. Used for the jail and the proxy, both of which must outlive this process.
fn spawn_detached(mut cmd: std::process::Command, listener_fd: i32, log: std::fs::File) -> Result<std::process::Child> {
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: only async-signal-safe calls (dup2/fcntl, setsid) between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let src = std::os::fd::BorrowedFd::borrow_raw(listener_fd);
            if listener_fd == 3 {
                // dup2 onto itself keeps close-on-exec; clear it instead
                rustix::io::fcntl_setfd(src, rustix::io::FdFlags::empty())?;
            } else {
                let mut fd3 = OwnedFd::from_raw_fd(3);
                rustix::io::dup2(src, &mut fd3)?; // dup2 clears close-on-exec on the target
                std::mem::forget(fd3);
            }
            rustix::process::setsid()?;
            Ok(())
        });
    }
    cmd.spawn().context("spawn")
}

fn shell_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

// --- shared --------------------------------------------------------------------------------------

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME not set")?))
}

fn build_spec(a: JailArgs, command: Vec<OsString>) -> Result<JailSpec> {
    let home = home()?;
    let uid = rustix::process::getuid().as_raw();
    let user = std::env::var("USER").unwrap_or_else(|_| uid.to_string());
    let policy = Policy::load(&a.policy, &home)?;
    let state_dir = match a.state_dir {
        Some(d) => d,
        None => default_state_dir(uid, &home)?,
    };
    let mut env = BTreeMap::new();
    for kv in &a.env {
        let Some((k, v)) = kv.split_once('=') else { bail!("--env expects KEY=VALUE, got {kv}") };
        env.insert(k.to_string(), v.to_string());
    }
    let cwd = match a.cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };
    let command = if command.is_empty() {
        vec![OsString::from(std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())), "-l".into()]
    } else {
        command
    };
    let mut extra_args = vec![];
    if let Some(shim) = &a.shim_bin {
        extra_args.extend(["--ro-bind".into(), shim.clone().into(), "/.sill/shim".into()]);
    }
    Ok(JailSpec {
        policy,
        home,
        user,
        uid,
        state_dir: std::path::absolute(state_dir)?,
        unsafe_state: a.unsafe_state,
        cwd: std::path::absolute(cwd)?,
        env,
        share_net: a.net,
        mounts: mounts::host_mounts()?,
        root_entries: jail::read_root_entries()?,
        proxy_socket: a.proxy.map(std::path::absolute).transpose()?,
        proxy_port: jail::PROXY_PORT,
        extra_args,
        command,
    })
}

/// `/var/lib/sill/<uid>/sessions/dev` when the sill state filesystem is mounted, otherwise the user's
/// state dir (which `plan()` refuses without `--unsafe-state`).
fn default_state_dir(uid: u32, home: &Path) -> Result<PathBuf> {
    let state_fs = Path::new(jail::STATE_FS);
    let all = mounts::host_mounts()?;
    if mounts::is_mount_point(&all, state_fs) {
        // the state fs is sticky 1777 (like /tmp): each user owns a private 0700 dir in it
        let mine = state_fs.join(uid.to_string());
        if !mine.is_dir() {
            std::fs::create_dir(&mine).with_context(|| format!("create {}", mine.display()))?;
        }
        std::fs::set_permissions(&mine, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        return Ok(mine.join("sessions/dev"));
    }
    let xdg = std::env::var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/state"));
    Ok(xdg.join("sill/sessions/dev"))
}

use std::os::fd::AsRawFd;
