//! Host-side agent login (SPEC §4.1 agents). Some logins open a browser and wait on a localhost
//! callback (Codex's ChatGPT flow); inside the jail neither the browser nor the callback port is
//! reachable, and the adapter does not hand out the URL. So the agent is started on the host for the
//! login only: `initialize` → `authenticate` over its stdio, nothing else. Its home is relocated
//! (`home_env`, `CODEX_HOME`) to a directory made fresh for the login — not the jail's copy: that
//! one is the agent's to write, and a config planted there (an MCP server command, a hook) would run
//! on the host through this very process (security.md S4). What the login writes is copied into
//! the jail's copy afterwards — after the adapter's own helpers have stopped, and never the
//! databases, logs or temp files a fresh home creates around the login (they would replace the
//! agent's history) — files only and never through a link, so the host never gets a
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
pub fn run(agent: &Agent, home: &Path, method: &str, report: impl FnMut(Line)) -> Result<()> {
    run_into(agent, home, &[], method, report)
}

/// Run `method` of `agent` on the host, copying login files into its sill-owned home and each
/// existing directory in `extra_homes` (such as ghost sessions' copies of the agent home).
pub fn run_into(agent: &Agent, home: &Path, extra_homes: &[PathBuf], method: &str, mut report: impl FnMut(Line)) -> Result<()> {
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
    // what the login writes is a credential: the scratch is 0700 from `login/` down
    let private = |dir: &Path| -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("create {}", dir.display()))?;
        if let Some(parent) = dir.parent() {
            let _ = std::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        }
        Ok(())
    };
    let mut copy_back: Option<PathBuf> = None;
    match (&agent.home_env, agent.home_paths.first()) {
        (Some(var), Some(first)) => {
            let name = Path::new(first).file_name().map(|n| n.to_os_string()).unwrap_or_default();
            let owned = scratch.join(name);
            private(&owned)?;
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
            private(&scratch)?;
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
    stop_agent(&mut child, &crate::registry::tools_dir(home));
    let copied = match (&result, copy_back) {
        (Ok(()), Some(dst)) => (|| -> Result<usize> {
            std::fs::create_dir_all(&dst)?;
            let n = copy_login(&scratch, &dst)?;
            for extra in extra_homes {
                if extra.is_dir() {
                    crate::credentials::seed(agent, &dst, extra)?;
                }
            }
            Ok(n)
        })()
        .map(Some),
        _ => Ok(None),
    };
    // the scratch holds a credential: gone whatever happened
    let _ = std::fs::remove_dir_all(&scratch);
    result?;
    if let Some(n) = copied? {
        report(Line::Status(format!("{n} login file(s) copied into the session's home")));
    }
    Ok(())
}

/// End the login agent and the helpers it runs from sill's agent installs (Codex's `codex
/// app-server`), before the scratch is copied: one still running there deletes files under the
/// copy. Only helpers whose executable is under `tools`: a browser the login opened is the
/// person's and stays.
fn stop_agent(child: &mut std::process::Child, tools: &Path) {
    let helpers: Vec<i32> = descendants(child.id() as i32).into_iter()
        .filter(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe.starts_with(tools)))
        .collect();
    let _ = child.kill();
    let _ = child.wait();
    let signal = |sig| for pid in &helpers { if let Some(p) = rustix::process::Pid::from_raw(*pid) { let _ = rustix::process::kill_process(p, sig); } };
    signal(rustix::process::Signal::TERM);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && helpers.iter().any(|pid| Path::new(&format!("/proc/{pid}")).exists()) {
        std::thread::sleep(Duration::from_millis(50));
    }
    signal(rustix::process::Signal::KILL);
}

/// Every process below `root`, by parent links in `/proc`.
fn descendants(root: i32) -> Vec<i32> {
    let parents: Vec<(i32, i32)> = std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| {
        let pid: i32 = e.file_name().to_str()?.parse().ok()?;
        let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
        // `pid (comm) state ppid …`; comm may hold spaces and parentheses
        let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
        Some((pid, ppid))
    }).collect();
    let mut out = vec![];
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, _) in parents.iter().filter(|(_, ppid)| *ppid == parent) {
            if !out.contains(pid) { out.push(*pid); frontier.push(*pid); }
        }
    }
    out
}

