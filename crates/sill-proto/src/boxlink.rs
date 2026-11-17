//! One shared SSH connection per box (docs/sill-gui.md, "Box restart recovery").
//!
//! Every protocol connection and byte stream to a box is an `ssh` process of its own that rides
//! one authenticated *master* connection. This module is the only thing that makes that master:
//! serialised in this process by a mutex and between processes by a lock file, so two starts
//! never race for the control socket. Session connections never become a master themselves
//! (`ControlMaster=no`, `transport::ssh_base`).
//!
//! The master is rebuilt unattended when SSH can log in without asking (an agent, an unencrypted
//! key). When SSH needs the person — a key passphrase, a password, a host-key question — the box
//! is marked *sign-in required* and nothing retries in the background until `sign_in` succeeds;
//! network failures retry with a box-wide backoff, so open sessions do not each hammer the box.
//! Nothing typed at a prompt is stored or logged: it goes to OpenSSH and nowhere else.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How long a verified master is trusted before `ssh -O check` is asked again.
const FRESH: Duration = Duration::from_secs(5);
/// An unattended master start: `ConnectTimeout` bounds the network part, this bounds all of it.
const BATCH_DEADLINE: Duration = Duration::from_secs(40);
/// The master leaves this long after its last sill connection closed, so reopening the window
/// soon after needs no new sign-in.
const PERSIST_SECS: u32 = 600;
/// The helper's environment: where the window listens for SSH's questions.
pub const ASKPASS_SOCKET: &str = "SILL_ASKPASS_SOCKET";

/// Why a box cannot be reached right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// SSH needs the person: a key passphrase, a password, a second factor.
    SignIn,
    /// SSH refused the box's host key (unknown in a background attempt, or changed).
    HostKey,
    /// The network or the machine: retried with backoff.
    Offline,
    /// Anything else SSH said; retried with backoff.
    Other,
}

/// What the window shows about the box: one line for the whole box, not one per session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub host: String,
    pub problem: Problem,
    /// SSH's own last message.
    pub detail: String,
    /// When the next background attempt is due; None while none is planned.
    pub retry_in: Option<Duration>,
    pub signing_in: bool,
}

/// The error every connection attempt gets while the box is unavailable.
#[derive(Debug, Clone)]
pub struct Unavailable {
    pub host: String,
    pub problem: Problem,
    pub detail: String,
    pub retry_in: Option<Duration>,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { host, detail, .. } = self;
        match self.problem {
            Problem::SignIn => write!(f, "box {host} needs a sign-in ({detail}): use Sign in in the window, or run `sill box add {host}` in a terminal"),
            Problem::HostKey => write!(
                f,
                "SSH does not trust the host key of box {host} ({detail}): sign in to answer SSH's question, or check it with `ssh {host}` in a terminal; nothing retries by itself"
            ),
            Problem::Offline | Problem::Other => {
                write!(f, "box {host} is unreachable ({detail})")?;
                match self.retry_in {
                    Some(wait) => write!(f, "; trying again in {} s", wait.as_secs().max(1)),
                    None => Ok(()),
                }
            }
        }
    }
}
impl std::error::Error for Unavailable {}

/// One question from SSH, shown by the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    pub host: String,
    /// SSH's own words.
    pub text: String,
    pub kind: PromptKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptKind {
    /// A passphrase, password or code: typed hidden.
    Secret,
    /// A typed answer shown as typed (the host-key question's yes / no / fingerprint).
    Text,
    /// Yes or no.
    Confirm,
    /// Information only (touch the hardware key); no answer.
    Notice,
}

/// Shows a prompt and returns the answer; None when the person cancelled. Blocks its thread.
pub type Prompter = Arc<dyn Fn(Prompt) -> Option<String> + Send + Sync>;

#[derive(Default)]
struct Link {
    verified: Option<Instant>,
    problem: Option<(Problem, String)>,
    attempt: u32,
    retry_at: Option<Instant>,
}

#[derive(Default)]
struct Shown {
    problem: Option<(Problem, String)>,
    retry_at: Option<Instant>,
    signing_in: bool,
}

