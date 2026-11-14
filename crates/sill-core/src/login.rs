//! Host-side login uses a fresh private HOME and XDG tree. Only declared credential files
//! return to the profile; settings, history, hooks and incidental tool output never do.
//! The browser helper alone restores the person's environment. Host methods must be declared.

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
pub fn run_into(agent: &Agent, home: &Path, extra_homes: &[(PathBuf, crate::credentials::Rules)], method: &str, mut report: impl FnMut(Line)) -> Result<()> {
    let paths = agent.ready(home)?;
    anyhow::ensure!(agent.host_login.iter().any(|m| m == method), "host login method is not explicitly declared");
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
    private(&scratch)?;
    let shim_dir = browser_shim(home)?;
    let path = std::env::var("PATH").unwrap_or_default();
    env.insert("PATH".into(), format!("{}:{path}", shim_dir.display()));
    env.insert("BROWSER".into(), shim_dir.join("xdg-open").display().to_string());
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
    unset.push("DBUS_SESSION_BUS_ADDRESS");
    env.insert("PYTHON_KEYRING_BACKEND".into(), "keyring.backends.fail.Keyring".into());
    let copy_back = Some(crate::session::agent_home(home, &agent.id));
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
        let v2 = agent.experimental_acp_v2;
        let params = if v2 { json!({"protocolVersion":2,"info":{"name":"sill","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}) }
            else { json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"sill","version":env!("CARGO_PKG_VERSION")}}) };
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":params});
        writeln!(stdin, "{init}")?;
        let resp = wait_response(&rx, &erx, 1, Duration::from_secs(30), &mut report)?;
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
        report(Line::Status(format!("logging in with {method}…")));
        let auth = json!({ "jsonrpc": "2.0", "id": 2, "method": if v2 {"auth/login"} else {"authenticate"}, "params": { "methodId": method } });
        writeln!(stdin, "{auth}")?;
        wait_response(&rx, &erx, 2, TIMEOUT, &mut report)?;
        Ok(())
    })();
    stop_agent(&mut child, &crate::registry::tools_dir(home));
    let copied = match (&result, copy_back) {
        (Ok(()), Some(dst)) => (|| -> Result<usize> {
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
    fn v2_host_login_negotiates_and_uses_only_an_offered_agent_method() {
        let home=dir("v2-host");
        let script=home.join("agent.py");
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