/// What a login wrote that is the login: everything in the scratch except what a fresh agent
/// home creates around it — databases (copied over the agent's own they would replace its history
/// and state), logs, caches and temp files. A file that vanished meanwhile is skipped.
fn copy_login(scratch: &Path, dst: &Path) -> Result<usize> {
    const SKIP_DIRS: &[&str] = &["tmp", ".tmp", "log", "logs", "cache", ".cache", "sessions", "shell_snapshots", "thread-writer-locks", "plugins", "skills"];
    fn skipped_file(name: &str) -> bool {
        [".sqlite", ".sqlite-wal", ".sqlite-shm", ".sqlite-journal", ".db", ".db-wal", ".db-shm", ".lock", ".log"].iter().any(|s| name.ends_with(s))
    }
    fn walk(base: &Path, dir: &Path, dst: &Path, n: &mut usize) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
        let entries = match std::fs::read_dir(dir) { Ok(e) => e, Err(e) if gone(&e) => return Ok(()), Err(e) => return Err(e.into()) };
        for e in entries {
            let e = match e { Ok(e) => e, Err(e) if gone(&e) => continue, Err(e) => return Err(e.into()) };
            let (p, name) = (e.path(), e.file_name().to_string_lossy().into_owned());
            let md = match std::fs::symlink_metadata(&p) { Ok(m) => m, Err(e) if gone(&e) => continue, Err(e) => return Err(e.into()) };
            if md.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) { walk(base, &p, dst, n)?; }
            } else if md.is_file() && !skipped_file(&name) {
                let bytes = match std::fs::read(&p) { Ok(b) => b, Err(e) if gone(&e) => continue, Err(e) => return Err(e.into()) };
                let rel = p.strip_prefix(base).unwrap_or(&p);
                crate::safe_fs::write_file_beneath(dst, rel, &bytes, md.permissions().mode() & 0o777)?;
                *n += 1;
            }
        }
        Ok(())
    }
    let mut n = 0;
    walk(scratch, scratch, dst, &mut n)?;
    Ok(n)
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

#[cfg(test)]
mod login_copy_tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sill-login-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_login_copies_the_credential_and_never_a_fresh_homes_databases() {
        let (scratch, dst) = (dir("scratch"), dir("dst"));
        for (f, t) in [(".codex/auth.json", "login"), (".codex/config.toml", "x"), (".codex/state_5.sqlite", "fresh"), (".codex/state_5.sqlite-wal", "fresh"),
                       (".codex/logs_2.sqlite", "fresh"), (".codex/.tmp/plugins/README.md", "p"), (".codex/tmp/arg0/x", "a"), (".codex/log/codex.log", "l")] {
            std::fs::create_dir_all(scratch.join(f).parent().unwrap()).unwrap();
            std::fs::write(scratch.join(f), t).unwrap();
        }
        std::fs::create_dir_all(dst.join(".codex")).unwrap();
        std::fs::write(dst.join(".codex/state_5.sqlite"), "the agent's history").unwrap();
        assert_eq!(copy_login(&scratch, &dst).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(dst.join(".codex/auth.json")).unwrap(), "login");
        assert_eq!(std::fs::read_to_string(dst.join(".codex/state_5.sqlite")).unwrap(), "the agent's history");
        for gone in [".codex/state_5.sqlite-wal", ".codex/logs_2.sqlite", ".codex/.tmp", ".codex/tmp", ".codex/log"] {
            assert!(!dst.join(gone).exists(), "{gone} copied");
        }
        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn stopping_the_login_ends_its_installed_helpers_but_not_other_programs() {
        // `sleep` stands in for both: once as a helper from the agent's install, once as a program
        // from elsewhere (the person's browser)
        let alive = |pid: i32| Path::new(&format!("/proc/{pid}")).exists() && !std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default().contains(") Z ");
        for (tools, survives) in [(PathBuf::from("/usr"), false), (dir("tools"), true)] {
            let mut child = Command::new("sh").args(["-c", "sleep 30 & wait"]).spawn().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let helper = loop {
                if let Some(pid) = descendants(child.id() as i32).first() { break *pid; }
                assert!(std::time::Instant::now() < deadline, "helper never started");
                std::thread::sleep(Duration::from_millis(20));
            };
            stop_agent(&mut child, &tools);
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(alive(helper), survives, "tools {}", tools.display());
            if survives { let _ = rustix::process::kill_process(rustix::process::Pid::from_raw(helper).unwrap(), rustix::process::Signal::KILL); }
        }
    }
}