/// Per host: held for the whole of a check or a master start, so one thread does it at a time.
static LINKS: Mutex<BTreeMap<String, Arc<Mutex<Link>>>> = Mutex::new(BTreeMap::new());
/// Per host: what `status` reports. Never held across an `ssh` run, so a view can read it.
static SHOWN: Mutex<BTreeMap<String, Shown>> = Mutex::new(BTreeMap::new());

fn link(host: &str) -> Arc<Mutex<Link>> {
    LINKS.lock().unwrap().entry(host.to_string()).or_default().clone()
}

fn show(host: &str, change: impl FnOnce(&mut Shown)) {
    change(SHOWN.lock().unwrap().entry(host.to_string()).or_default());
}

impl Link {
    fn connected(&mut self, host: &str) {
        *self = Link { verified: Some(Instant::now()), ..Link::default() };
        show(host, |s| {
            s.problem = None;
            s.retry_at = None;
        });
    }

    fn failed(&mut self, host: &str, problem: Problem, detail: String) -> anyhow::Error {
        self.verified = None;
        self.retry_at = match problem {
            // the person's move: no background attempt can succeed
            Problem::SignIn | Problem::HostKey => None,
            Problem::Offline | Problem::Other => {
                let wait = backoff(self.attempt);
                self.attempt = self.attempt.saturating_add(1);
                Some(Instant::now() + wait)
            }
        };
        self.problem = Some((problem, detail));
        let (problem, retry_at) = (self.problem.clone(), self.retry_at);
        show(host, |s| {
            s.problem = problem;
            s.retry_at = retry_at;
        });
        self.unavailable(host)
    }

    fn unavailable(&self, host: &str) -> anyhow::Error {
        let (problem, detail) = self.problem.clone().unwrap_or((Problem::Other, "no shared SSH connection".into()));
        anyhow::Error::new(Unavailable {
            host: host.to_string(),
            problem,
            detail,
            retry_in: self.retry_at.map(|at| at.saturating_duration_since(Instant::now())),
        })
    }
}

/// 2 s doubling to a minute, a fifth either way so several windows do not knock together.
fn backoff(attempt: u32) -> Duration {
    let base = 2u64.saturating_mul(1 << attempt.min(5)).min(60) * 1000;
    let noise = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as u64);
    Duration::from_millis(base * 4 / 5 + noise % (base * 2 / 5 + 1))
}

/// The box's trouble, if any, for the window's one indicator. With several boxes known to this
/// process, the first in trouble.
pub fn status() -> Option<Status> {
    let now = Instant::now();
    SHOWN.lock().unwrap().iter().find_map(|(host, shown)| {
        let (problem, detail) = shown.problem.clone()?;
        Some(Status { host: host.clone(), problem, detail, retry_in: shown.retry_at.map(|at| at.saturating_duration_since(now)), signing_in: shown.signing_in })
    })
}

/// The master is there, or is made now when SSH can log in without asking. Called before every
/// connection to the box; fails at once, without touching the network, while the box waits for
/// a sign-in or for its next retry.
pub fn ensure(host: &str) -> Result<()> {
    let link = link(host);
    let mut link = link.lock().unwrap();
    if link.verified.is_some_and(|at| at.elapsed() < FRESH) {
        return Ok(());
    }
    if check(host)? {
        link.connected(host);
        return Ok(());
    }
    link.verified = None;
    match &link.problem {
        Some((Problem::SignIn | Problem::HostKey, _)) => return Err(link.unavailable(host)),
        _ if link.retry_at.is_some_and(|at| at > Instant::now()) => return Err(link.unavailable(host)),
        _ => {}
    }
    let _exclusive = exclusive(host)?;
    // another process may have made it while this one waited
    if check(host)? {
        link.connected(host);
        return Ok(());
    }
    match start_master(host, Mode::Batch) {
        Ok(()) => {
            link.connected(host);
            Ok(())
        }
        Err((problem, detail)) => Err(link.failed(host, problem, detail)),
    }
}

