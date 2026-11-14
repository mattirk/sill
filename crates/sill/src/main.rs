//! `sill` command line. M0: drive the jail builder (`sill jail …`) and one development session
//! (`sill dev …`) by hand; the shelf and the real session commands come after.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use sill_core::acp::{self, AutoPermission, Event};
use sill_core::jail::{self, JailSpec};
use sill_core::ledger;
use sill_core::mounts;
use sill_core::policy::Policy;
use sill_core::shim;

mod daemon;
mod init;
mod shelf;
mod tui;

#[derive(Parser)]
#[command(name = "sill", version, about = "agent shelf")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build and run jails by hand (M0)
    Jail {
        #[command(subcommand)]
        cmd: JailCmd,
    },
    /// One development session: jail + shim + agent, driven step by step (M0)
    Dev {
        #[command(subcommand)]
        cmd: DevCmd,
    },
    /// Session launcher (SPEC §4.7): binds the shim and proxy sockets, starts bwrap with the shim
    /// inside, serves the proxy, exits when the jail ends. Runs inside the session's systemd user
    /// unit, or detached in `direct` mode
    Launch {
        /// session id (sockets under $XDG_RUNTIME_DIR/sill/<id>)
        #[arg(long)]
        session: String,
        #[command(flatten)]
        jail: JailArgs,
        /// sill-shim binary to bind into the jail (default: next to this executable)
        #[arg(long)]
        shim: Option<PathBuf>,
        /// registry id of the agent: its domains join the proxy allow-list
        #[arg(long)]
        agent: Option<String>,
        /// started by the daemon as its child (docs/daemon.md §3.1): write pid files, put the jail in
        /// the session cgroup under --cgroup-root, log everything to stderr
        #[arg(long)]
        child: bool,
        /// delegated cgroup subtree to create the session cgroup in
        #[arg(long)]
        cgroup_root: Option<PathBuf>,
    },
    /// Talk to a session from this terminal (docs/daemon.md decision 11): a line is a prompt, the
    /// agent's text and tool calls come back, a permission request is answered on the next line
    Attach {
        /// session id, its first characters, or the exact title
        key: String,
        /// start the agent again instead of reattaching to one running in the jail
        #[arg(long)]
        fresh: bool,
        /// thoughts, tool call updates and usage too
        #[arg(short, long)]
        verbose: bool,
    },
    /// The daemon (docs/daemon.md): run it (what sill.service does), or ask the running one
    Daemon {
        #[command(subcommand)]
        cmd: Option<DaemonCmd>,
    },
    /// Set the machine up (root): checks, binaries, policy, state filesystem, cgroup delegation;
    /// `--user` sets up the shelf autostart for this account instead (SPEC §4.12)
    Init(init::InitArgs),
    /// Serve a session's network proxy (SPEC §4.8) on an inherited listening socket
    Proxy {
        /// listening Unix socket fd (inherited from the launcher)
        #[arg(long, default_value_t = 3)]
        fd: i32,
        /// bind this socket path instead of using an inherited fd (testing)
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(short, long, default_value = "default")]
        policy: String,
    },
    /// Open the shelf (GTK); single instance per desktop session
    Shelf {
        /// working directory of the main session (default: current directory)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// start without showing the window (autostart); `sill toggle` shows it
        #[arg(long)]
        hidden: bool,
    },
    /// Show/hide the running shelf (bind this to a hotkey)
    Toggle,
    /// Restart the shelf: `systemctl --user restart sill-shelf` when the service is installed
    Restart {
        /// start hidden (dev mode)
        #[arg(long)]
        hidden: bool,
        /// stop the service and run THIS binary instead (try a build without installing it)
        #[arg(long)]
        dev: bool,
        /// (internal) already running outside the shelf's process group — do not detach again
        #[arg(long, hide = true)]
        detached: bool,
    },
    /// Switch the shelf's colour theme: adwaita | adwaita-light | dracula | solarized-dark | solarized-light | nord
    Theme { name: String },
    /// Set the show/hide key, GTK accelerator syntax: "<Alt>space", "<Super>grave", "F12"
    Hotkey { spec: String },
    /// Log an agent in on this machine (browser flows that cannot run inside the jail, e.g. Codex
    /// with ChatGPT); the login file is bound into the agent's jails from then on
    Login {
        /// registry id: claude | codex | gemini
        agent: String,
        /// login method id as the agent advertises it (default: the agent's first host-side method)
        #[arg(long)]
        method: Option<String>,
    },
    /// What sill remembers of ended sessions (docs/memory.md)
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// What sessions changed against the host, through the daemon: list, diff, keep, revert, undo
    /// (SPEC §4.4; `sill jail keep|kept|unkeep|changes` work on a state dir by hand)
    Changes {
        #[command(subcommand)]
        cmd: Option<ChangesCmd>,
    },
    /// Sessions, through the daemon: start one, list them, end one (docs/daemon.md §3.4)
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// The ACP agent registry sill installs from (security.md S13)
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
    /// The global pins (docs/pins.md): bound in every new session's jail, a chip only when pinned
    /// in the session too
    Pin {
        #[command(subcommand)]
        cmd: Option<PinCmd>,
    },
    /// Show a policy as sill reads it
    Policy {
        /// name (searched in ~/.config/sill/policies, /etc/sill/policies, built-in) or path
        #[arg(default_value = "default")]
        name: String,
    },
}

#[derive(Subcommand)]
enum PinCmd {
    /// List the global pins
    List,
    /// Add a folder or file (refused inside another global pin, or inside sill's own state)
    Add { path: PathBuf },
    /// Remove one; sessions started later no longer bind it
    Remove { path: PathBuf },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Version, mode, uptime and sessions of the running daemon (starts one when none runs)
    Status,
    /// Ask the running daemon to leave; sessions are not touched
    Stop,
    /// The protocol as one JSON document: every method with its params and result, every type's
    /// JSON Schema (docs/protocol.md) — for a frontend in another language
    Schema,
}

