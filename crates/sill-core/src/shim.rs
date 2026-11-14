//! Shelf side of the shim protocol (SPEC §4.3): connect to a session's control socket, drive the
//! agent process, open the agent's ACP byte stream.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

pub struct Control {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// A managed process as the shim reports it (docs/tasks.md §2.4).
#[derive(Debug, Clone, Deserialize)]
pub struct Proc {
    pub name: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub port: Option<u16>,
    pub pid: u32,
    /// Unix seconds.
    pub started: u64,
    /// `"running"` | `"stopped"` | `{"exited": code}`.
    pub state: serde_json::Value,
    pub last_line: String,
}

impl Proc {
    pub fn running(&self) -> bool {
        self.state.as_str() == Some("running")
    }
    /// The exit code of one that ended on its own.
    pub fn exit_code(&self) -> Option<i64> {
        self.state.get("exited").and_then(|v| v.as_i64())
    }
    pub fn failed(&self) -> bool {
        self.exit_code().is_some_and(|c| c != 0)
    }
}

#[derive(Debug, Deserialize)]
pub struct Status {
    pub running: bool,
    pub pid: Option<u32>,
    pub exit: Option<i32>,
    pub buffered_lines: u64,
    /// Frontends attached through the daemon, as it last said (None: never told).
    #[serde(default)]
    pub frontends: Option<u32>,
}

fn connect(socket: &Path, role: &str) -> Result<UnixStream> {
    let mut s = UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    s.write_all(format!("{}\n", json!({ "role": role })).as_bytes())?;
    Ok(s)
}

/// How long a control request may take before it is an error. The shelf calls these on its main
/// loop at startup; a shim that doesn't answer must not freeze the UI.
const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl Control {
    pub fn connect(socket: &Path) -> Result<Self> {
        let s = connect(socket, "control")?;
        s.set_read_timeout(Some(CONTROL_TIMEOUT))?;
        s.set_write_timeout(Some(CONTROL_TIMEOUT))?;
        Ok(Self { reader: BufReader::new(s.try_clone()?), writer: s })
    }

    fn call(&mut self, req: serde_json::Value) -> Result<serde_json::Map<String, serde_json::Value>> {
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut reply = String::new();
        match self.reader.read_line(&mut reply) {
            Ok(0) => bail!("shim closed the control connection"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                bail!("the session's shim did not answer within {} s", CONTROL_TIMEOUT.as_secs())
            }
            Err(e) => return Err(e.into()),
        }
        let reply: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(reply.trim()).context("shim reply")?;
        if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let msg = reply.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            bail!("shim: {msg}");
        }
        Ok(reply)
    }

    pub fn spawn(&mut self, argv: &[String], env: &BTreeMap<String, String>, cwd: Option<&Path>) -> Result<u32> {
        let r = self.call(json!({ "op": "spawn", "argv": argv, "env": env, "cwd": cwd }))?;
        Ok(r.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32)
    }

    pub fn status(&mut self) -> Result<Status> {
        let r = self.call(json!({ "op": "status" }))?;
        Ok(serde_json::from_value(serde_json::Value::Object(r))?)
    }

    /// Tell the shim how many frontends are attached through the daemon (its idle rule,
    /// docs/daemon.md §3.6).
    pub fn attached(&mut self, frontends: u32) -> Result<()> {
        self.call(json!({ "op": "attached", "frontends": frontends }))?;
        Ok(())
    }

    pub fn kill(&mut self, grace_ms: u64) -> Result<Option<i32>> {
        let r = self.call(json!({ "op": "kill", "grace_ms": grace_ms }))?;
        Ok(r.get("exit").and_then(|v| v.as_i64()).map(|v| v as i32))
    }