/// The one interactive path. With a prompter, SSH's questions go to it (the window); without,
/// SSH asks on this terminal. Returns once the master is there: true when this call made it,
/// false when one was already there (another window's or command's, which its caller must leave).
pub fn sign_in(host: &str, prompter: Option<Prompter>) -> Result<bool> {
    show(host, |s| s.signing_in = true);
    let result = (|| {
        let link = link(host);
        let mut link = link.lock().unwrap();
        let _exclusive = exclusive(host)?;
        if check(host)? {
            link.connected(host);
            return Ok(false);
        }
        let mode = match prompter {
            Some(prompter) => Mode::Ask(prompter),
            None => Mode::Terminal,
        };
        match start_master(host, mode) {
            Ok(()) => {
                link.connected(host);
                Ok(true)
            }
            Err((problem, detail)) => Err(link.failed(host, problem, detail)),
        }
    })();
    show(host, |s| s.signing_in = false);
    result
}

/// Retry now: the next `ensure` tries at once. A box waiting for a sign-in keeps waiting.
pub fn retry_now(host: &str) {
    let link = link(host);
    let mut link = link.lock().unwrap();
    if matches!(link.problem, Some((Problem::Offline | Problem::Other, _))) {
        link.retry_at = None;
        show(host, |s| s.retry_at = None);
    }
}

/// A connection over the master failed with `said` on its stderr. When the master is gone, what
/// SSH said decides the box's state (the session's own attempt was the background retry).
pub fn session_failed(host: &str, said: &str) {
    let link = link(host);
    let mut link = link.lock().unwrap();
    link.verified = None;
    if check(host).unwrap_or(false) {
        return;
    }
    if let Some(problem) = classify(said) {
        let _ = link.failed(host, problem, last_line(said));
    }
}

