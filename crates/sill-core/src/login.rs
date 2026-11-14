//! Host-side agent login (SPEC §4.1 agents). Some logins open a browser and wait on a localhost
//! callback (Codex's ChatGPT flow); inside the jail neither the browser nor the callback port is
//! reachable, and the adapter does not hand out the URL. So the agent is started on the host for the
//! login only: `initialize` → `authenticate` over its stdio, nothing else. Its home is relocated
//! (`home_env`, `CODEX_HOME`) to a directory made fresh for the login — not the jail's copy: that
//! one is the agent's to write, and a config planted there (an MCP server command, a hook) would run
//! on the host through this very process (security.md S4). What the login writes is copied into
//! the jail's copy afterwards, files only and never through a link, so the host never gets a
//! `~/.codex` or `~/.gemini`; an agent without `home_env` writes the host file named in
//! `shared_files`, which is bound in. Either way the session starts again.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::agents::Agent;

/// How long the person gets to finish the browser flow.
const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What a login reports while it runs.
#[derive(Debug, Clone)]
pub enum Line {
    /// Worth showing: sill's own steps and the agent's auth status notifications.
    Status(String),
    /// The agent's stderr: noise unless the login fails.
    Stderr(String),
}

/// Run `method` of `agent` on the host; `report` gets progress lines. Blocking.
pub fn run(agent: &Agent, home: &Path, method: &str, mut report: impl FnMut(Line)) -> Result<()> {
    let Some(prog) = agent.command.first() else { bail!("agent {} has no command", agent.id) };
    let mut env = agent.env.clone();
    let mut unset: Vec<&str> = Vec::new();
    // the login's own home: empty before, copied into the jail's copy after, removed then
    let scratch = crate::session::agent_home(home, &agent.id)
        .parent()
        .and_then(Path::parent)
        .map(|state| state.join("login").join(&agent.id))
        .context("state dir")?;
    let _ = std::fs::remove_dir_all(&scratch);
    let mut copy_back: Option<PathBuf> = None;
    match (&agent.home_env, agent.home_paths.first()) {
        (Some(var), Some(first)) => {
            let name = Path::new(first).file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let owned = scratch.join(name);
            std::fs::create_dir_all(&owned).with_context(|| format!("create {}", owned.display()))?;
            let target = if agent.home_env_parent { &scratch } else { &owned };
            env.insert(var.clone(), target.display().to_string());
            copy_back = Some(crate::session::agent_home(home, &agent.id));
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
            // browser it opens must NOT inherit that: an `xdg-open` shim first on PATH (and
            // `$BROWSER`) restores the real home and XDG dirs, so it is your browser, your profile.
            // Only `$HOME` moves; the XDG dirs stay real, so a browser the agent launches by any
            // route (xdg-open, a bundled opener, Python's webbrowser) finds its own profile. The
            // price: an agent keeping its login under `$XDG_CONFIG_HOME` writes it to the host.
            std::fs::create_dir_all(&scratch)?;
            let shim_dir = browser_shim(home)?;
            let path = std::env::var("PATH").unwrap_or_default();
            env.insert("PATH".into(), format!("{}:{path}", shim_dir.display()));
            env.insert("BROWSER".into(), shim_dir.join("xdg-open").display().to_string());
            env.insert("HOME".into(), scratch.display().to_string());
            copy_back = Some(crate::session::agent_home(home, &agent.id));
            // no keyring for the login (the "fail" backend: every call raises, the "null" one silently
            // swallows writes): a token kept in the desktop's secret service would be
            // invisible inside the jail, so the agent must fall back to its file (Vibe: ~/.vibe/.env)
            unset.push("DBUS_SESSION_BUS_ADDRESS");
            env.insert("PYTHON_KEYRING_BACKEND".into(), "keyring.backends.fail.Keyring".into());
        }
    }
    let mut cmd = Command::new(prog);
    cmd.args(&agent.command[1..]).envs(&env).current_dir(home);
    for v in &unset {
        cmd.env_remove(v);
    }
    let mut child = cmd
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
        report(Line::Status(format!("logging in with {method}…")));
        let auth = json!({ "jsonrpc": "2.0", "id": 2, "method": "authenticate", "params": { "methodId": method } });
        writeln!(stdin, "{auth}")?;
        wait_response(&rx, &erx, 2, TIMEOUT, &mut report)?;
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    if let (Ok(()), Some(dst)) = (&result, copy_back) {
        std::fs::create_dir_all(&dst)?;
        let n = crate::safe_fs::copy_files_beneath(&scratch, &dst)?;
        report(Line::Status(format!("{n} login file(s) copied into the session's home")));
    }
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// The result object of response `id`; notifications on the way are reported.
fn wait_response(
    rx: &std::sync::mpsc::Receiver<Value>,
    erx: &std::sync::mpsc::Receiver<String>,
    id: u64,
    timeout: Duration,
    report: &mut impl FnMut(Line),
) -> Result<Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        for line in erx.try_iter() {
            report(Line::Stderr(line));
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
                    report(Line::Status(label));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("the agent exited during login"),
        }
    }
}

/// A directory with an `xdg-open` that runs the real one in the person's own environment, for
/// logins whose process runs with a relocated home.
fn browser_shim(home: &Path) -> Result<PathBuf> {
    let dir = crate::registry::tools_dir(home).join(".browser-shim");
    std::fs::create_dir_all(&dir)?;
    let real = |var: &str, fallback: &str| std::env::var(var).unwrap_or_else(|_| home.join(fallback).display().to_string());
    let script = format!(
        "#!/bin/sh\n# sill: open in the person's browser, not the agent's relocated home\nexport HOME='{home}'\nexport XDG_CONFIG_HOME='{cfg}'\nexport XDG_DATA_HOME='{data}'\nexport XDG_STATE_HOME='{state}'\nexport XDG_CACHE_HOME='{cache}'\n{dbus}unset BROWSER\nexec /usr/bin/xdg-open \"$@\"\n",
        home = home.display(),
        dbus = std::env::var("DBUS_SESSION_BUS_ADDRESS").map(|d| format!("export DBUS_SESSION_BUS_ADDRESS='{d}'\n")).unwrap_or_default(),
        cfg = real("XDG_CONFIG_HOME", ".config"),
        data = real("XDG_DATA_HOME", ".local/share"),
        state = real("XDG_STATE_HOME", ".local/state"),
        cache = real("XDG_CACHE_HOME", ".cache"),
    );
    let p = dir.join("xdg-open");
    if std::fs::read_to_string(&p).ok().as_deref() != Some(script.as_str()) {
        std::fs::write(&p, script)?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}
