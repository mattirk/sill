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

#[derive(Debug, Deserialize)]
pub struct Status {
    pub running: bool,
    pub pid: Option<u32>,
    pub exit: Option<i32>,
    pub buffered_lines: u64,
}

fn connect(socket: &Path, role: &str) -> Result<UnixStream> {
    let mut s = UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    s.write_all(format!("{}\n", json!({ "role": role })).as_bytes())?;
    Ok(s)
}

impl Control {
    pub fn connect(socket: &Path) -> Result<Self> {
        let s = connect(socket, "control")?;
        Ok(Self { reader: BufReader::new(s.try_clone()?), writer: s })
    }

    fn call(&mut self, req: serde_json::Value) -> Result<serde_json::Map<String, serde_json::Value>> {
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            bail!("shim closed the control connection");
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

    pub fn write_file(&mut self, path: &Path, content: &str) -> Result<()> {
        self.call(json!({ "op": "write_file", "path": path, "content": content }))?;
        Ok(())
    }
}

/// Open the agent's ACP byte stream (lines in, lines out).
pub fn agent_stream(socket: &Path) -> Result<UnixStream> {
    connect(socket, "agent")
}

/// Where a session's sockets live on the host.
pub fn runtime_dir(session: &str) -> Result<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())));
    Ok(base.join("sill").join(session))
}
