//! What runs in the jail, seen from the shim (docs/jail-processes.md): every process in the jail's
//! pid namespace, classed. Nothing in the jail can stay off this list: the jail is a namespace of
//! its own and the shim is the subreaper every orphan comes back to. It is a plain list for the
//! person to see; nothing is concluded from it about the agent's turn.
//!
//! - **command**: anything the agent started, or left behind, that runs now;
//! - **service**: a managed process (`sill-proc`) and its tree, or a program listening on a port;
//! - the agent itself, the person's terminals and sill's own (the shim, the private bus) are
//!   left out.
//!
//! A tree is reported once, at its root.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use serde::Serialize;

/// Sessions of the person's terminals (`open_pty`'s children lead their own).
static TERMINALS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());
/// The private bus's dbus-daemon, when there is one.
static BUS: Mutex<Option<u32>> = Mutex::new(None);

pub fn terminal_started(pid: u32) {
    TERMINALS.lock().unwrap().insert(pid);
}

pub fn bus_started(pid: u32) {
    *BUS.lock().unwrap() = Some(pid);
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tree {
    pub pid: u32,
    pub argv: Vec<String>,
    /// Unix milliseconds.
    pub started_ms: u64,
    /// Processes in the tree, the root included.
    pub count: u32,
    /// The lowest port a process in the tree listens on.
    pub port: Option<u16>,
    /// The managed process's name, when the tree is one.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub commands: Vec<Tree>,
    pub services: Vec<Tree>,
}

/// One process as `/proc` shows it.
#[derive(Debug, Clone, Default)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub session: u32,
    pub argv: Vec<String>,
    /// Clock ticks after boot.
    pub start_ticks: u64,
    pub ports: Vec<u16>,
}

/// What the classing needs to know besides the processes.
pub struct Known<'a> {
    pub shim: u32,
    pub agent: Option<u32>,
    /// Managed processes: pid → name.
    pub managed: &'a HashMap<u32, String>,
    pub terminals: &'a BTreeSet<u32>,
    pub bus: Option<u32>,
    /// Unix milliseconds at boot, and clock ticks per second.
    pub boot_ms: u64,
    pub hz: u64,
}

/// The jail's processes classed, from `procs` (the whole namespace).
pub fn classify(procs: &[Proc], known: &Known) -> Report {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    // the roots: what the agent started, and what the shim adopted or started besides the agent
    let mut report = Report::default();
    let mut roots: Vec<u32> = Vec::new();
    for p in procs {
        if p.pid == 1 || p.pid == known.shim {
            continue;
        }
        if Some(p.pid) == known.agent {
            continue;
        }
        let parent_is_agent = Some(p.ppid) == known.agent;
        let parent_is_shim = p.ppid == known.shim || p.ppid == 1;
        if parent_is_agent || parent_is_shim {
            roots.push(p.pid);
        }
    }
    for root in roots {
        let p = by_pid[&root];
        if known.terminals.contains(&p.session) || Some(p.pid) == known.bus {
            continue;
        }
        let mut tree = Tree {
            pid: p.pid,
            argv: p.argv.clone(),
            started_ms: known.boot_ms + p.start_ticks * 1000 / known.hz.max(1),
            count: 0,
            port: None,
            name: known.managed.get(&p.pid).cloned(),
        };
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            let Some(q) = by_pid.get(&pid) else { continue };
            tree.count += 1;
            if let Some(port) = q.ports.iter().min() {
                tree.port = Some(tree.port.map_or(*port, |known| known.min(*port)));
            }
            if tree.name.is_none() {
                tree.name = known.managed.get(&pid).cloned();
            }
            stack.extend(children.get(&pid).into_iter().flatten().copied());
        }
        if tree.name.is_some() || tree.port.is_some() {
            report.services.push(tree);
        } else {
            report.commands.push(tree);
        }
    }
    report.commands.sort_by_key(|t| t.started_ms);
    report.services.sort_by_key(|t| t.started_ms);
    report
}

/// Read every process of this namespace from `/proc`.
pub fn read_procs() -> Vec<Proc> {
    let listening = listening_inodes();
    let Ok(dir) = std::fs::read_dir("/proc") else { return vec![] };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let Some(mut p) = parse_stat(&stat) else { continue };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        // a zombie has no command line: it has ended
        if cmdline.is_empty() {
            continue;
        }
        p.argv = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        if !listening.is_empty() && let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for fd in fds.flatten() {
                if let Ok(target) = std::fs::read_link(fd.path())
                    && let Some(inode) = target.to_str().and_then(|t| t.strip_prefix("socket:[")).and_then(|t| t.strip_suffix(']')).and_then(|t| t.parse::<u64>().ok())
                    && let Some(port) = listening.get(&inode)
                    && !p.ports.contains(port)
                {
                    p.ports.push(*port);
                }
            }
        }
        out.push(p);
    }
    out
}