/// End the shared connection (and with it every sill connection to the box from this account).
pub fn disconnect(host: &str) -> Result<()> {
    let link = link(host);
    let mut link = link.lock().unwrap();
    let _exclusive = exclusive(host)?;
    let _ = control(host)?.args(["-O", "exit", "--", host]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    *link = Link::default();
    show(host, |s| *s = Shown::default());
    Ok(())
}

/// A key or agent logs in to `host` without asking, on a connection of its own. False means every
/// rebuilt master needs the person.
pub fn unattended(host: &str) -> Result<bool> {
    let mut command = crate::transport::ssh_program(host);
    command
        .args(["-T", "-o", "BatchMode=yes", "-o", "ControlMaster=no", "-o", "ControlPath=none", "-o", "ConnectTimeout=10", "--", host, "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().context("starting ssh")?;
    Ok(wait(&mut child, Some(BATCH_DEADLINE), None).is_some_and(|status| status.success()))
}

fn dir() -> Result<PathBuf> {
    let dir = crate::runtime_dir()?.join("ssh");
    std::fs::create_dir_all(&dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// `-o ControlPath=…`, the same for the master and everything riding it.
pub(crate) fn control_path() -> Result<String> {
    Ok(format!("ControlPath={}/%C", dir()?.display()))
}

fn control(host: &str) -> Result<Command> {
    crate::transport::validate_host(host)?;
    let mut command = crate::transport::ssh_program(host);
    command.arg("-o").arg(control_path()?);
    Ok(command)
}

fn check(host: &str) -> Result<bool> {
    Ok(control(host)?
        .args(["-O", "check", "--", host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("starting ssh")?
        .success())
}

/// One master start at a time across processes (a window and a CLI command, two windows).
fn exclusive(host: &str) -> Result<std::fs::File> {
    let name: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir()?.join(format!("{name}.lock")))?;
    file.lock().context("locking the box's shared SSH connection")?;
    Ok(file)
}

enum Mode {
    Batch,
    Terminal,
    Ask(Prompter),
}

/// Start the master and leave it in the background: `ssh … true` authenticates, runs nothing and
/// returns, and ControlPersist keeps the connection. A longer keepalive budget than a session's,
/// because losing the master may cost the person a sign-in: two minutes without an answer.
fn start_master(host: &str, mode: Mode) -> Result<(), (Problem, String)> {
    let other = |e: anyhow::Error| (Problem::Other, format!("{e:#}"));
    let mut command = control(host).map_err(other)?;
    command.args([
        "-T",
        "-o",
        "ControlMaster=yes",
        "-o",
        &format!("ControlPersist={PERSIST_SECS}"),
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=8",
        "-o",
        if matches!(mode, Mode::Batch) { "BatchMode=yes" } else { "BatchMode=no" },
    ]);
    command.args(["--", host, "true"]);
    if matches!(mode, Mode::Terminal) {
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| other(anyhow::Error::new(e).context("starting ssh")))?;
        if !status.success() {
            return Err((Problem::Other, format!("SSH authentication or connection failed ({status}); see SSH's message above")));
        }
        return verify(host);
    }
    // stderr to a file, not a pipe: nothing can keep a pipe's end open behind our back
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let said_path = dir().map_err(other)?.join(format!("said-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let said_file = std::fs::File::create(&said_path).map_err(|e| other(e.into()))?;
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(said_file);
    let cancelled = Arc::new(AtomicBool::new(false));
    let asking = match &mode {
        Mode::Ask(prompter) => {
            let asking = Asking::listen(host, prompter.clone(), cancelled.clone()).map_err(other)?;
            let helper = std::env::current_exe().map_err(|e| other(e.into()))?;
            command.env("SSH_ASKPASS", helper).env("SSH_ASKPASS_REQUIRE", "force").env(ASKPASS_SOCKET, &asking.path);
            Some(asking)
        }
        _ => None,
    };
    let spawned = command.spawn();
    let status = spawned.map(|mut child| wait(&mut child, matches!(mode, Mode::Batch).then_some(BATCH_DEADLINE), Some(&cancelled)));
    drop(asking);
    let said = std::fs::read_to_string(&said_path).unwrap_or_default();
    let _ = std::fs::remove_file(&said_path);
    match status {
        Err(e) => Err((Problem::Other, format!("starting ssh: {e}"))),
        Ok(Some(status)) if status.success() => verify(host),
        Ok(_) if cancelled.load(Ordering::Acquire) => Err((Problem::SignIn, "sign-in cancelled".into())),
        Ok(None) => Err((Problem::Offline, "SSH did not connect in time".into())),
        Ok(Some(status)) => Err((classify(&said).unwrap_or(Problem::Other), if said.trim().is_empty() { format!("ssh: {status}") } else { last_line(&said) })),
    }
}

/// SSH can succeed without leaving a master (a control path it cannot make): every connection
/// would then authenticate by itself, which is the failure this module exists to prevent.
fn verify(host: &str) -> Result<(), (Problem, String)> {
    match check(host) {
        Ok(true) => Ok(()),
        Ok(false) => Err((Problem::Other, "SSH logged in but kept no shared connection (control socket)".into())),
        Err(e) => Err((Problem::Other, format!("{e:#}"))),
    }
}

/// The child's exit; None when the deadline passed or `cancelled` was raised, the child killed.
fn wait(child: &mut Child, deadline: Option<Duration>, cancelled: Option<&AtomicBool>) -> Option<std::process::ExitStatus> {
    let deadline = deadline.map(|d| Instant::now() + d);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if deadline.is_some_and(|at| Instant::now() >= at) || cancelled.is_some_and(|c| c.load(Ordering::Acquire)) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// What OpenSSH's message means for the box; None when it names none of these.
pub fn classify(said: &str) -> Option<Problem> {
    let has = |words: &[&str]| words.iter().any(|w| said.contains(w));
    if has(&["Host key verification failed", "HOST IDENTIFICATION HAS CHANGED", "host key is known", "Host key for"]) {
        Some(Problem::HostKey)
    } else if has(&["Permission denied", "Too many authentication failures", "No more authentication methods", "passphrase", "Authentication failed"]) {
        Some(Problem::SignIn)
    } else if has(&[
        "Connection timed out",
        "Connection refused",
        "No route to host",
        "Network is unreachable",
        "Could not resolve hostname",
        "Connection closed by",
        "Connection reset",
        "kex_exchange_identification",
        "Broken pipe",
        "Operation timed out",
        "Timeout, server",
        "timed out",
    ]) {
        Some(Problem::Offline)
    } else {
        None
    }
}

fn last_line(said: &str) -> String {
    let line = said.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("no message from SSH");
    line.chars().take(300).collect()
}

/// Keep the end of a connection's stderr while passing it on to ours, as it went before.
pub(crate) fn capture(stderr: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let kept = Arc::new(Mutex::new(String::new()));
    let keep = kept.clone();
    let _ = std::thread::Builder::new().name("sill-ssh-stderr".into()).spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
            let text = String::from_utf8_lossy(&line);
            eprint!("{text}");
            let mut kept = keep.lock().unwrap();
            kept.push_str(&text);
            if kept.len() > 4096 {
                let cut = kept.len() - 2048;
                let cut = (cut..kept.len()).find(|i| kept.is_char_boundary(*i)).unwrap_or(kept.len());
                kept.drain(..cut);
            }
            line.clear();
        }
    });
    kept
}

/// The window's end of SSH's questions: a socket the helper (this executable, started by ssh as
/// `SSH_ASKPASS`) connects to, one connection per question.
struct Asking {
    path: PathBuf,
    stop: Arc<AtomicBool>,
}

#[derive(Serialize, Deserialize)]
struct Answer {
    answer: Option<String>,
}

impl Asking {
    fn listen(host: &str, prompter: Prompter, cancelled: Arc<AtomicBool>) -> Result<Asking> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = dir()?.join(format!("ask-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        Self::listen_at(path, host, prompter, cancelled)
    }

    fn listen_at(path: PathBuf, host: &str, prompter: Prompter, cancelled: Arc<AtomicBool>) -> Result<Asking> {
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).context("listening for SSH's questions")?;
        let stop = Arc::new(AtomicBool::new(false));
        let (stopped, host) = (stop.clone(), host.to_string());
        std::thread::Builder::new().name("sill-askpass".into()).spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (prompter, cancelled, host) = (prompter.clone(), cancelled.clone(), host.clone());
                let _ = std::thread::Builder::new().name("sill-askpass-question".into()).spawn(move || {
                    let mut line = String::new();
                    let Ok(mut writer) = stream.try_clone() else { return };
                    if BufReader::new(stream).read_line(&mut line).is_err() {
                        return;
                    }
                    let Ok(mut prompt) = serde_json::from_str::<Prompt>(&line) else { return };
                    prompt.host = host;
                    // one Cancel ends the sign-in: SSH would otherwise ask for the next method
                    let answer = if cancelled.load(Ordering::Acquire) { None } else { prompter(prompt.clone()) };
                    if answer.is_none() && prompt.kind != PromptKind::Notice {
                        cancelled.store(true, Ordering::Release);
                    }
                    if let Ok(mut reply) = serde_json::to_vec(&Answer { answer }) {
                        reply.push(b'\n');
                        let _ = writer.write_all(&reply);
                    }
                });
            }
        })?;
        Ok(Asking { path, stop })
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // wake the accept loop so it sees the flag
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// True when this process was started by ssh as the askpass helper of a window's sign-in.
pub fn is_askpass() -> bool {
    std::env::var_os(ASKPASS_SOCKET).is_some()
}

/// The helper: hand SSH's question to the window, print the answer for SSH. Exit 0 with an
/// answer (or a yes), 1 without.
pub fn askpass_main() -> ! {
    let answer = (|| -> Result<Option<String>> {
        let text = std::env::args().nth(1).unwrap_or_default();
        let kind = match std::env::var("SSH_ASKPASS_PROMPT").as_deref() {
            Ok("confirm") => PromptKind::Confirm,
            Ok("none") => PromptKind::Notice,
            _ if text.contains("yes/no") => PromptKind::Text,
            _ => PromptKind::Secret,
        };
        let path = std::env::var_os(ASKPASS_SOCKET).context("no window to ask")?;
        let mut stream = std::os::unix::net::UnixStream::connect(path)?;
        let mut line = serde_json::to_vec(&Prompt { host: String::new(), text, kind })?;
        line.push(b'\n');
        stream.write_all(&line)?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        Ok(serde_json::from_str::<Answer>(&reply)?.answer)
    })();
    match answer {
        Ok(Some(answer)) => {
            println!("{answer}");
            std::process::exit(0)
        }
        _ => std::process::exit(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_messages_decide_the_box_state() {
        assert_eq!(classify("pat@10.0.0.140: Permission denied (publickey,password)."), Some(Problem::SignIn));
        assert_eq!(classify("ssh: connect to host 10.0.0.140 port 22: Connection timed out"), Some(Problem::Offline));
        assert_eq!(classify("ssh: connect to host 10.0.0.140 port 22: No route to host"), Some(Problem::Offline));
        assert_eq!(classify("ssh: Could not resolve hostname box: Name or service not known"), Some(Problem::Offline));
        assert_eq!(classify("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @\nHost key verification failed."), Some(Problem::HostKey));
        assert_eq!(
            classify("No ED25519 host key is known for 10.0.0.140 and you have requested strict checking.\nHost key verification failed."),
            Some(Problem::HostKey)
        );
        assert_eq!(classify("sh: 1: sill: not found"), None);
        assert_eq!(last_line("Warning: something\nbox: Permission denied (publickey).\n\n"), "box: Permission denied (publickey).");
    }

    #[test]
    fn sign_in_waits_for_the_person_and_offline_backs_off() {
        let mut link = Link::default();
        let error = link.failed("sign-in.test", Problem::SignIn, "Permission denied".into());
        assert!(link.retry_at.is_none(), "nothing retries a sign-in in the background");
        assert_eq!(error.downcast_ref::<Unavailable>().unwrap().problem, Problem::SignIn);
        let mut link = Link::default();
        let mut last = Duration::ZERO;
        for _ in 0..8 {
            let error = link.failed("offline.test", Problem::Offline, "Connection timed out".into());
            let wait = error.downcast_ref::<Unavailable>().unwrap().retry_in.unwrap();
            assert!(wait <= Duration::from_secs(72), "{wait:?}");
            last = wait;
        }
        assert!(last >= Duration::from_secs(47), "the backoff reaches about a minute: {last:?}");
        let shown = status().unwrap();
        assert!(matches!(shown.problem, Problem::Offline | Problem::SignIn));
        link.connected("offline.test");
        link.connected("sign-in.test");
        assert!(link.problem.is_none() && link.attempt == 0);
        assert_eq!(status(), None);
    }

    #[test]
    fn the_window_answers_ssh_and_one_cancel_ends_the_sign_in() {
        let dir = std::env::temp_dir().join(format!("sill-ask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let prompter: Prompter = Arc::new(move |prompt: Prompt| {
            seen.lock().unwrap().push(prompt.clone());
            prompt.text.contains("passphrase").then(|| "secret words".to_string())
        });
        let asking = Asking::listen_at(dir.join("ask"), "box.test", prompter, cancelled.clone()).unwrap();
        // the helper's side, by hand
        let ask = |text: &str| {
            let mut stream = std::os::unix::net::UnixStream::connect(&asking.path).unwrap();
            let mut line = serde_json::to_vec(&Prompt { host: String::new(), text: text.into(), kind: PromptKind::Secret }).unwrap();
            line.push(b'\n');
            stream.write_all(&line).unwrap();
            let mut reply = String::new();
            BufReader::new(stream).read_line(&mut reply).unwrap();
            serde_json::from_str::<Answer>(&reply).unwrap().answer
        };
        assert_eq!(ask("Enter passphrase for key 'k':").as_deref(), Some("secret words"));
        assert!(!cancelled.load(Ordering::Acquire));
        assert_eq!(ask("user@box's password:"), None);
        assert!(cancelled.load(Ordering::Acquire), "a cancelled question stops the ssh attempt");
        assert_eq!(ask("user@box's password:"), None);
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "nothing is asked after a cancel");
        assert_eq!(asked[0].host, "box.test");
        let path = asking.path.clone();
        drop(asking);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