#[derive(Subcommand)]
enum ChangesCmd {
    /// Every session's pending changes and undoable keeps, and the pinned repositories' git status
    List,
    /// One change's diff: the session's version against the host's
    Diff { session: String, path: PathBuf },
    /// Copy changed paths to the host, the previous versions saved for undo
    Keep { session: String, paths: Vec<PathBuf> },
    /// Put the host's version back in the session
    Revert { session: String, paths: Vec<PathBuf> },
    /// What sessions did to this machine over time: kept, undone, reverted, auto-reverted, the
    /// commits in pinned repositories (SPEC §3.6)
    History {
        /// only this long back: 7d, 12h, 2w
        #[arg(long)]
        since: Option<String>,
        /// only on this path or below it
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        agent: Option<String>,
        /// kept | undone | reverted | auto-reverted | pinned | deleted | folded
        #[arg(long)]
        kind: Option<String>,
        /// one session (id, its first characters, or its title)
        #[arg(long)]
        session: Option<String>,
    },
    /// Undo keeps: the given keep ids, or every undoable keep of the session
    Undo {
        session: String,
        ids: Vec<String>,
        /// even when the host was edited after the keep
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// A new session with its jail started; prints its id. The shelf lists it under Reopen to
    /// open its conversation.
    New {
        /// registry id of the agent
        #[arg(long, default_value = "claude")]
        agent: String,
        #[arg(long, default_value = "default")]
        policy: String,
        /// a ghost (docs/memory.md §3): no pins, no memory written, writes revert after an hour,
        /// everything deleted at its end
        #[arg(long)]
        ghost: bool,
        /// pin a folder or file (repeatable; refused for a ghost)
        #[arg(long = "pin")]
        pins: Vec<PathBuf>,
    },
    /// Sessions in the store, newest first: id, state, agent, title
    List,
    /// End a session as closing its tab does: the summary, or for a ghost its deletion (KEY: an
    /// id, its first characters, or the exact title)
    End { key: Vec<String> },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Ended sessions, newest first: id, date, agent, title, the summary's first line
    List,
    /// One session's summary and facts (ID: an id, its first characters, or the exact title)
    Show { id: Vec<String> },
    /// Sessions whose title, summary, notes or prompts hold every word (also folder:PATH agent:NAME since:30d)
    Search {
        words: Vec<String>,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        /// 30d, 12h, 2w …
        #[arg(long)]
        since: Option<String>,
    },
    /// One session's conversation: the person's and the agent's text, a tool call per line
    Transcript {
        id: Vec<String>,
        /// only the messages holding every one of these words (agents' text included)
        #[arg(long)]
        grep: Option<String>,
    },
    /// What a session's agent is told about earlier sessions when it starts (the recall block)
    Recall { id: Vec<String> },
    /// Write the jail's read-only snapshot anew (the shelf does this when a session ends)
    Refresh,
    /// Compaction and retention now, by the settings (the shelf runs this nightly)
    Compact,
    /// One Markdown file per session into a folder: facts, summary, notes, the conversation
    Export { dir: PathBuf },
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// Rebuild the compiled-in snapshot from the registry repository's current commit: every
    /// agent.json plus a resolved npm lockfile per package; review the diff, then commit it
    Snapshot {
        /// where to write (default: crates/sill-core/registry/snapshot.json of this checkout)
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum JailCmd {
    /// Print the bwrap command line for a jail
    Plan(RunArgs),
    /// Run a command in a jail (default: an interactive shell)
    Run(RunArgs),
    /// Copy one changed path from the session's upper tree to the host (SPEC §4.4 keep)
    Keep {
        #[command(flatten)]
        jail: JailArgs,
        /// host path to keep (as listed by `changes`)
        path: PathBuf,
        /// keep setuid/setgid bits
        #[arg(long)]
        allow_special: bool,
        /// don't save the host's previous version under the state dir's kept/ (no undo)
        #[arg(long)]
        no_undo: bool,
    },
    /// List the session's keeps that can still be undone (SPEC §4.4 keep with undo)
    Kept {
        #[command(flatten)]
        jail: JailArgs,
    },
    /// Put the host's previous version of a kept path back
    Unkeep {
        #[command(flatten)]
        jail: JailArgs,
        /// host path to restore (its most recent keep)
        path: Option<PathBuf>,
        /// undo the most recent keep
        #[arg(long, conflicts_with_all = ["path", "all"])]
        last: bool,
        /// undo every keep of the session, newest first
        #[arg(long, conflicts_with = "path")]
        all: bool,
        /// restore even if the host changed after the keep
        #[arg(long)]
        force: bool,
    },
    /// List what a jail's session has changed against the host (reads the upper trees)
    Changes {
        #[command(flatten)]
        jail: JailArgs,
        /// also list entries matching the policy's ledger_ignore patterns
        #[arg(long)]
        all: bool,
        /// print a unified diff for this host path
        #[arg(long)]
        diff: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DevCmd {
    /// Start the jail with sill-shim inside, detached; sockets under $XDG_RUNTIME_DIR/sill/dev
    Start {
        #[command(flatten)]
        jail: JailArgs,
        /// sill-shim binary to bind into the jail (default: next to this executable)
        #[arg(long)]
        shim: Option<PathBuf>,
    },
    /// Shim and agent status
    Status,
    /// Run one prompt in a fresh ACP session (starts the agent if needed)
    Prompt {
        text: String,
        /// agent command inside the jail
        #[arg(long, default_value = "claude-agent-acp")]
        agent: String,
        /// working directory for the agent (default: the jail's cwd given at start)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// kill a running agent first
        #[arg(long)]
        fresh: bool,
        /// answer permission requests with reject instead of allow-once
        #[arg(long)]
        reject: bool,
        /// print thoughts and raw updates too
        #[arg(short, long)]
        verbose: bool,
    },
    /// Restore a path inside the jail from the host (revert)
    Revert { path: PathBuf },
    /// Stop the agent and the jail
    Stop,
}

#[derive(Args, Clone)]
struct JailArgs {
    /// policy name or path
    #[arg(short, long, default_value = "default")]
    policy: String,
    /// session state dir (upper/work trees go below it)
    #[arg(short, long)]
    state_dir: Option<PathBuf>,
    /// accept a state dir inside an overlaid tree (development only)
    #[arg(long)]
    unsafe_state: bool,
    /// working directory inside the jail
    #[arg(short, long)]
    cwd: Option<PathBuf>,
    /// share the host network namespace (until the proxy exists)
    #[arg(long)]
    net: bool,
    /// extra environment, KEY=VALUE
    #[arg(short, long)]
    env: Vec<String>,
    /// host path of a running session proxy socket to bind at /.sill/proxy.sock (testing)
    #[arg(long)]
    proxy: Option<PathBuf>,
    /// bind this sill-shim binary at /.sill/shim (testing)
    #[arg(long)]
    shim_bin: Option<PathBuf>,
    /// extra read-write bind HOST=JAIL (agent home, private scratch); repeatable
    #[arg(long = "bind")]
    binds: Vec<String>,
    /// dev start: no systemd scope even where the user manager answers — the session cgroup under
    /// the `sill init` subtree, or no limits; the path of a host without systemd. Same as
    /// SILL_LAUNCHER=direct
    #[arg(long)]
    direct: bool,
    /// override the policy's desktop access: on | off (SPEC §4.2)
    #[arg(long)]
    x11: Option<String>,
    #[arg(long)]
    dbus: Option<String>,
    /// a ghost session (docs/memory.md §3): the policy tightened — `direct` as `ro`, no X11 or
    /// D-Bus, `all` network as the default allow-list — and no memory unless Settings allow it
    #[arg(long)]
    ghost: bool,
}

#[derive(Args, Clone)]
struct RunArgs {
    #[command(flatten)]
    jail: JailArgs,
    /// command (after --); default: $SHELL -l
    #[arg(last = true)]
    command: Vec<OsString>,
}

fn memory_cmd(cmd: MemoryCmd) -> Result<()> {
    let home = home()?;
    // the readers go through the daemon (docs/daemon.md decision 13), and so does compaction (the
    // daemon is the store's only writer, M1c); recall, export and refresh read the store below
    match cmd {
        MemoryCmd::Compact => {
            let c = sill_proto::Client::connect_or_start()?;
            let done: sill_proto::Compacted = c.call(sill_proto::method::MEMORY_COMPACT, sill_proto::Empty::default())?;
            println!("{} compacted, {} trimmed to summary and prompts, {} deleted", done.compacted, done.trimmed, done.deleted);
            return Ok(());
        }
        MemoryCmd::List => {
            let c = sill_proto::Client::connect_or_start()?;
            let rows: Vec<sill_proto::MemorySession> = c.call(sill_proto::method::MEMORY_LIST, sill_proto::Empty::default())?;
            if rows.is_empty() {
                println!("no ended sessions yet (a session ends when its tab is closed)");
            }
            for s in rows {
                println!("{}  {}  {:<8} {}  — {}", s.id, sill_memory::date(s.ended_at), s.agent, s.title, s.summary);
            }
            return Ok(());
        }
        MemoryCmd::Show { id } => {
            let c = sill_proto::Client::connect_or_start()?;
            let t: sill_proto::Text = c.call(sill_proto::method::MEMORY_SHOW, sill_proto::Key { key: id.join(" ") })?;
            print!("{}", t.text);
            return Ok(());
        }
        MemoryCmd::Search { words, folder, agent, since } => {
            let c = sill_proto::Client::connect_or_start()?;
            let t: sill_proto::Text = c.call(sill_proto::method::MEMORY_SEARCH, sill_proto::MemorySearch { words, folder, agent, since })?;
            print!("{}", t.text);
            return Ok(());
        }
        MemoryCmd::Transcript { id, grep } => {
            let c = sill_proto::Client::connect_or_start()?;
            let t: sill_proto::Text = c.call(sill_proto::method::MEMORY_TRANSCRIPT, sill_proto::Transcript { key: id.join(" "), grep })?;
            print!("{}", t.text);
            return Ok(());
        }
        _ => {}
    }
    let store = sill_core::store::Store::open_default()?;
    let conn = store.conn();
    let find = |key: &[String]| -> Result<sill_memory::SessionInfo> {
        let key = key.join(" ");
        sill_memory::find_session(conn, &key)?.with_context(|| format!("no session {key}"))
    };
    match cmd {
        MemoryCmd::List | MemoryCmd::Show { .. } | MemoryCmd::Search { .. } | MemoryCmd::Transcript { .. } | MemoryCmd::Compact => unreachable!(),
        MemoryCmd::Recall { id } => {
            let s = find(&id)?;
            let Some(full) = store.sessions()?.into_iter().find(|x| x.id == s.id) else { bail!("no session {}", s.id) };
            let pins = store.pins(&s.id)?;
            let block = sill_core::memory::recall(&store, &full, &pins, &home, sill_core::config::load().memory_recall());
            let scope = sill_memory::Scope::resolve(&full.memory_scope, !pins.is_empty());
            println!("scope: {}{}", scope.id(), if full.memory_scope.is_empty() { " (the default)" } else { "" });
            println!("{}", if block.is_empty() { "(nothing: memory is off)" } else { block.as_str() });
        }
        MemoryCmd::Export { dir } => {
            let n = sill_core::memory::export_all(&store, &dir, &home)?;
            println!("{n} sessions written to {}", dir.display());
        }
        MemoryCmd::Refresh => {
            drop(store);
            sill_core::memory::refresh_snapshot()?;
            let path = sill_core::memory::snapshot_dir().join(sill_memory::SNAPSHOT_FILE);
            if path.is_file() {
                println!("{}", path.display());
            } else {
                println!("memory is off: no snapshot");
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Policy { name } => {
            let p = Policy::load(&name, &home()?)?;
            print!("{}", toml::to_string_pretty(&p)?);
            Ok(())
        }
        Cmd::Memory { cmd } => memory_cmd(cmd),
        Cmd::Session { cmd } => session_cmd(cmd),
        Cmd::Pin { cmd } => pin_cmd(cmd.unwrap_or(PinCmd::List)),
        Cmd::Changes { cmd } => changes_cmd(cmd.unwrap_or(ChangesCmd::List)),
        Cmd::Attach { key, fresh, verbose } => tui::line_mode(&key, fresh, verbose),
        Cmd::Daemon { cmd } => daemon_cmd(cmd),
        Cmd::Registry { cmd: RegistryCmd::Snapshot { out } } => {
            let out = out.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sill-core/registry/snapshot.json"));
            sill_core::registry::build_snapshot(&out, &mut |line| eprintln!("{line}"))
        }
        Cmd::Jail { cmd } => jail_cmd(cmd),
        Cmd::Dev { cmd } => dev_cmd(cmd),
        Cmd::Launch { session, jail, shim, agent, child, cgroup_root } => launch(&session, jail, shim, agent.as_deref(), child, cgroup_root),
        Cmd::Init(a) => init::run(a),
        Cmd::Shelf { cwd, hidden } => shelf::run(cwd, hidden),
        Cmd::Toggle => shelf::toggle(),
        Cmd::Restart { hidden, dev, detached } => shelf::restart(hidden, dev, detached),
        Cmd::Theme { name } => shelf::set_theme(&name),
        Cmd::Hotkey { spec } => shelf::set_hotkey(&spec),
        Cmd::Login { agent, method } => {
            let home = home()?;
            let registry = sill_core::agents::load(&home);
            let Some(a) = registry.get(&agent) else { bail!("unknown agent {agent}") };
            let method = match method.or_else(|| a.host_login.first().cloned()) {
                Some(m) => m,
                None => bail!("{agent} has no host-side login method; set the key in its environment instead"),
            };
            sill_core::login::run(a, &home, &method, |line| match line {
                sill_core::login::Line::Status(t) => eprintln!("{t}"),
                sill_core::login::Line::Stderr(t) => eprintln!("\x1b[2m{t}\x1b[0m"),
            })?;
            eprintln!("logged in; running {agent} sessions pick it up when they start again");
            Ok(())
        }
        Cmd::Proxy { fd, socket, policy } => {
            let policy = Policy::load(&policy, &home()?)?;
            let listener = match socket {
                Some(path) => {
                    let _ = std::fs::remove_file(&path);
                    UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?
                }
                // SAFETY: the launcher put a listening Unix socket on this fd before exec.
                None => unsafe { UnixListener::from_raw_fd(fd) },
            };
            let rules = sill_core::proxy::Rules::from(&policy.network);
            eprintln!("sill proxy: mode {:?}, {} allow patterns", rules.mode, rules.allow.len());
            sill_core::proxy::serve(listener, rules, |line| eprintln!("sill proxy: {line}"))
        }
    }
}

// --- jail ----------------------------------------------------------------------------------------

fn jail_cmd(cmd: JailCmd) -> Result<()> {
    match cmd {
        JailCmd::Plan(a) => {
            let mut spec = build_spec(a.jail, a.command)?;
            let _seccomp = seccomp_fd(&mut spec)?; // so the plan shows the flag as `run` would pass it
            let plan = spec.plan()?;
            for w in &plan.warnings {
                eprintln!("warning: {w}");
            }
            println!("# layers:");
            for l in &plan.layers {
                println!("#   {}  upper={}", l.host.display(), l.upper.display());
            }
            println!("{}", jail::render(&plan.argv));
            Ok(())
        }
        JailCmd::Keep { mut jail, path, allow_special, no_undo } => {
            jail.unsafe_state = true;
            let spec = build_spec(jail, vec!["true".into()])?;
            let plan = spec.plan()?;
            let mut skip = plan.masked.clone();
            skip.extend(plan.direct.iter().cloned());
            let opts = ledger::ScanOptions { session_start: None, ignore: spec.policy.paths.ledger_ignore.clone(), skip };
            let changes = ledger::scan(&plan.layers, &opts)?;
            let path = std::path::absolute(path)?;
            let Some(c) = changes.iter().find(|c| c.path == path) else { bail!("{} is not changed", path.display()) };
            if c.conflict {
                bail!("{} changed on the host since the session started; not keeping", path.display());
            }
            let undo = if no_undo { None } else { Some(spec.state_dir.join("kept")) };
            let what = sill_core::keep::keep(c, &sill_core::keep::KeepOptions { expected_hash: None, allow_special, undo })?;
            println!("{}: {what}", path.display());
            Ok(())
        }
        JailCmd::Kept { mut jail } => {
            jail.unsafe_state = true; // reading only
            let spec = build_spec(jail, vec!["true".into()])?;
            let kept = sill_core::keep::undoable(&spec.state_dir.join("kept"));
            if kept.is_empty() {
                println!("nothing kept");
            }
            for k in kept {
                println!("{}  {:<9} {}  (was {})", k.ts, k.what, k.path.display(), k.before);
            }
            Ok(())
        }
        JailCmd::Unkeep { mut jail, path, last, all, force } => {
            jail.unsafe_state = true;
            let spec = build_spec(jail, vec!["true".into()])?;
            let dir = spec.state_dir.join("kept");
            let kept = sill_core::keep::undoable(&dir); // newest first
            let todo: Vec<_> = if all {
                kept
            } else if last {
                kept.into_iter().take(1).collect()
            } else if let Some(p) = path {
                let p = std::path::absolute(p)?;
                let Some(k) = kept.into_iter().find(|k| k.path == p) else { bail!("{} was not kept by this session", p.display()) };
                vec![k]
            } else {
                bail!("give a path, --last or --all");
            };
            if todo.is_empty() {
                bail!("nothing kept");
            }
            let mut failed = 0;
            for k in &todo {
                match sill_core::keep::undo(&dir, k, force) {
                    Ok(what) => println!("{}: {what}", k.path.display()),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}: {e:#}", k.path.display());
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} of {} not undone (--force restores over host edits)", todo.len());
            }
            Ok(())
        }
        JailCmd::Changes { mut jail, all, diff } => {
            jail.unsafe_state = true; // reading only; the plan's state check is for running
            let spec = build_spec(jail, vec!["true".into()])?;
            let plan = spec.plan()?;
            print_changes(&spec, &plan, all, diff)
        }
        JailCmd::Run(a) => {
            let mut spec = build_spec(a.jail, a.command)?;
            let _seccomp = seccomp_fd(&mut spec)?; // inherited by the exec below
            let plan = spec.plan()?;
            for w in &plan.warnings {
                eprintln!("sill: warning: {w}");
            }
            jail::prepare(&plan)?;
            let err = std::process::Command::new(&plan.argv[0]).args(&plan.argv[1..]).exec();
            bail!("exec bwrap: {err}")
        }
    }
}

fn print_changes(spec: &JailSpec, plan: &jail::JailPlan, all: bool, diff: Option<PathBuf>) -> Result<()> {
    let mut skip = plan.masked.clone();
    skip.extend(plan.direct.iter().cloned());
    let opts = ledger::ScanOptions { session_start: None, ignore: spec.policy.paths.ledger_ignore.clone(), skip };
    let changes = ledger::scan(&plan.layers, &opts)?;
    if let Some(path) = diff {
        let path = std::path::absolute(path)?;
        let Some(c) = changes.iter().find(|c| c.path == path) else { bail!("{} is not changed", path.display()) };
        let host = if path.exists() { path.as_os_str().to_owned() } else { "/dev/null".into() };
        let upper = if c.kind == ledger::Kind::Deleted { "/dev/null".into() } else { c.upper.as_os_str().to_owned() };
        let st = std::process::Command::new("diff")
            .args(["-u", "--label"])
            .arg(format!("host:{}", path.display()))
            .arg("--label")
            .arg(format!("jail:{}", path.display()))
            .arg(host)
            .arg(upper)
            .status()?;
        std::process::exit(st.code().unwrap_or(2));
    }
    for c in &changes {
        if c.ignored && !all {
            continue;
        }
        println!(
            "{}{} {:>9}  {}{}",
            c.kind.letter(),
            if c.conflict { '!' } else { ' ' },
            c.size,
            c.path.display(),
            if c.ignored { "  (ignored)" } else { "" }
        );
    }
    let hidden = changes.iter().filter(|c| c.ignored).count();
    if hidden > 0 && !all {
        eprintln!("({hidden} ignored entries not shown; --all lists them)");
    }
    Ok(())
}

// --- dev session ---------------------------------------------------------------------------------

const DEV: &str = "dev";

fn dev_cmd(cmd: DevCmd) -> Result<()> {
    let rt = shim::runtime_dir(DEV)?;
    let socket = rt.join("shim.sock");
    match cmd {
        DevCmd::Start { jail, shim: shim_bin } => {
            std::fs::create_dir_all(&rt)?;
            if socket.exists() && shim::Control::connect(&socket).is_ok() {
                bail!("a dev session is already running ({})", socket.display());
            }
            let _ = std::fs::remove_file(&socket);
            if jail.direct {
                // before the launcher is probed (first use is in ensure_started)
                // SAFETY: single-threaded at this point.
                unsafe { std::env::set_var("SILL_LAUNCHER", "direct") };
            }
            start_session(DEV, jail, shim_bin)
        }
        DevCmd::Status => {
            let pid = std::fs::read_to_string(rt.join("bwrap.pid")).ok();
            println!("bwrap pid: {}", pid.as_deref().unwrap_or("-"));
            let mut c = shim::Control::connect(&socket)?;
            let s = c.status()?;
            println!(
                "agent: {} pid={:?} exit={:?} buffered_lines={}",
                if s.running { "running" } else { "not running" },
                s.pid,
                s.exit,
                s.buffered_lines
            );
            Ok(())
        }
        DevCmd::Prompt { text, agent, cwd, fresh, reject, verbose } => {
            let mut c = shim::Control::connect(&socket)?;
            let cwd = match cwd {
                Some(c) => c,
                None => PathBuf::from(std::fs::read_to_string(rt.join("cwd")).context("dev session not started")?),
            };
            if fresh {
                c.kill(3000)?;
            }
            let st = c.status()?;
            if !st.running {
                let argv = shell_words(&agent);
                let pid = c.spawn(&argv, &BTreeMap::new(), Some(&cwd))?;
                eprintln!("agent started in the jail, pid {pid}");
            }
            let stream = shim::agent_stream(&socket)?;
            let auto = if reject { AutoPermission::Reject } else { AutoPermission::AllowOnce };
            use std::io::Write;
            acp::run_prompt(stream, &cwd, &text, auto, move |ev| {
                let mut out = std::io::stdout().lock();
                match ev {
                    Event::Text(t) => {
                        let _ = out.write_all(t.as_bytes());
                        let _ = out.flush();
                    }
                    Event::Thought(t) if verbose => {
                        let _ = write!(out, "\x1b[2m{t}\x1b[0m");
                    }
                    Event::ToolCall { title, status, .. } => {
                        let _ = writeln!(out, "\n\x1b[36m⚙ {title} [{status}]\x1b[0m");
                    }
                    Event::ToolCallUpdate { id, status, title, .. } if verbose => {
                        let _ = writeln!(out, "\x1b[36m  ↳ {id} {status:?} {title:?}\x1b[0m");
                    }
                    Event::PermissionRequest { title, reply, .. } => {
                        // unreachable with AllowOnce/Reject; answered here only for completeness
                        let _ = writeln!(out, "\n\x1b[33m? {title} → cancelled\x1b[0m");
                        let _ = reply.send(None);
                    }
                    Event::Other(s) if verbose => {
                        let _ = writeln!(out, "\x1b[2m· {s}\x1b[0m");
                    }
                    Event::AuthRequired { message, methods } => {
                        let names: Vec<String> = methods.iter().map(|m| format!("{} ({})", m.name, m.id)).collect();
                        let _ = writeln!(out, "\n\x1b[33mlogin needed: {message} — methods: {}\x1b[0m", names.join(", "));
                    }
                    Event::UserText(_) | Event::Ready { .. } | Event::TurnStarted | Event::Controls(_) => {}
                    Event::Stopped(r) => {
                        let _ = writeln!(out, "\n\x1b[2m[stop: {r:?}]\x1b[0m");
                    }
                    Event::Ended(Some(e)) => {
                        let _ = writeln!(out, "\x1b[31m{e}\x1b[0m");
                    }
                    _ => {}
                }
            })?;
            Ok(())
        }
        DevCmd::Revert { path } => {
            let mut c = shim::Control::connect(&socket)?;
            let path = std::path::absolute(path)?;
            println!("{}: {}", path.display(), c.revert(&path)?);
            Ok(())
        }
        DevCmd::Stop => {
            if let Ok(mut c) = shim::Control::connect(&socket) {
                match c.kill(5000) {
                    Ok(code) => println!("agent stopped (exit {code:?})"),
                    Err(e) => eprintln!("agent: {e}"),
                }
            }
            sill_core::session::stop(DEV)?;
            // a proxy of the pre-launcher dev path, if one is still around
            sill_core::launcher::kill_pid_file(&rt.join("proxy.pid"));
            let _ = std::fs::remove_file(rt.join("proxy.pid"));
            println!("dev session stopped");
            Ok(())
        }
    }
}

/// The systemd scope this process runs in, when it is one of sill's (`…/sill.slice/sill-<id>.scope`).
fn own_scope() -> Option<String> {
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cg.lines().find_map(|l| l.strip_prefix("0::"))?;
    let leaf = path.rsplit('/').next()?;
    (path.contains("/sill.slice/") && leaf.starts_with("sill-") && leaf.ends_with(".scope")).then(|| leaf.to_string())
}

/// `sill launch`: the process that owns a session on the host side — the daemon's child in its own
/// scope, or with `--cgroup-root` in a session cgroup it makes (docs/daemon.md §3.1); it records its
/// pids for re-adoption.
fn launch(session: &str, jail: JailArgs, shim_bin: Option<PathBuf>, agent: Option<&str>, child: bool, cgroup_root: Option<PathBuf>) -> Result<()> {
    let rt = shim::runtime_dir(session)?;
    std::fs::create_dir_all(&rt)?;
    if child {
        std::fs::write(rt.join("launch.pid"), std::process::id().to_string())?;
    }
    let shim_bin = match shim_bin {
        Some(p) => p,
        None => std::env::current_exe()?.with_file_name("sill-shim"),
    };
    if !shim_bin.is_file() {
        bail!("sill-shim not found at {}", shim_bin.display());
    }
    let use_proxy = !jail.net;
    let mut command: Vec<OsString> = vec!["/.sill/shim".into(), "--control-fd".into(), "3".into()];
    if use_proxy {
        command.extend(["--forward".into(), format!("127.0.0.1:{}={}", jail::PROXY_PORT, jail::PROXY_SOCKET).into()]);
    }
    let ghost = jail.ghost;
    let mut spec = build_spec(jail, command)?;
    // the other agents' homes and login files stay out of this jail (SPEC §4.11 #8)
    spec.masks = sill_core::session::other_agents_paths(&sill_core::agents::load(&spec.home), agent, &spec.home);
    let idle = spec.policy.limits.agent_idle_minutes as u64 * 60;
    spec.command.extend(["--agent-idle".into(), idle.to_string().into()]);
    // the shim, and the same binary as `sill-proc` on the jail's PATH (docs/tasks.md §2.4)
    spec.extra_args = vec!["--ro-bind".into(), shim_bin.clone().into(), "/.sill/shim".into(), "--ro-bind".into(), shim_bin.clone().into(), "/.sill/bin/sill-proc".into()];
    // memory (docs/memory.md §2.3): the snapshot's directory read-only, and the shim as
    // `sill-memory` to read it; made here if the shelf has not written one yet, so the first
    // snapshot shows up in jails already running
    let cfg = sill_core::config::load();
    if if ghost { cfg.ghost_memory() } else { cfg.memory() } {
        let dir = sill_core::memory::snapshot_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            spec.memory_dir = Some(dir);
            spec.extra_args.extend(["--ro-bind".into(), shim_bin.into(), "/.sill/bin/sill-memory".into()]);
        }
    }

    let shim_sock = rt.join("shim.sock");
    let proxy_sock = rt.join("proxy.sock");
    let _ = std::fs::remove_file(&shim_sock);
    let _ = std::fs::remove_file(&proxy_sock);
    let mut proxy_thread = None;
    if use_proxy {
        let listener = UnixListener::bind(&proxy_sock).with_context(|| format!("bind {}", proxy_sock.display()))?;
        let mut rules = sill_core::proxy::Rules::from(&spec.policy.network);
        // the agent's own endpoints (registry `domains`) on top of the policy's list
        if let Some(a) = agent.and_then(|id| sill_core::agents::load(&home().ok()?).remove(id)) {
            rules.allow.extend(a.domains);
        }
        eprintln!("sill launch: proxy mode {:?}, {} allow patterns", rules.mode, rules.allow.len());
        // refused hosts also go to <runtime dir>/denied, one per line: the shelf watches it and
        // offers to allow the host for this agent (SPEC §4.8)
        let denied = rt.join("denied");
        let _ = std::fs::remove_file(&denied);
        proxy_thread = Some(std::thread::spawn(move || {
            sill_core::proxy::serve(listener, rules, move |l| {
                // stderr is the session's journald stream or its jail.log (docs/daemon.md §3.1)
                eprintln!("proxy: {l}");
                // `deny host:port (why)`: the host alone when the port is the default one a bare
                // pattern grants, `host:port` otherwise — what the shelf offers is what would allow
                // it. Only a plain host name (or IP) gets there: a wildcard or garbage in the
                // request line stays in the log and never becomes a button (security.md S7)
                if let Some(rest) = l.strip_prefix("deny ")
                    && let Some(target) = rest.split_whitespace().next()
                    && let Some((host, port)) = target.rsplit_once(':')
                    && sill_core::proxy::valid_hostname(host)
                    && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&denied)
                {
                    use std::io::Write;
                    let _ = if port == sill_core::proxy::CONNECT_PORT.to_string() || port == sill_core::proxy::HTTP_PORT.to_string() {
                        writeln!(f, "{host}")
                    } else {
                        writeln!(f, "{target}")
                    };
                }
            })
        }));
        spec.proxy_socket = Some(proxy_sock);
    }
    let seccomp = seccomp_fd(&mut spec)?;
    let plan = spec.plan()?;
    for w in &plan.warnings {
        eprintln!("sill launch: warning: {w}");
    }
    jail::prepare(&plan)?;
    std::fs::write(rt.join("cwd"), spec.cwd.to_string_lossy().as_bytes())?;
    std::fs::write(rt.join("state_dir"), spec.state_dir.to_string_lossy().as_bytes())?;

    // limits: the session cgroup under the daemon's root, bwrap moves itself in before exec so
    // every process of the jail is born inside it
    let mut cgroup_procs: Option<PathBuf> = None;
    if child {
        match &cgroup_root {
            Some(root) => {
                let mut warnings = Vec::new();
                let dir = sill_core::launcher::make_session_cgroup(root, session, &spec.policy.limits, &mut warnings)?;
                for w in warnings {
                    eprintln!("sill launch: warning: {w}");
                }
                eprintln!("sill launch: limits in cgroup {}", dir.display());
                cgroup_procs = Some(dir.join("cgroup.procs"));
            }
            // a scope made by the daemon (`systemd-run --scope`) carries the limits already
            None => match own_scope() {
                Some(scope) => eprintln!("sill launch: limits on {scope}"),
                None => eprintln!("sill launch: warning: no resource limits — a busy agent can starve the host"),
            },
        }
    }

    let listener = UnixListener::bind(&shim_sock).with_context(|| format!("bind {}", shim_sock.display()))?;
    let listener_fd = listener.as_fd().as_raw_fd();
    // A jail that dies the moment it starts is almost always the overlay refusing its workdir while
    // the previous jail of the same session is still being torn down (a restart right after a stop:
    // "Can't make overlay mount … Device or resource busy"); the teardown takes a moment, so try again.
    let mut attempt = 0;
    let status = loop {
        attempt += 1;
        let mut cmd = std::process::Command::new(&plan.argv[0]);
        cmd.args(&plan.argv[1..]).stdin(std::process::Stdio::null());
        if let Some(f) = &seccomp {
            sill_core::seccomp::rewind(f)?; // bwrap read it to the end last time
        }
        let cgroup_procs = cgroup_procs.clone();
        // SAFETY: only dup2/fcntl/open/write/getpid between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                let src = std::os::fd::BorrowedFd::borrow_raw(listener_fd);
                if listener_fd == 3 {
                    rustix::io::fcntl_setfd(src, rustix::io::FdFlags::empty())?;
                } else {
                    let mut fd3 = OwnedFd::from_raw_fd(3);
                    rustix::io::dup2(src, &mut fd3)?;
                    std::mem::forget(fd3);
                }
                if let Some(procs) = &cgroup_procs {
                    let f = rustix::fs::open(procs, rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty())?;
                    let pid = rustix::process::getpid().as_raw_nonzero().get().to_string();
                    rustix::io::write(&f, pid.as_bytes())?;
                }
                Ok(())
            });
        }
        let mut bwrap = cmd.spawn().context("spawn bwrap")?;
        if child {
            std::fs::write(rt.join("bwrap.pid"), bwrap.id().to_string())?;
        }
        eprintln!("sill launch: jail started (bwrap pid {}), sockets in {}", bwrap.id(), rt.display());
        let started = std::time::Instant::now();
        let status = bwrap.wait()?;
        if !status.success() && started.elapsed() < std::time::Duration::from_secs(2) && attempt < 6 {
            eprintln!("sill launch: jail ended at once ({status}); the previous jail may still be going away, retrying ({attempt}/5)");
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        break status;
    };
    drop(listener);
    eprintln!("sill launch: jail ended ({status})");
    let _ = std::fs::remove_file(&shim_sock);
    let _ = std::fs::remove_file(rt.join("proxy.sock"));
    if child {
        let _ = std::fs::remove_file(rt.join("bwrap.pid"));
        let _ = std::fs::remove_file(rt.join("launch.pid"));
        if let Some(root) = &cgroup_root {
            let _ = std::fs::remove_dir(root.join(format!("sill-{session}")));
        }
    }
    drop(proxy_thread); // the proxy thread dies with the process
    Ok(())
}

/// Start a session through sill-core `session` in whichever launcher mode this host offers.
fn start_session(session: &str, jail: JailArgs, shim_bin: Option<PathBuf>) -> Result<()> {
    let opts = sill_core::session::StartOptions {
        session: session.to_string(),
        policy: jail.policy.clone(),
        cwd: match jail.cwd { Some(c) => c, None => std::env::current_dir()? },
        state_dir: jail.state_dir.clone(),
        unsafe_state: jail.unsafe_state,
        share_net: jail.net,
        env: jail.env.clone(),
        shim_bin,
        agent: None,
        x11: jail.x11.as_deref().map(|v| v == "on"),
        dbus: jail.dbus.as_deref().map(|v| v == "on"),
        ghost: jail.ghost,
        binds: jail
            .binds
            .iter()
            .map(|b| match b.split_once('=') {
                Some((h, j)) => Ok((PathBuf::from(h), PathBuf::from(j))),
                None => bail!("--bind expects HOST=JAIL, got {b}"),
            })
            .collect::<Result<Vec<_>>>()?,
    };
    // a dev session is this terminal's child, not the daemon's: it runs on after this command
    // (its own session and scope)
    let launcher = sill_core::launcher::current();
    let started = sill_core::session::ensure_started(&opts, &home()?, &std::env::current_exe()?)?;
    println!("session {session} started; socket {}", started.socket.display());
    println!("logs: {}", sill_core::session::log_hint(session));
    if !launcher.has_limits() {
        println!("warning: no resource limits — a busy agent can starve the host ({})", launcher.reason);
    }
    std::mem::forget(started.child); // runs on; nobody here waits for it
    Ok(())
}

fn shell_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

// --- shared --------------------------------------------------------------------------------------

fn changes_cmd(cmd: ChangesCmd) -> Result<()> {
    use sill_proto::method as m;
    let c = sill_proto::Client::connect_or_start()?;
    let session = |key: &str| -> Result<String> { Ok(c.call::<sill_proto::Session>(m::SESSIONS_GET, sill_proto::Key { key: key.to_string() })?.id) };
    let abs = |paths: Vec<PathBuf>| -> Result<Vec<PathBuf>> {
        let cwd = std::env::current_dir()?;
        Ok(paths.into_iter().map(|p| if p.is_absolute() { p } else { cwd.join(p) }).collect())
    };
    let report = |d: sill_core::wire::Done| {
        for (p, what) in &d.done {
            println!("{}: {what}", p.display());
        }
        for (p, e) in &d.failed {
            eprintln!("{}: {e}", p.display());
        }
        if d.failed.is_empty() { Ok(()) } else { Err(anyhow::anyhow!("{} failed", d.failed.len())) }
    };
    match cmd {
        ChangesCmd::List => {
            let l: sill_core::wire::ChangesList = c.call(m::CHANGES_LIST, sill_proto::Empty::default())?;
            for r in &l.live {
                println!("{} (pinned, git)", r.repo.display());
                for (rel, st) in &r.status {
                    println!("  {st:<2} {}", rel.display());
                }
            }
            for s in &l.sessions {
                println!("{}  {}{}", s.id, if s.ghost { "ghost " } else { "" }, s.title);
                for r in s.changes.iter().chain(&s.agent_state) {
                    let ch = &r.change;
                    println!("  {}{} {}", ch.kind.letter(), if ch.conflict { "!" } else { " " }, ch.path.display());
                }
                for k in &s.kept {
                    println!("  kept {} {} ({})", k.id, k.path.display(), k.what);
                }
            }
            Ok(())
        }
        ChangesCmd::Diff { session: key, path } => {
            let path = abs(vec![path])?.remove(0);
            let t: sill_proto::Text = c.call(m::CHANGES_DIFF, sill_core::wire::DiffOf::Change { session: session(&key)?, path })?;
            print!("{}", t.text);
            Ok(())
        }
        ChangesCmd::Keep { session: key, paths } => {
            // what the person names is what is kept: no listed hash to hold it to
            let items = abs(paths)?.into_iter().map(|p| (p, None)).collect();
            report(c.call(m::CHANGES_KEEP, sill_proto::ChangesKeep { session: session(&key)?, items })?)
        }
        ChangesCmd::Revert { session: key, paths } => report(c.call(m::CHANGES_REVERT, sill_proto::ChangesPaths { session: session(&key)?, paths: abs(paths)? })?),
        ChangesCmd::History { since, path, agent, kind, session: key } => {
            let since = match since {
                Some(v) => Some(sill_core::store::now() - sill_memory::parse_age(&v).context("--since expects 7d, 12h, 2w …")?),
                None => None,
            };
            let kind = match kind {
                Some(k) => Some(sill_core::history::Kind::parse(&k).with_context(|| format!("no kind {k}"))?),
                None => None,
            };
            let path = match path {
                Some(p) => Some(abs(vec![p])?.remove(0)),
                None => None,
            };
            let session = match key {
                Some(k) => Some(session(&k)?),
                None => None,
            };
            let f = sill_core::history::Filter { since, path, agent, kind, session, limit: 0 };
            let rows: Vec<sill_core::history::Event> = c.call(m::CHANGES_HISTORY, f)?;
            if rows.is_empty() {
                println!("nothing in the history");
            }
            let home = home()?;
            for e in rows {
                print!("{}", sill_memory::history::line(&e, &home));
            }
            Ok(())
        }
        ChangesCmd::Undo { session: key, ids, force } => report(c.call(m::CHANGES_UNDO, sill_proto::ChangesUndo { session: session(&key)?, ids, force })?),
    }
}

fn pin_cmd(cmd: PinCmd) -> Result<()> {
    let c = sill_proto::Client::connect_or_start()?;
    match cmd {
        PinCmd::List => {
            let pins: Vec<PathBuf> = c.call(sill_proto::method::PINS_LIST, sill_proto::Empty::default())?;
            for p in pins {
                println!("{}", p.display());
            }
        }
        PinCmd::Add { path } => {
            let path = path.canonicalize().with_context(|| format!("pin {}", path.display()))?;
            let _: serde_json::Value = c.call(sill_proto::method::PINS_ADD, sill_proto::GlobalPin { path: path.clone() })?;
            println!("{} pinned globally", path.display());
        }
        PinCmd::Remove { path } => {
            let path = path.canonicalize().unwrap_or(path);
            let _: serde_json::Value = c.call(sill_proto::method::PINS_REMOVE, sill_proto::GlobalPin { path: path.clone() })?;
            println!("{} removed from the global pins", path.display());
        }
    }
    Ok(())
}

fn session_cmd(cmd: SessionCmd) -> Result<()> {
    let c = sill_proto::Client::connect_or_start()?;
    match cmd {
        SessionCmd::New { agent, policy, ghost, pins } => {
            let pins = pins.into_iter().map(|p| p.canonicalize().with_context(|| format!("pin {}", p.display()))).collect::<Result<Vec<_>>>()?;
            let new = sill_proto::NewSession { agent, policy, pins, ghost, ..Default::default() };
            let k: sill_proto::Key = c.call(sill_proto::method::SESSIONS_NEW, new)?;
            let started: sill_proto::Started = c.call(sill_proto::method::SESSIONS_START, sill_proto::Key { key: k.key.clone() })?;
            println!("{}", k.key);
            eprintln!("{}session {} started; socket {}", if ghost { "ghost " } else { "" }, k.key, started.socket.display());
        }
        SessionCmd::List => {
            let rows: Vec<sill_proto::Session> = c.call(sill_proto::method::SESSIONS_LIST, sill_proto::Empty::default())?;
            for s in rows {
                let state = if s.running { "running" } else if s.ended_at.is_some() { "ended" } else { "open" };
                println!("{}  {:<7} {}{:<8} {}", s.id, state, if s.ghost { "ghost " } else { "" }, s.agent, s.title);
            }
        }
        SessionCmd::End { key } => {
            let s: sill_proto::Session = c.call(sill_proto::method::SESSIONS_GET, sill_proto::Key { key: key.join(" ") })?;
            let e: sill_proto::Ending = c.call(sill_proto::method::SESSIONS_END, sill_proto::End { key: s.id.clone(), reason: "close".into() })?;
            match (e.pending, e.deleted) {
                (true, _) => println!("{}: the agent writes its summary; the session ends when it is done", s.id),
                (false, true) => println!("{}: deleted ({})", s.id, if s.ghost { "a ghost leaves nothing" } else { "nothing was asked in it" }),
                (false, false) => println!("{}: ended", s.id),
            }
        }
    }
    Ok(())
}

fn daemon_cmd(cmd: Option<DaemonCmd>) -> Result<()> {
    match cmd {
        None => daemon::run(),
        Some(DaemonCmd::Status) => {
            let c = sill_proto::Client::connect_or_start()?;
            let s: sill_proto::DaemonStatus = c.call_within(sill_proto::method::DAEMON_STATUS, sill_proto::Empty::default(), sill_proto::client::QUICK_TIMEOUT)?;
            println!("sill daemon {} (protocol {}), pid {}, {}, up since {}", s.version, s.protocol, s.pid, s.mode, sill_memory::datetime(s.started));
            println!("jails:    {}", s.launcher);
            println!("socket:   {}", s.socket);
            println!("sessions: {} in the store, {} running", s.sessions, s.running);
            Ok(())
        }
        Some(DaemonCmd::Schema) => {
            println!("{}", serde_json::to_string_pretty(&sill_proto::schema::document())?);
            Ok(())
        }
        Some(DaemonCmd::Stop) => match sill_proto::Client::connect() {
            Ok(c) => {
                let _: serde_json::Value = c.call_within(sill_proto::method::DAEMON_SHUTDOWN, sill_proto::Empty::default(), sill_proto::client::QUICK_TIMEOUT)?;
                println!("asked the daemon to leave; sessions keep running");
                Ok(())
            }
            Err(_) => {
                println!("no daemon running");
                Ok(())
            }
        },
    }
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME not set")?))
}

/// The seccomp program for this jail as an inheritable fd, recorded in the spec for `plan()`; kept
/// alive by the caller until bwrap has started. Never fd 3: the shim's control socket is dup'd there.
fn seccomp_fd(spec: &mut JailSpec) -> Result<Option<OwnedFd>> {
    if !spec.policy.jail.seccomp {
        return Ok(None);
    }
    let fd = match sill_core::seccomp::memfd(spec.policy.jail.allow_userns)? {
        Some(fd) if fd.as_raw_fd() == 3 => {
            let moved = rustix::io::dup(&fd)?;
            rustix::io::fcntl_setfd(&moved, rustix::io::FdFlags::empty())?;
            moved
        }
        Some(fd) => fd,
        None => {
            eprintln!("sill: warning: no seccomp filter for this architecture");
            return Ok(None);
        }
    };
    spec.seccomp_fd = Some(fd.as_raw_fd());
    Ok(Some(fd))
}

fn build_spec(a: JailArgs, command: Vec<OsString>) -> Result<JailSpec> {
    let home = home()?;
    let uid = rustix::process::getuid().as_raw();
    let user = std::env::var("USER").unwrap_or_else(|_| uid.to_string());
    let mut policy = Policy::load(&a.policy, &home)?;
    if let Some(v) = &a.x11 {
        policy.jail.x11 = v == "on";
    }
    if let Some(v) = &a.dbus {
        policy.jail.dbus = v == "on";
    }
    // after the overrides: a ghost's desktop access is off whatever the session asked for
    if a.ghost {
        policy.ghostly(&home);
    }
    let state_dir = match a.state_dir {
        Some(d) => d,
        None => default_state_dir(uid, &home)?,
    };
    let mut env = BTreeMap::new();
    for kv in &a.env {
        let Some((k, v)) = kv.split_once('=') else { bail!("--env expects KEY=VALUE, got {kv}") };
        env.insert(k.to_string(), v.to_string());
    }
    let cwd = match a.cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };
    let command = if command.is_empty() {
        vec![OsString::from(std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())), "-l".into()]
    } else {
        command
    };
    let mut extra_args = vec![];
    if let Some(shim) = &a.shim_bin {
        extra_args.extend(["--ro-bind".into(), shim.clone().into(), "/.sill/shim".into(), "--ro-bind".into(), shim.clone().into(), "/.sill/bin/sill-proc".into()]);
    }
    let mut binds = Vec::new();
    for b in &a.binds {
        let Some((h, j)) = b.split_once('=') else { bail!("--bind expects HOST=JAIL, got {b}") };
        binds.push((PathBuf::from(h), PathBuf::from(j)));
    }
    Ok(JailSpec {
        policy,
        home,
        user,
        uid,
        state_dir: std::path::absolute(state_dir)?,
        unsafe_state: a.unsafe_state,
        cwd: std::path::absolute(cwd)?,
        env,
        share_net: a.net,
        mounts: mounts::host_mounts()?,
        root_entries: jail::read_root_entries()?,
        proxy_socket: a.proxy.map(std::path::absolute).transpose()?,
        proxy_port: jail::PROXY_PORT,
        binds,
        masks: vec![],
        memory_dir: None,
        seccomp_fd: None,
        extra_args,
        command,
    })
}

/// `/var/lib/sill/<uid>/sessions/dev` when the sill state filesystem is mounted, otherwise the user's
/// state dir (which `plan()` refuses without `--unsafe-state`).
fn default_state_dir(uid: u32, home: &Path) -> Result<PathBuf> {
    let state_fs = Path::new(jail::STATE_FS);
    let all = mounts::host_mounts()?;
    if mounts::is_mount_point(&all, state_fs) {
        // the state fs is sticky 1777 (like /tmp): each user owns a private 0700 dir in it
        let mine = state_fs.join(uid.to_string());
        if !mine.is_dir() {
            std::fs::create_dir(&mine).with_context(|| format!("create {}", mine.display()))?;
        }
        std::fs::set_permissions(&mine, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        return Ok(mine.join("sessions/dev"));
    }
    let xdg = std::env::var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/state"));
    Ok(xdg.join("sill/sessions/dev"))
}

use std::os::fd::AsRawFd;