/// `/proc/PID/stat`: the fields after the command name, which may hold spaces and parentheses.
pub fn parse_stat(stat: &str) -> Option<Proc> {
    let (head, rest) = stat.rsplit_once(") ")?;
    let pid = head.split_once(" (")?.0.trim().parse().ok()?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    // rest[0] is field 3 (state): ppid is field 4, session 6, starttime 22
    let num = |field: usize| f.get(field - 3).and_then(|v| v.parse::<u64>().ok());
    Some(Proc {
        pid,
        ppid: num(4)? as u32,
        session: num(6)? as u32,
        start_ticks: num(22)?,
        ..Proc::default()
    })
}

/// Listening TCP sockets of this network namespace: inode → port.
fn listening_inodes() -> HashMap<u64, u16> {
    let mut out = HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            // local address, state 0A (listen), inode
            if f.len() > 9 && f[3] == "0A"
                && let Some(port) = f[1].rsplit_once(':').and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
                && let Ok(inode) = f[9].parse::<u64>()
            {
                out.insert(inode, port);
            }
        }
    }
    out
}

/// Unix milliseconds at boot, and clock ticks per second.
pub fn clock() -> (u64, u64) {
    let btime = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse::<u64>().ok())))
        .unwrap_or(0);
    (btime * 1000, rustix::param::clock_ticks_per_second().max(1))
}

/// The report now: `agent` is the agent's pid, `managed` the managed processes.
pub fn report(agent: Option<u32>, managed: &HashMap<u32, String>) -> Report {
    let (boot_ms, hz) = clock();
    let terminals = TERMINALS.lock().unwrap().clone();
    let bus = *BUS.lock().unwrap();
    let known = Known { shim: std::process::id(), agent, managed, terminals: &terminals, bus, boot_ms, hz };
    classify(&read_procs(), &known)
}

/// Stop the command tree rooted at `pid`: asked, then made to. Only a command root of the report
/// now; the agent, services and terminals have their own ways.
pub fn stop_command(pid: u32, agent: Option<u32>, managed: &HashMap<u32, String>) -> Result<(), String> {
    let report = report(agent, managed);
    if !report.commands.iter().any(|t| t.pid == pid) {
        return Err(format!("no command with pid {pid} runs in the jail"));
    }
    let tree = move |root: u32| -> Vec<u32> {
        let procs = read_procs();
        let mut out = vec![];
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            out.push(p);
            stack.extend(procs.iter().filter(|q| q.ppid == p).map(|q| q.pid));
        }
        out
    };
    let signal = move |sig: rustix::process::Signal| {
        for p in tree(pid) {
            if let Some(p) = rustix::process::Pid::from_raw(p as i32) {
                let _ = rustix::process::kill_process(p, sig);
            }
        }
    };
    signal(rustix::process::Signal::TERM);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        signal(rustix::process::Signal::KILL);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, session: u32, argv: &str) -> Proc {
        Proc {
            pid,
            ppid,
            session,
            argv: argv.split(' ').map(str::to_owned).collect(),
            start_ticks: pid as u64 * 100,
            ports: vec![],
        }
    }

    #[test]
    fn processes_are_classed_and_trees_reported_at_their_root() {
        let mut server = proc(30, 2, 2, "npm run dev");
        server.ports = vec![5173];
        let procs = vec![
            proc(1, 0, 1, "bwrap"),
            proc(2, 1, 2, "/.sill/shim"),
            proc(4, 2, 2, "dbus-daemon"),
            proc(8, 2, 2, "kimi-code"),
            // a build the agent runs: a tree
            proc(10, 8, 2, "bash -c cargo build"),
            proc(11, 10, 2, "cargo build"),
            proc(12, 11, 2, "rustc"),
            // detached: the shim adopted it
            proc(20, 2, 20, "sill-shim --listen /tmp/x.sock"),
            server,
            // a managed process
            proc(40, 2, 40, "python3 -m http.server"),
            // the person's terminal and what runs in it
            proc(50, 2, 50, "bash"),
            proc(51, 50, 50, "vim"),
        ];
        let managed = HashMap::from([(40, "web".to_string())]);
        let terminals = BTreeSet::from([50]);
        let known = Known { shim: 2, agent: Some(8), managed: &managed, terminals: &terminals, bus: Some(4), boot_ms: 1_000_000, hz: 100 };
        let report = classify(&procs, &known);
        let commands: Vec<(u32, u32)> = report.commands.iter().map(|t| (t.pid, t.count)).collect();
        assert_eq!(commands, [(10, 3), (20, 1)]);
        let services: Vec<(u32, Option<&str>, Option<u16>)> =
            report.services.iter().map(|t| (t.pid, t.name.as_deref(), t.port)).collect();
        assert_eq!(services, [(30, None, Some(5173)), (40, Some("web"), None)]);
        assert_eq!(report.commands[0].started_ms, 1_000_000 + 10 * 100 * 1000 / 100);
    }

    #[test]
    fn stat_lines_with_odd_names_parse() {
        let line = "1234 (a (weird) name) S 2 1234 1234 0 -1 4194304 100 0 0 0 7 3 0 0 20 0 1 0 5555 1000 10 0 0 0";
        let p = parse_stat(line).unwrap();
        assert_eq!((p.pid, p.ppid, p.session, p.start_ticks), (1234, 2, 1234, 5555));
    }
}
