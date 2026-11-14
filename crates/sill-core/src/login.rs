//! Host-side agent login (SPEC §4.1 agents). Some logins open a browser and wait on a localhost
//! callback (Codex's ChatGPT flow); inside the jail neither the browser nor the callback port is
//! reachable, and the adapter does not hand out the URL. So the agent is started on the host for the
//! login only: `initialize` → `authenticate` over its stdio, nothing else. Its home is relocated to
//! the sill-owned copy (`home_env`, `CODEX_HOME`), so the login file and everything else it writes
//! land in the jail's home and the host never gets a `~/.codex` or `~/.gemini`; an agent without `home_env` writes
//! the host file named in `shared_files`, which is bound in. Either way the session starts again.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::agents::Agent;

/// How long the person gets to finish the browser flow.
const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Run `method` of `agent` on the host; `report` gets progress lines (auth status notifications,
/// the agent's own stderr). Blocking.
pub fn run(agent: &Agent, home: &Path, method: &str, mut report: impl FnMut(String)) -> Result<()> {
    let Some(prog) = agent.command.first() else { bail!("agent {} has no command", agent.id) };
    let mut env = agent.env.clone();
    match (&agent.home_env, agent.home_paths.first()) {
        (Some(var), Some(first)) => {
            // the agent's home for this run is the jail's copy: nothing of it lands on the host
            let name = Path::new(first).file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let parent = crate::session::agent_home(home, &agent.id);
            let owned = parent.join(name);
            std::fs::create_dir_all(&owned).with_context(|| format!("create {}", owned.display()))?;
            let target = if agent.home_env_parent { &parent } else { &owned };
            env.insert(var.clone(), target.display().to_string());
        }
        _ if !agent.shared_files.is_empty() => {
            // the login file's directory may not exist yet on a host that never ran the agent
            for p in &agent.shared_files {
                let path = crate::paths::expand_home(p, home);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
        }
        _ => {
            // an agent sill knows nothing about: run its login with `$HOME` (and the XDG dirs) at
            // the sill-owned agent home, so whatever it writes — `~/.foo`, `~/.config/foo` — lands
            // there and is bound into its jails from then on (`session::adopted_binds`). The
            // desktop's file-association files are linked in so the browser it opens is yours.
            let base = crate::session::agent_home(home, &agent.id);
            for sub in [".config", ".local/share", ".local/state", ".cache"] {
                std::fs::create_dir_all(base.join(sub))?;
            }
            for rel in ["mimeapps.list"] {
                let src = home.join(".config").join(rel);
                let dst = base.join(".config").join(rel);
                if src.exists() && !dst.exists() {
                    let _ = std::os::unix::fs::symlink(&src, &dst);
                }
            }
            let apps_src = home.join(".local/share/applications");
            let apps_dst = base.join(".local/share/applications");
            if apps_src.is_dir() && !apps_dst.exists() {
                let _ = std::os::unix::fs::symlink(&apps_src, &apps_dst);
            }
            env.insert("HOME".into(), base.display().to_string());
            env.insert("XDG_CONFIG_HOME".into(), base.join(".config").display().to_string());
            env.insert("XDG_DATA_HOME".into(), base.join(".local/share").display().to_string());
            env.insert("XDG_STATE_HOME".into(), base.join(".local/state").display().to_string());
            env.insert("XDG_CACHE_HOME".into(), base.join(".cache").display().to_string());
        }
    }
    let mut child = Command::new(prog)
        .args(&agent.command[1..])
        .envs(&env)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {prog} on the host"))?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("stdout")?);
    let stderr = BufReader::new(child.stderr.take().context("stderr")?);

    // stdout lines → channel (so the wait has a deadline); stderr → report
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let (etx, erx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if etx.send(line).is_err() {
                break;
            }
        }
    });

    let result = (|| -> Result<()> {
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
            "clientInfo": { "name": "sill", "version": env!("CARGO_PKG_VERSION") },
        }});
        writeln!(stdin, "{init}")?;
        let resp = wait_response(&rx, &erx, 1, Duration::from_secs(30), &mut report)?;
        let methods: Vec<String> = resp
            .get("authMethods")
            .and_then(|m| m.as_array())
            .map(|a| a.iter().filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect())
            .unwrap_or_default();
        if !methods.is_empty() && !methods.iter().any(|m| m == method) {
            bail!("{prog} offers no login method {method:?} (it has: {})", methods.join(", "));
        }
        report(format!("logging in with {method}…"));
        let auth = json!({ "jsonrpc": "2.0", "id": 2, "method": "authenticate", "params": { "methodId": method } });
        writeln!(stdin, "{auth}")?;
        wait_response(&rx, &erx, 2, TIMEOUT, &mut report)?;
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// The result object of response `id`; notifications on the way are reported.
fn wait_response(
    rx: &std::sync::mpsc::Receiver<Value>,
    erx: &std::sync::mpsc::Receiver<String>,
    id: u64,
    timeout: Duration,
    report: &mut impl FnMut(String),
) -> Result<Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        for line in erx.try_iter() {
            report(line);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            bail!("no answer within {} s", timeout.as_secs());
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(500))) {
            Ok(v) => {
                if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                    if let Some(err) = v.get("error") {
                        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("error");
                        return Err(anyhow!("{msg}"));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                    // e.g. `_auth/status_update` {"authStatus":{"kind":"none","label":"Not logged in"}}
                    let label = v
                        .pointer("/params/authStatus/label")
                        .and_then(|l| l.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| m.to_string());
                    report(label);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("the agent exited during login"),
        }
    }
}