    pub fn revert(&mut self, path: &Path) -> Result<String> {
        let r = self.call(json!({ "op": "revert", "path": path }))?;
        Ok(r.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    pub fn read_file(&mut self, path: &Path) -> Result<String> {
        let r = self.call(json!({ "op": "read_file", "path": path }))?;
        Ok(r.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    /// Open a pty in the jail running `argv`; returns the master fd and the child's pid.
    pub fn open_pty(
        &mut self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&Path>,
        cols: u16,
        rows: u16,
    ) -> Result<(std::os::fd::OwnedFd, u32)> {
        use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
        let req = json!({ "op": "open_pty", "argv": argv, "env": env, "cwd": cwd, "cols": cols, "rows": rows });
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        // the reply rides with the fd: read it with recvmsg on the raw socket (nothing is buffered:
        // every earlier reply was exactly one line, consumed whole)
        let mut buf = [0u8; 4096];
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let r = recvmsg(&self.writer, &mut [std::io::IoSliceMut::new(&mut buf)], &mut control, RecvFlags::empty())?;
        let text = String::from_utf8_lossy(&buf[..r.bytes]).to_string();
        let reply: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(text.lines().next().unwrap_or("").trim()).context("shim reply")?;
        if reply.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            bail!("shim: {}", reply.get("error").and_then(|v| v.as_str()).unwrap_or("open_pty failed"));
        }
        let mut fd = None;
        for msg in control.drain() {
            if let RecvAncillaryMessage::ScmRights(fds) = msg {
                for f in fds {
                    fd = Some(f);
                }
            }
        }
        let fd = fd.context("no pty fd in the shim's reply")?;
        let pid = reply.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        Ok((fd, pid))
    }

    pub fn write_file(&mut self, path: &Path, content: &str) -> Result<()> {
        self.call(json!({ "op": "write_file", "path": path, "content": content }))?;
        Ok(())
    }

    // --- managed processes (docs/tasks.md §2.4)

    pub fn proc_list(&mut self) -> Result<Vec<Proc>> {
        let r = self.call(json!({ "op": "proc_list" }))?;
        Ok(serde_json::from_value(r.get("procs").cloned().unwrap_or_default())?)
    }

    pub fn proc_tail(&mut self, name: &str, lines: usize) -> Result<String> {
        let r = self.call(json!({ "op": "proc_tail", "name": name, "lines": lines }))?;
        Ok(r.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    pub fn proc_stop(&mut self, name: &str) -> Result<()> {
        self.call(json!({ "op": "proc_stop", "name": name }))?;
        Ok(())
    }

    pub fn proc_stop_all(&mut self) -> Result<usize> {
        let r = self.call(json!({ "op": "proc_stop_all" }))?;
        Ok(r.get("stopped").and_then(|v| v.as_u64()).unwrap_or(0) as usize)
    }

    pub fn proc_dismiss(&mut self, name: &str) -> Result<()> {
        self.call(json!({ "op": "proc_dismiss", "name": name }))?;
        Ok(())
    }
}

/// Open the agent's ACP byte stream (lines in, lines out).
pub fn agent_stream(socket: &Path) -> Result<UnixStream> {
    connect(socket, "agent")
}

/// A byte stream to `127.0.0.1:port` inside the jail, relayed by the shim — only for a port a
/// running managed process declared (docs/tasks.md §2.5). The shim answers one line first.
pub fn port_stream(socket: &Path, port: u16) -> Result<UnixStream> {
    let mut s = UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    s.write_all(format!("{}\n", json!({ "role": "port", "port": port })).as_bytes())?;
    let mut reader = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let reply: serde_json::Value = serde_json::from_str(line.trim()).context("shim reply")?;
    if reply["ok"].as_bool() != Some(true) {
        bail!("shim: {}", reply["error"].as_str().unwrap_or("port refused"));
    }
    Ok(s)
}

/// Where a session's sockets live on the host.
pub fn runtime_dir(session: &str) -> Result<PathBuf> {
    if !crate::session::valid_id(session) {
        anyhow::bail!("bad session id {session:?}");
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())));
    Ok(base.join("sill").join(session))
}
