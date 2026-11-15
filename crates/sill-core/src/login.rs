//! Jailed login uses a fresh private HOME and XDG tree. Only declared credential files
//! return to the profile; settings, history, hooks and incidental tool output never do.
//! The daemon never opens a browser: the browser helper hands each URL to the login's report
//! ([`Line::OpenUrl`]) and the frontend opens it where the person is (docs/login.md, "The browser
//! belongs to the frontend"). The methods that log in this way (`host_login`) must be declared.

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
    /// A page the login asked a browser to open: http(s), each once. `callback_port` is the
    /// daemon host's loopback port its `redirect_uri` names, which the browser must reach.
    OpenUrl { url: String, callback_port: Option<u16> },
}

/// The login was cancelled (`_sill/operations.cancel`); the agent and its helpers were stopped.
#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("the login was cancelled") }
}
impl std::error::Error for Cancelled {}

/// Never cancelled: for callers with no way to cancel.
pub fn never() -> bool { false }

/// Run `method` of `agent` in a login jail; `report` gets progress lines. Blocking.
pub fn run(agent: &Agent, home: &Path, method: &str, report: impl FnMut(Line)) -> Result<()> {
    run_into(agent, home, &[], method, report)
}

/// Run `method` of `agent` in a login jail, copying login files into its sill-owned home and each
/// existing directory in `extra_homes` (such as ghost sessions' copies of the agent home).
pub fn run_into(agent: &Agent, home: &Path, extra_homes: &[(PathBuf, crate::credentials::Rules)], method: &str, report: impl FnMut(Line)) -> Result<()> {
    run_cancellable(agent, home, extra_homes, method, &never, report)
}

/// [`run_into`], ended with [`Cancelled`] once `cancelled` says so.
pub fn run_cancellable(agent: &Agent, home: &Path, extra_homes: &[(PathBuf, crate::credentials::Rules)], method: &str, cancelled: &dyn Fn() -> bool, mut report: impl FnMut(Line)) -> Result<()> {
    let paths = agent.ready(home)?;
    anyhow::ensure!(agent.host_login.iter().any(|m| m == method), "host login method is not explicitly declared");
    let scratch = scratch_dir(home, &agent.id)?;
    let result = login_in(agent, home, &scratch, paths, method, cancelled, &mut report);
    let copied = match &result {
        Ok(()) => (|| -> Result<usize> {
            let dst = crate::session::agent_home(home, &agent.id);
            std::fs::create_dir_all(&dst)?;
            crate::credentials::seed(&crate::credentials::Rules::for_agent(agent)?, &scratch, &dst)?;
            let n = crate::credentials::files(agent, &scratch)?.iter().filter(|p| scratch.join(p).is_file()).count();
            for (extra, frozen_rules) in extra_homes {
                if extra.is_dir() {
                    crate::credentials::seed(frozen_rules, &dst, extra)?;
                }
            }
            Ok(n)
        })()
        .map(Some),
        Err(_) => Ok(None),
    };
    // the scratch holds a credential: gone whatever happened
    let _ = std::fs::remove_dir_all(&scratch);
    result?;
    if let Some(n) = copied? {
        report(Line::Status(format!("{n} login file(s) copied into the session's home")));
    }
    Ok(())
}

/// Set up an agent that has no `home_paths` yet by logging in: `method` runs in a login jail with an
/// empty home, as [`run_into`] does, and what the login wrote there is the proposal for the
/// agent's directories and credential files (docs/agent-directories.md, "Directory setup by
/// probe"). The proposed directories are copied whole into the agent's sill-owned home, so the
/// login holds once the person applies the proposal; nothing is declared here.
pub fn setup(agent: &Agent, home: &Path, method: &str, report: impl FnMut(Line)) -> Result<crate::registry::Discovery> {
    setup_cancellable(agent, home, method, &never, report)
}

