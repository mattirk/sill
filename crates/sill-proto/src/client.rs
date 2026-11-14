//! A blocking client: one connection, requests answered in order of arrival by id, notifications
//! handed out on a channel. What the CLI uses; the shelf wraps the same thing in a GLib channel.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::rpc::{Error, Incoming, Request};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, Error>>>>>;

/// The longest any request legitimately takes (an attach that starts a jail and an agent on a
/// slow disk), with room; past it the daemon is stuck and the person is told so.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// For `daemon.status` and `shutdown`: answered at once by a daemon that answers at all.
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(5);
/// A bare daemon's `daemon.log` is rotated to `.1` past this when the next one starts.
const LOG_KEEP: u64 = 8 * 1024 * 1024;

pub struct Client {
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: Pending,
    /// `(method, params)` of every notification the daemon sent.
    pub notifications: Mutex<Receiver<(String, Value)>>,
}

impl Client {
    /// Connect to the running daemon; an error when there is none.
    pub fn connect() -> Result<Client> {
        let path = crate::socket_path()?;
        let stream = UnixStream::connect(&path).with_context(|| format!("sill daemon at {}", path.display()))?;
        Self::over(stream)
    }

    /// Connect, starting the daemon when nothing answers: the systemd socket starts it by itself
    /// where the units are installed; otherwise `sill daemon` is started — as a transient user unit
    /// with `Delegate=yes` where the user manager answers, so sessions get their cgroups, else
    /// spawned detached — and the socket waited for (direct mode, docs/daemon.md §3.1).
    pub fn connect_or_start() -> Result<Client> {
        match Self::connect() {
            Ok(c) => match c.protocol()? {
                p if p == crate::PROTOCOL => return Ok(c),
                p => {
                    // an older (or newer) binary's daemon, left running across an upgrade: it is
                    // asked to leave and comes back as this binary — the socket unit starts the
                    // installed one on the next connection, else it is started below
                    eprintln!("sill: the daemon speaks protocol {p}, this binary {}: restarting it", crate::PROTOCOL);
                    let _ = c.call_within::<Value>(crate::method::DAEMON_SHUTDOWN, crate::Empty::default(), QUICK_TIMEOUT);
                    drop(c);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(100));
                        match Self::connect() {
                            Ok(c) if c.protocol().ok() == Some(crate::PROTOCOL) => return Ok(c),
                            Ok(_) => continue, // the old one is still leaving
                            Err(_) => break,   // gone: start one
                        }
                    }
                }
            },
            Err(e) => {
                if std::env::var_os("SILL_NO_DAEMON_START").is_some() {
                    return Err(e);
                }
                // a jail has its own tmpfs /run and no host socket: a daemon started here would
                // die at once on the hidden sill.db and leave a dead socket behind
                if std::env::var_os("SILL_SESSION").is_some() {
                    bail!("no sill daemon is reachable from inside a session");
                }
            }
        }
        let dir = crate::runtime_dir()?;
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let exe = std::env::current_exe()?;
        // `sill-daemon.service`, transient, its log in the journal and its cgroup its own (a bare
        // child of a shelf would live — and die — in the shelf's unit); `systemctl --user stop
        // sill-daemon` ends it and sessions stay, they are scopes of their own. Where that fails —
        // no user manager, or a unit of that name still loaded — the daemon is spawned bare.
        // the installed units exist but gave up (docs/daemon.md §3.7: the start limit after
        // repeated failures stops sill.socket too): starting a transient daemon beside them would
        // hide why — the person is sent to the journal instead
        if let Some(failed) = failed_unit() {
            bail!("{failed} failed and systemd stopped trying; `journalctl --user -u sill` has why, then `systemctl --user reset-failed sill.socket sill.service && systemctl --user start sill.socket`");
        }
        let mut why_bare = String::from("no systemd user instance");
        let unit_started = systemd_user_usable() && {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "reset-failed", "--quiet", "sill-daemon.service"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            match std::process::Command::new("systemd-run")
                .args(["--user", "--unit", "sill-daemon", "--collect", "--quiet", "-p", "Slice=app.slice"])
                .args(["--setenv=HOME", "--setenv=USER", "--setenv=TERM", "--setenv=LANG"])
                .arg("--")
                .arg(&exe)
                .arg("daemon")
                .stdin(std::process::Stdio::null())
                .output()
            {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    why_bare = format!("systemd-run: {}", String::from_utf8_lossy(&o.stderr).trim());
                    false
                }
                Err(e) => {
                    why_bare = format!("systemd-run: {e}");
                    false
                }
            }
        };
        if !unit_started {
            // bare mode has no journal: the log is a file, kept to one generation of LOG_KEEP
            let log_path = dir.join("daemon.log");
            if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > LOG_KEEP) {
                let _ = std::fs::rename(&log_path, dir.join("daemon.log.1"));
            }
            let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
            // the person sees why in the daemon's own log, not on a CLI command's stderr
            let _ = std::io::Write::write_all(&mut log, format!("sill client: starting the daemon bare ({why_bare})\n").as_bytes());
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
            // its own session: no terminal hangup reaches it, and it is nobody's job
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                    let _ = rustix::process::setsid();
                    Ok(())
                });
            }
            cmd.spawn().with_context(|| format!("starting {} daemon", exe.display()))?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Self::connect() {
                Ok(c) => return Ok(c),
                Err(e) if Instant::now() >= deadline => {
                    bail!("the daemon did not come up in 5 s ({e:#}); its log is {}", dir.join("daemon.log").display())
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// The protocol the daemon on this connection speaks (`daemon.status`).
    fn protocol(&self) -> Result<u32> {
        let s: crate::DaemonStatus = self.call_within(crate::method::DAEMON_STATUS, crate::Empty::default(), QUICK_TIMEOUT)?;
        Ok(s.protocol)
    }

    /// The connection is gone (the daemon left or restarted): the caller connects again.
    pub fn is_closed(e: &anyhow::Error) -> bool {
        e.to_string().contains("closed the connection") || e.to_string().contains("went away") || e.to_string().contains("writing to the daemon")
    }

    pub fn over(stream: UnixStream) -> Result<Client> {
        let reader = stream.try_clone()?;
        let pending: Pending = Arc::default();
        let (ntx, nrx) = channel();
        let p2 = pending.clone();
        std::thread::Builder::new().name("sill-proto-reader".into()).spawn(move || {
            let mut lines = BufReader::new(reader).lines();
            while let Some(Ok(line)) = lines.next() {
                let Ok(msg) = serde_json::from_str::<Incoming>(&line) else { continue };
                match (msg.id, msg.method) {
                    (Some(id), None) => {
                        let tx = p2.lock().unwrap().remove(&id);
                        if let Some(tx) = tx {
                            let _ = tx.send(match msg.error {
                                Some(e) => Err(e),
                                None => Ok(msg.result.unwrap_or(Value::Null)),
                            });
                        }
                    }
                    (None, Some(method)) => {
                        let _ = ntx.send((method, msg.params.unwrap_or(Value::Null)));
                    }
                    // a request from the daemon (a permission to answer, M1): not yet
                    _ => {}
                }
            }
            // the connection is gone: every waiter learns it
            let mut p = p2.lock().unwrap();
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(Error::new(Error::INTERNAL, "the daemon closed the connection")));
            }
        })?;
        Ok(Client { writer: Mutex::new(stream), next_id: AtomicU64::new(1), pending, notifications: Mutex::new(nrx) })
    }

    /// One request, its answer — within `CALL_TIMEOUT`: an attach waits for a jail to start and an
    /// end for the agent's summary, nothing waits longer than this, and a daemon that does not
    /// answer at all must not hang its client forever (docs/daemon.md §3.7).
    pub fn call<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<T> {
        self.call_within(method, params, CALL_TIMEOUT)
    }

    /// `call` with a timeout of its own: short for `daemon.status` and `shutdown`, which answer
    /// at once or not at all.
    pub fn call_within<T: DeserializeOwned>(&self, method: &str, params: impl Serialize, timeout: Duration) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        let req = Request::new(Some(id), method, serde_json::to_value(params)?);
        {
            let mut w = self.writer.lock().unwrap();
            let mut line = serde_json::to_string(&req)?;
            line.push('\n');
            w.write_all(line.as_bytes()).context("writing to the daemon")?;
        }
        let v = match rx.recv_timeout(timeout) {
            Ok(v) => v.map_err(|e| anyhow::anyhow!("{method}: {e}"))?,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("the daemon went away"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("the daemon did not answer {method} in {} s: it is stuck; `sill restart` (or `systemctl --user restart sill`) starts a fresh one, sessions survive it", timeout.as_secs())
            }
        };
        serde_json::from_value(v).with_context(|| format!("{method}: unexpected answer shape"))
    }
}

/// The first of `sill.socket`, `sill.service` in the failed state, if any (`is-failed` exits 0
/// for exactly that; a unit that is not installed is "inactive").
fn failed_unit() -> Option<&'static str> {
    ["sill.socket", "sill.service"].into_iter().find(|u| {
        std::process::Command::new("systemctl")
            .args(["--user", "is-failed", "--quiet", u])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// `systemd-run --user` works here: `$XDG_RUNTIME_DIR` set, systemd the init, the user manager answering.
fn systemd_user_usable() -> bool {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() || !std::path::Path::new("/run/systemd/system").is_dir() {
        return false;
    }
    std::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| matches!(String::from_utf8_lossy(&o.stdout).trim(), "running" | "degraded" | "starting" | "initializing" | "maintenance"))
        .unwrap_or(false)
}
