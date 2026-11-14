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
            Ok(c) => return Ok(c),
            Err(e) => {
                if std::env::var_os("SILL_NO_DAEMON_START").is_some() {
                    return Err(e);
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
            let mut log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log"))?;
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

    /// One request, its answer.
    pub fn call<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<T> {
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
        let v = rx.recv().context("the daemon went away")?.map_err(|e| anyhow::anyhow!("{method}: {e}"))?;
        serde_json::from_value(v).with_context(|| format!("{method}: unexpected answer shape"))
    }
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