/// [`setup`], ended with [`Cancelled`] once `cancelled` says so.
pub fn setup_cancellable(agent: &Agent, home: &Path, method: &str, cancelled: &dyn Fn() -> bool, mut report: impl FnMut(Line)) -> Result<crate::registry::Discovery> {
    anyhow::ensure!(agent.home_paths.is_none(), "agent {} is set up already", agent.id);
    let scratch = scratch_dir(home, &agent.id)?;
    let result = login_in(agent, home, &scratch, &[], method, cancelled, &mut report).and_then(|()| {
        let proposals = crate::registry::proposals_from(&scratch, home);
        anyhow::ensure!(!proposals.is_empty(), "the login succeeded but wrote nothing under the home; there is nothing to keep");
        let login_files = crate::registry::credential_candidates(&scratch, &proposals);
        let dst = crate::session::agent_home(home, &agent.id);
        for p in &proposals {
            let rel = p.path.trim_start_matches("~/");
            copy_private(&scratch.join(rel), &dst.join(rel)).with_context(|| format!("keep {}", p.path))?;
            report(Line::Status(format!("kept {}{}", p.path, if p.kind == crate::agents::HomePathKind::Directory { "/" } else { "" })));
        }
        for f in &login_files {
            report(Line::Status(format!("login file: ~/{f}")));
        }
        let earlier = crate::registry::discovered(home, &agent.id);
        let discovery = crate::registry::Discovery {
            at: crate::registry::now_secs(),
            version: crate::registry::installed(home, &agent.id).map(|i| i.version).unwrap_or_default(),
            proposals,
            auth_methods: earlier.map(|d| d.auth_methods).unwrap_or_default(),
            login_method: Some(method.to_string()),
            login_files,
            note: None,
        };
        crate::registry::record_discovery(home, &agent.id, &discovery)?;
        Ok(discovery)
    });
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// The login's own home: empty before, removed after. 0700 from `login/` down: what the login
/// writes is a credential.
fn scratch_dir(home: &Path, id: &str) -> Result<PathBuf> {
    let scratch = crate::session::agent_home(home, id)
        .parent()
        .and_then(Path::parent)
        .map(|state| state.join("login").join(id))
        .context("state dir")?;
    let _ = std::fs::remove_dir_all(&scratch);
    private(&scratch)?;
    Ok(scratch)
}

fn private(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("create {}", dir.display()))?;
    if let Some(parent) = dir.parent() {
        let _ = std::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    Ok(())
}

/// Copy a login's directory or file into the agent's home, private, refusing symlinks and special
/// files. An existing file is replaced; other files already there stay.
fn copy_private(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        bail!("{} is a symlink", src.display());
    }
    if meta.is_dir() {
        private(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_private(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else if meta.is_file() {
        if let Some(parent) = dst.parent() {
            private(parent)?;
        }
        let bytes = std::fs::read(src)?;
        crate::safe_fs::replace_private_beneath(dst.parent().context("parent")?, Path::new(dst.file_name().context("name")?), &bytes)?;
        Ok(())
    } else {
        bail!("{} is not a file or directory", src.display())
    }
}

/// Start the agent in a jail with `scratch` as its home and run `method`'s `authenticate`.
fn login_in(agent: &Agent, home: &Path, scratch: &Path, paths: &[crate::agents::HomePath], method: &str, cancelled: &dyn Fn() -> bool, report: &mut impl FnMut(Line)) -> Result<()> {
    let urls = scratch.with_extension("urls");
    let _ = std::fs::remove_file(&urls);
    let mut watch = Watch { urls: urls.clone(), read: 0, seen: Default::default(), cancelled };
    let result = login_watched(agent, home, scratch, paths, method, &mut watch, report);
    let _ = std::fs::remove_file(&urls);
    result
}

fn login_watched(agent: &Agent, home: &Path, scratch: &Path, paths: &[crate::agents::HomePath], method: &str, watch: &mut Watch, report: &mut impl FnMut(Line)) -> Result<()> {
    let Some(prog) = agent.command.first() else { bail!("agent {} has no command", agent.id) };
    let mut env = agent.env.clone();
    let shim_dir = browser_shim(home)?;
    let path = std::env::var("PATH").unwrap_or_default();
    env.insert("PATH".into(), format!("{}:{path}", shim_dir.display()));
    env.insert("BROWSER".into(), shim_dir.join("xdg-open").display().to_string());
    env.insert(URLS_ENV.into(), watch.urls.display().to_string());
    env.insert("HOME".into(), scratch.display().to_string());
    for (name, rel) in [("XDG_CONFIG_HOME", ".config"), ("XDG_DATA_HOME", ".local/share"), ("XDG_STATE_HOME", ".local/state"), ("XDG_CACHE_HOME", ".cache")] {
        let dir = scratch.join(rel);
        private(&dir)?;
        env.insert(name.into(), dir.display().to_string());
    }
    if let (Some(var), Some(first)) = (&agent.home_env, paths.first()) {
        let rel = crate::agents::relative_path(first.path.strip_prefix("~/").context("home path")?)?;
        let owned = scratch.join(rel);
        private(&owned)?;
        let target = if agent.home_env_parent { owned.parent().context("home_env parent")? } else { &owned };
        env.insert(var.clone(), target.display().to_string());
    }
    env.insert("PYTHON_KEYRING_BACKEND".into(), "keyring.backends.fail.Keyring".into());
    // Some agents refuse browser helpers beneath their working directory. The
    // private login home is separate from tools/.browser-shim under the real home.
    let (mut cmd, _seccomp) = login_command(home, prog, None, &agent.command[1..], &env, report)?;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {prog} in the login jail"))?;
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
        let v2 = agent.experimental_acp_v2;
        let params = if v2 { json!({"protocolVersion":2,"info":{"name":"sill","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}) }
            else { json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"sill","version":env!("CARGO_PKG_VERSION")}}) };
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":params});
        writeln!(stdin, "{init}")?;
        let resp = wait_response(&rx, &erx, 1, Duration::from_secs(30), watch, report)?;
        if v2 && resp.get("protocolVersion").and_then(Value::as_u64)!=Some(2) {
            bail!("the opted-in ACP v2 agent did not select protocolVersion 2");
        }
        let offered = resp.get("authMethods").and_then(Value::as_array);
        let id_field = if v2 { "methodId" } else { "id" };
        let methods: Vec<String> = offered.into_iter().flatten().filter_map(|m|m.get(id_field).and_then(Value::as_str).map(str::to_string)).collect();
        if (v2 || !methods.is_empty()) && !methods.iter().any(|m|m==method) {
            bail!("{prog} offers no login method {method:?} (it has: {})", methods.join(", "));
        }
        if v2 && !offered.into_iter().flatten().any(|m|m.get(id_field).and_then(Value::as_str)==Some(method) && m.get("type").and_then(Value::as_str)==Some("agent")) {
            bail!("login method {method:?} is not an agent-driven ACP v2 login");
        }
        if !v2 && let Some(offered) = offered.into_iter().flatten().find(|m| m.get("id").and_then(Value::as_str) == Some(method)) {
            if offered.get("type").and_then(Value::as_str) == Some("terminal") {
                return terminal_login(agent, home, &env, offered, false, watch, report);
            }
            if let Some(terminal) = offered.pointer("/_meta/terminal-auth") {
                return terminal_login(agent, home, &env, terminal, true, watch, report);
            }
        }
        report(Line::Status(format!("logging in with {method}…")));
        let auth = json!({ "jsonrpc": "2.0", "id": 2, "method": if v2 {"auth/login"} else {"authenticate"}, "params": { "methodId": method } });
        writeln!(stdin, "{auth}")?;
        wait_response(&rx, &erx, 2, TIMEOUT, watch, report)?;
        Ok(())
    })();
    stop_agent(&mut child, &crate::registry::tools_dir(home));
    result
}

/// Every login runs in a jail, independent of its agent or installation source.
/// Failure to construct or start the jail is an error; there is no host fallback.
/// `program` is the agent's own and decides the jail; `advertised` is a command the agent's
/// answer asked to run in it instead ([`crate::registry::login_jail`]).
fn login_command(
    home: &Path,
    program: &str,
    advertised: Option<&str>,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
    report: &mut impl FnMut(Line),
) -> Result<(Command, Option<std::os::fd::OwnedFd>)> {
    let scratch = PathBuf::from(env.get("HOME").context("private login home")?);
    let urls = PathBuf::from(env.get(URLS_ENV).context("login URL file")?);
    report(Line::Status("the login runs in a jail, without your files".into()));
    crate::registry::login_jail(program, advertised, args, env, home, &scratch, &urls, &browser_shim(home)?)
}

/// Terminal auth runs the advertised login command rather than the ACP authenticate check.
/// Device flows print their URL/code to the registry's progress pane; no stdin is forwarded.
fn terminal_login(
    agent: &Agent,
    home: &Path,
    env: &std::collections::BTreeMap<String, String>,
    method: &Value,
    legacy: bool,
    watch: &mut Watch,
    report: &mut impl FnMut(Line),
) -> Result<()> {
    let args: Vec<String> =
        serde_json::from_value(method.get("args").cloned().unwrap_or(json!([])))
            .context("terminal login arguments")?;
    let extra_env: std::collections::BTreeMap<String, String> =
        serde_json::from_value(method.get("env").cloned().unwrap_or(json!({})))
            .context("terminal login environment")?;
    // The agent's own program builds the jail; a command its answer names only runs in it.
    let program = agent.command.first().context("agent has no command")?;
    let (advertised, mut all_args) = if legacy {
        let command = method
            .get("command")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .context("terminal login command is missing")?;
        (Some(command), Vec::new())
    } else {
        (None, agent.command[1..].to_vec())
    };
    all_args.extend(args);
    // the login's own variables win over the ones the method asks for
    let mut all_env = extra_env;
    all_env.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let (mut command, _seccomp) = login_command(home, program, advertised, &all_args, &all_env, report)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start terminal login")?;
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().context("terminal login stdout")?;
    let stderr = child.stderr.take().context("terminal login stderr")?;
    for stream in [
        Box::new(stdout) as Box<dyn std::io::Read + Send>,
        Box::new(stderr),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    report(Line::Status(
        "Complete the login in your browser; the login command's progress follows.".into(),
    ));
    let deadline = std::time::Instant::now() + TIMEOUT;
    let result = (|| -> Result<()> {
        loop {
            for line in rx.try_iter() {
                report(Line::Status(line));
            }
            watch.poll(report)?;
            if let Some(status) = child.try_wait()? {
                // Drain final output, bounded even if a descendant inherited a pipe.
                let drain_until = std::time::Instant::now() + Duration::from_secs(1);
                while std::time::Instant::now() < drain_until {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(line) => report(Line::Status(line)),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                anyhow::ensure!(status.success(), "terminal login failed: {status}");
                return Ok(());
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "terminal login timed out"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    })();
    stop_agent(&mut child, &crate::registry::tools_dir(home));
    result
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

/// The URLs a login's browser helper handed over, reported each once; and its cancellation.
struct Watch<'a> {
    urls: PathBuf,
    read: usize,
    seen: std::collections::HashSet<String>,
    cancelled: &'a dyn Fn() -> bool,
}

impl Watch<'_> {
    /// Report URLs that arrived since the last poll; [`Cancelled`] once the login is cancelled.
    fn poll(&mut self, report: &mut impl FnMut(Line)) -> Result<()> {
        if (self.cancelled)() {
            return Err(Cancelled.into());
        }
        let Ok(bytes) = std::fs::read(&self.urls) else { return Ok(()) };
        let bytes = &bytes[..bytes.len().min(MAX_URLS_BYTES)];
        // whole lines only: the helper may be writing the rest
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n').map(|i| i + 1) else { return Ok(()) };
        if end <= self.read {
            return Ok(());
        }
        let fresh = String::from_utf8_lossy(&bytes[self.read..end]).into_owned();
        self.read = end;
        for url in fresh.lines().map(str::trim) {
            let lower = url.to_ascii_lowercase();
            let web = lower.starts_with("https://") || lower.starts_with("http://");
            if web && url.len() <= 16 * 1024 && !url.chars().any(char::is_control) && self.seen.insert(url.to_string()) {
                report(Line::OpenUrl { url: url.to_string(), callback_port: callback_port(url) });
            }
        }
        Ok(())
    }
}

/// The daemon host's loopback port an OAuth page sends the browser back to: its `redirect_uri`
/// when that is `http://localhost:PORT` or `http://127.0.0.1:PORT`.
pub fn callback_port(url: &str) -> Option<u16> {
    let query = url.split_once('?')?.1.split('#').next()?;
    let value = query.split('&').find_map(|pair| pair.strip_prefix("redirect_uri="))?;
    let redirect = percent_decode(value)?;
    let authority = redirect.strip_prefix("http://")?.split(['/', '?', '#']).next()?;
    let (host, port) = authority.rsplit_once(':')?;
    if !matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1") {
        return None;
    }
    port.parse().ok().filter(|port| *port != 0)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => { out.push(b' '); i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8(out).ok()
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



/// The result object of response `id`; notifications on the way are reported.
fn wait_response(
    rx: &std::sync::mpsc::Receiver<Value>,
    erx: &std::sync::mpsc::Receiver<String>,
    id: u64,
    timeout: Duration,
    watch: &mut Watch,
    report: &mut impl FnMut(Line),
) -> Result<Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        for line in erx.try_iter() {
            report(Line::Stderr(line));
        }
        watch.poll(report)?;
        let now = std::time::Instant::now();
        if now >= deadline {
            bail!("no answer within {} s", timeout.as_secs());
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(200))) {
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

/// The variable naming the file the browser helper appends URLs to, one per line.
const URLS_ENV: &str = "SILL_LOGIN_URLS";
/// At most this much of the URL file is read: a login opens a page or two.
const MAX_URLS_BYTES: usize = 1024 * 1024;

/// A directory with an `xdg-open`, also the login's `BROWSER`, that only hands its URL to the
/// login ([`Watch`]): the frontend opens it in the person's browser, wherever that is.
fn browser_shim(home: &Path) -> Result<PathBuf> {
    let dir = crate::registry::tools_dir(home).join(".browser-shim");
    std::fs::create_dir_all(&dir)?;
    let script = format!(
        "#!/bin/sh\n# sill: the frontend opens login pages in the person's browser (docs/login.md); this hands the URL over\n[ -n \"${URLS_ENV}\" ] || {{ echo \"sill: no login to hand $1 to\" >&2; exit 1; }}\nfor url in \"$@\"; do printf '%s\\n' \"$url\" >> \"${URLS_ENV}\"; done\n"
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

    #[test]
    fn login_commands_always_use_bwrap() {
        let home = dir("always-jailed");
        let scratch = home.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let urls = home.join("urls");
        let env = [("HOME".into(), scratch.display().to_string()),
                   (URLS_ENV.into(), urls.display().to_string())].into_iter().collect();
        let (command, _seccomp) = login_command(&home, "/bin/sh", None, &[], &env, &mut |_| {}).unwrap();
        assert_eq!(command.get_program(), "bwrap");
        // Install trust is deliberately absent from command selection.
        assert!(login_command(&home, "/nonexistent/login-program", None, &[], &env, &mut |_| {}).is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_jailed_login_has_the_network_and_its_own_home_and_nothing_of_the_persons() {
        let home = dir("jail");
        let scratch = home.join(".local/state/sill/login/x");
        std::fs::create_dir_all(&scratch).unwrap();
        let urls = scratch.with_extension("urls");
        let front = home.join("front");
        let env: std::collections::BTreeMap<String, String> =
            [("HOME", scratch.to_str().unwrap()), ("PATH", "/person/bin"), (URLS_ENV, urls.to_str().unwrap())].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let (command, _seccomp) = crate::registry::login_jail("/bin/sh", None, &["-c".into(), "true".into()], &env, &home, &scratch, &urls, &front).unwrap();
        assert_eq!(command.get_program(), "bwrap");
        let argv: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let r = argv.join(" ");
        let h = home.display();
        assert!(r.starts_with("--unshare-all --share-net --unshare-user --disable-userns --die-with-parent --new-session --cap-drop ALL --clearenv "), "{r}");
        assert!(r.contains(&format!("--tmpfs {h} ")), "the home is hidden: {r}");
        assert!(r.contains(&format!("--bind {s} {s} --bind {u} {u}", s = scratch.display(), u = urls.display())), "{r}");
        assert!(r.contains(&format!("--setenv HOME {}", scratch.display())), "{r}");
        assert!(r.contains(&format!("--setenv PATH {}:/usr/local/bin:/usr/bin:/bin", front.display())), "{r}");
        assert!(!r.contains("/person/bin"), "the host's PATH stays out: {r}");
        assert!(r.ends_with(&format!("--chdir {} -- /bin/sh -c true", scratch.display())), "{r}");
        assert!(urls.is_file(), "the helper's file exists before it is bound");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A terminal login's command comes from the agent's answer. Whatever it names, the jail is
    /// the agent's own: no directory of the person's is bound for it.
    #[test]
    fn an_advertised_login_command_shows_nothing_of_the_persons() {
        let home = dir("advertised");
        let scratch = home.join(".local/state/sill/login/x");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/known_hosts"), "host key").unwrap();
        std::fs::create_dir_all(home.join("work/project/bin")).unwrap();
        std::fs::write(home.join("work/project/bin/tool"), "#!/bin/sh\n").unwrap();
        // links the login made in its own home, pointing at the person's files
        std::os::unix::fs::symlink(home.join(".ssh/known_hosts"), scratch.join("keys")).unwrap();
        std::os::unix::fs::symlink(home.join("work/project/bin/tool"), scratch.join("tool")).unwrap();
        let urls = scratch.with_extension("urls");
        let env: std::collections::BTreeMap<String, String> =
            [("HOME", scratch.to_str().unwrap()), (URLS_ENV, urls.to_str().unwrap())].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        // the command line without the seccomp filter's descriptor, which differs per jail
        let line = |c: Command| {
            let mut args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
            if let Some(i) = args.iter().position(|a| a == "--seccomp") {
                args.drain(i..i + 2);
            }
            args.join(" ")
        };
        let jail = |advertised: &str| login_command(&home, "/bin/sh", Some(advertised), &["login".into()], &env, &mut |_| {}).map(|(c, _)| line(c));
        let own = line(login_command(&home, "/bin/sh", None, &[], &env, &mut |_| {}).unwrap().0);
        let own_mounts = own.split(" --chdir ").next().unwrap().to_string();
        for link in ["keys", "tool"] {
            let advertised = scratch.join(link);
            let r = jail(advertised.to_str().unwrap()).unwrap();
            assert_eq!(r.split(" --chdir ").next().unwrap(), own_mounts, "the agent's own jail, nothing added");
            assert!(!r.contains(&format!("bind {}", home.join(".ssh").display())) && !r.contains(&format!("bind {}", home.join("work").display())), "{r}");
            assert!(r.ends_with(&format!("-- {} login", advertised.display())), "it runs as the jail shows it: {r}");
        }
        // a path among the person's files is not in the jail at all
        for path in [home.join(".ssh/known_hosts"), home.join("work/project/bin/tool"), scratch.join("../../../../../.ssh/known_hosts")] {
            assert!(jail(path.to_str().unwrap()).is_err(), "{}", path.display());
        }
        assert!(jail("bin/sh").is_err() && jail("no-such-login-program").is_err());
        // a system program, by name or by path
        assert!(jail("sh").unwrap().ends_with("/sh login"));
        assert!(jail("/bin/sh").unwrap().ends_with("-- /bin/sh login"));
        let _ = std::fs::remove_dir_all(&home);
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sill-login-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_callback_port_comes_only_from_a_loopback_redirect() {
        let codex = "https://auth.openai.com/oauth/authorize?response_type=code&client_id=x&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&scope=openid";
        assert_eq!(callback_port(codex), Some(1455));
        assert_eq!(callback_port("https://a.example/?redirect_uri=http://127.0.0.1:54321/cb"), Some(54321));
        assert_eq!(callback_port("https://a.example/?redirect_uri=https%3A%2F%2Fexample.com%3A1455%2Fcb"), None);
        assert_eq!(callback_port("https://a.example/?redirect_uri=http%3A%2F%2Fevil.example%3A1455%2F"), None);
        assert_eq!(callback_port("https://a.example/?redirect_uri=http%3A%2F%2Flocalhost%2Fcb"), None);
        assert_eq!(callback_port("https://a.example/?redirect_uri=http%3A%2F%2Flocalhost%3A0%2F"), None);
        assert_eq!(callback_port("https://a.example/device"), None);
    }

    /// The agent "opens a browser" through `BROWSER` and then waits for a callback that never
    /// comes: the URL is reported once, with its port, and a cancel ends the wait.
    #[test]
    #[ignore = "requires working bubblewrap on a host"]
    fn a_login_reports_the_browser_url_and_can_be_cancelled() {
        let home = dir("open-url");
        let tools = crate::registry::tools_dir(&home);
        std::fs::create_dir_all(&tools).unwrap();
        let script = tools.join("agent.py");
        std::fs::write(&script, r#"import json,os,subprocess,sys
request=json.loads(sys.stdin.readline())
print(json.dumps({'jsonrpc':'2.0','id':1,'result':{'protocolVersion':1,'authMethods':[{'id':'browser','name':'Browser'}]}}),flush=True)
request=json.loads(sys.stdin.readline())
url='https://auth.example/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fcb'
for _ in range(2):
    subprocess.run([os.environ['BROWSER'],url],check=True)
subprocess.run(['xdg-open','https://auth.example/second'],check=True)
sys.stdin.readline()
"#).unwrap();
        let mut agent = crate::agents::builtin().into_iter().find(|a| a.id == "codex").unwrap();
        agent.host_login = vec!["browser".into()];
        agent.command = vec!["/usr/bin/python3".into(), script.to_string_lossy().into_owned()];
        let opened = std::cell::RefCell::new(Vec::new());
        let cancelled = || opened.borrow().len() >= 2;
        let started = std::time::Instant::now();
        let error = run_cancellable(&agent, &home, &[], "browser", &cancelled, |line| {
            if let Line::OpenUrl { url, callback_port } = line { opened.borrow_mut().push((url, callback_port)); }
        }).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(*opened.borrow(), [
            ("https://auth.example/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fcb".to_string(), Some(1455)),
            ("https://auth.example/second".to_string(), None),
        ]);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    #[ignore = "requires working bubblewrap on a host"]
    fn v2_host_login_negotiates_and_uses_only_an_offered_agent_method() {
        let home=dir("v2-host");
        let tools=crate::registry::tools_dir(&home);
        std::fs::create_dir_all(&tools).unwrap();
        let script=tools.join("agent.py");
        std::fs::write(&script,r#"import json,sys
request=json.loads(sys.stdin.readline())
assert request['params']['protocolVersion']==2
assert 'info' in request['params'] and 'clientInfo' not in request['params']
print(json.dumps({'jsonrpc':'2.0','id':1,'result':{'protocolVersion':int(sys.argv[1]),'info':{'name':'fixture','version':'1'},'capabilities':{},'authMethods':[{'type':sys.argv[2],'methodId':'browser','name':'Browser'}]}}),flush=True)
request=json.loads(sys.stdin.readline())
assert request['method']=='auth/login' and request['params']=={'methodId':'browser'}
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{}}),flush=True)
"#).unwrap();
        let mut agent=crate::agents::builtin().into_iter().find(|a|a.id=="codex").unwrap();
        agent.experimental_acp_v2=true;
        agent.host_login = vec!["browser".into()];
        agent.command=vec!["/usr/bin/python3".into(),script.to_string_lossy().into_owned(),"2".into(),"agent".into()];
        run(&agent,&home,"browser",|_|{}).unwrap();
        agent.command[2]="1".into();
        assert!(run(&agent,&home,"browser",|_|{}).unwrap_err().to_string().contains("protocolVersion 2"));
        agent.command[2]="2".into();agent.command[3]="future".into();
        assert!(run(&agent,&home,"browser",|_|{}).unwrap_err().to_string().contains("not an agent-driven"));
        let _=std::fs::remove_dir_all(home);
    }

    #[test]
    #[ignore = "requires working bubblewrap on a host"]
    fn terminal_setup_runs_advertised_login_and_keeps_its_files() {
        for legacy in [false, true] {
            let home = dir(if legacy {
                "terminal-legacy"
            } else {
                "terminal"
            });
            let tools = crate::registry::tools_dir(&home);
            std::fs::create_dir_all(&tools).unwrap();
            let script = tools.join("agent.py");
            std::fs::write(&script, r#"import json,sys,os,pathlib
legacy = sys.argv[1] == 'legacy'
if '--login' in sys.argv or 'login' in sys.argv:
    assert pathlib.Path.cwd() == pathlib.Path.home()
    # Kimi rejects xdg-open helpers under cwd: the shim must remain outside it.
    shim = pathlib.Path(os.environ['PATH'].split(os.pathsep)[0])/'xdg-open'
    assert shim.is_file()
    assert not shim.is_relative_to(pathlib.Path.cwd())
    assert os.environ['LOGIN_FIXTURE'] == 'yes'
    assert os.environ['XDG_CONFIG_HOME'].startswith(os.environ['HOME'])
    assert 'DBUS_SESSION_BUS_ADDRESS' not in os.environ
    target = pathlib.Path.home()/'.fixture'/'token.json'
    target.parent.mkdir(parents=True)
    target.write_text('{}')
    print('Open https://example.invalid/device and enter CODE', flush=True)
    print('Authorized', file=sys.stderr, flush=True)
    sys.exit(0)
assert pathlib.Path.cwd() == pathlib.Path.home()
request=json.loads(sys.stdin.readline())
assert request['method']=='initialize'
method={'id':'login','name':'Account'}
if legacy:
    method['_meta']={'terminal-auth':{'command':sys.executable,'args':[__file__,'legacy','login'],'env':{'LOGIN_FIXTURE':'yes'}}}
else:
    method.update(type='terminal',args=['--login'],env={'LOGIN_FIXTURE':'yes'})
print(json.dumps({'jsonrpc':'2.0','id':1,'result':{'authMethods':[method]}}),flush=True)
request=sys.stdin.readline()
assert not request, 'terminal methods must never receive authenticate'
"#).unwrap();
            let mut agent = crate::agents::builtin()
                .into_iter()
                .find(|a| a.id == "codex")
                .unwrap();
            agent.id = "terminal-fixture".into();
            agent.home_paths = None;
            agent.home_env = None;
            agent.command = vec![
                "/usr/bin/python3".into(),
                script.to_string_lossy().into_owned(),
                if legacy { "legacy" } else { "current" }.into(),
                "acp".into(),
            ];
            let mut lines = Vec::new();
            let discovery = setup(&agent, &home, "login", |line| {
                if let Line::Status(line) = line {
                    lines.push(line);
                }
            })
            .unwrap();
            assert_eq!(discovery.login_method.as_deref(), Some("login"));
            assert!(
                discovery
                    .login_files
                    .contains(&".fixture/token.json".into())
            );
            assert!(
                crate::session::agent_home(&home, &agent.id)
                    .join(".fixture/token.json")
                    .is_file()
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("https://example.invalid/device"))
            );
            assert!(lines.iter().any(|line| line == "Authorized"));
            // A failed helper must not become a successful discovery.
            std::fs::write(
                &script,
                std::fs::read_to_string(&script)
                    .unwrap()
                    .replace("sys.exit(0)", "sys.exit(7)"),
            )
            .unwrap();
            assert!(
                setup(&agent, &home, "login", |_| {})
                    .unwrap_err()
                    .to_string()
                    .contains("terminal login failed")
            );
            let _ = std::fs::remove_dir_all(home);
        }
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
        let agent = crate::agents::builtin().into_iter().find(|a| a.id == "codex").unwrap();
        crate::credentials::seed(&crate::credentials::Rules::for_agent(&agent).unwrap(), &scratch, &dst).unwrap();
        assert!(!dst.join(".codex/config.toml").exists());
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
