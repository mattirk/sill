//! Logins end to end, in each way agents have asked for one: a real daemon and relay, a scripted
//! agent in place of a jail, and the frontend's own view of the session (`sfp_client::View`, whose
//! events are what sill-gui draws). Host-side logins run the real `sill login` against a scripted
//! adapter. No network, no credentials, no browser.
//!
//! When an agent's login behaves in a way none of these scenarios does, add the scenario here
//! before changing the login code (docs/login.md §2).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sill_core::wire::Event;

const WAIT: Duration = Duration::from_secs(20);

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // on disk, not /tmp: every daemon stages a copy of the debug binary in its state dir
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{tag}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// --- the scripted agent ----------------------------------------------------------------------------

/// When the agent asks for a login.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ask {
    /// `session/new` fails until it is logged in (Codex, Gemini).
    AtOpen,
    /// The session opens; every prompt fails until it is logged in (Claude Code).
    AtPrompt,
    /// `session/new` gets no answer until it is logged in: the agent waits for a browser login it
    /// started itself (Antigravity).
    HangsAtOpen,
}

/// Where an agent reads an API key in `authenticate`'s `_meta`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    /// `{"api-key": "<key>"}` (Gemini CLI)
    Text,
    /// `{"api-key": {"apiKey": "<key>"}}` (codex-acp)
    Object,
}

/// An agent's login behavior. It is logged in while `credential` exists — `authenticate` creates
/// it, a terminal login is the test creating it before the frontend retries — or, with `key`,
/// once its `api-key` method got the right key: then in memory only, as Gemini CLI holds it.
#[derive(Clone)]
struct Script {
    asks: Ask,
    methods: Value,
    /// `authenticate` calls that fail before one succeeds.
    failing: Arc<Mutex<usize>>,
    key: Option<(Shape, &'static str)>,
    /// A key in the other shape is accepted without being read, as Gemini CLI does: the login
    /// "succeeds" and the session still cannot open. Sending that shape first would end there.
    lenient: bool,
    /// Accept any key at `authenticate` and `session/new`, and check it only on `session/prompt`
    /// (as Gemini CLI and codex-acp do when given a syntactically valid but revoked/wrong key).
    check_key_at_prompt: bool,
}

fn agent_method() -> Value { json!({"id": "browser", "name": "Browser"}) }
fn terminal_method() -> Value { json!({"type": "terminal", "id": "cli-login", "name": "Terminal login", "args": ["--login"]}) }
fn key_method() -> Value { json!({"id": "api-key", "name": "API key", "_meta": {"api-key": {"provider": "google"}}}) }

fn read(reader: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut line = String::new();
    (reader.read_line(&mut line).ok()? > 0).then(|| serde_json::from_str(&line).unwrap())
}

fn send(writer: &mut UnixStream, message: Value) -> bool { writeln!(writer, "{message}").is_ok() }

/// One connection from the relay: a control connection (presence, status) or the agent's ACP.
fn serve(stream: UnixStream, script: Script, credential: PathBuf, observed: mpsc::Sender<String>) {
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let Some(role) = read(&mut reader) else { return };
    if role["role"] == "control" {
        while let Some(request) = read(&mut reader) {
            let response = if request["op"] == "status" {
                json!({"ok": true, "running": true, "pid": std::process::id(), "exit": null, "buffered_lines": 0})
            } else {
                json!({"ok": true})
            };
            if !send(&mut writer, response) { return; }
        }
        return;
    }
    let auth_required = json!({"code": -32000, "message": "Authentication required"});
    let mut key_accepted = false;
    let mut key_sent = false;
    while let Some(message) = read(&mut reader) {
        let (Some(id), Some(method)) = (message.get("id").cloned(), message["method"].as_str()) else { continue };
        let logged_in = credential.exists() || key_accepted;
        if method == "session/new" && script.asks == Ask::HangsAtOpen && !logged_in {
            let _ = observed.send("session/new unanswered".into());
            continue;
        }
        let reply = match method {
            "initialize" => Ok(json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": false, "promptCapabilities": {"image": false}}, "authMethods": script.methods})),
            "session/new" if script.asks == Ask::AtOpen && !logged_in && !(script.check_key_at_prompt && key_sent) => Err(auth_required.clone()),
            "session/new" => Ok(json!({"sessionId": "login-session"})),
            "authenticate" if message["params"]["methodId"] == "api-key" && script.key.is_some() => {
                let (shape, expected) = script.key.unwrap();
                let meta = &message["params"]["_meta"]["api-key"];
                let given = match shape { Shape::Text => meta.as_str(), Shape::Object => meta["apiKey"].as_str() };
                let other_shape = match shape { Shape::Text => meta.is_object(), Shape::Object => meta.is_string() };
                let _ = observed.send(format!("key {}", given.unwrap_or("-")));
                if given == Some(expected) {
                    key_accepted = true;
                    key_sent = true;
                    Ok(json!({}))
                } else if script.check_key_at_prompt && given.is_some() {
                    key_accepted = false;
                    key_sent = true;
                    Ok(json!({}))
                } else if script.lenient && other_shape {
                    Ok(json!({}))
                } else {
                    Err(json!({"code": -32000, "message": "API key missing or not valid"}))
                }
            }
            "authenticate" => {
                let mut failing = script.failing.lock().unwrap();
                if *failing > 0 {
                    *failing -= 1;
                    Err(json!({"code": -32603, "message": "the code was not accepted"}))
                } else {
                    std::fs::write(&credential, "token").unwrap();
                    Ok(json!({}))
                }
            }
            "session/prompt" => {
                let text = message["params"]["prompt"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<String>();
                let marker = text.split_whitespace().find(|w| w.starts_with("PROMPT-")).unwrap_or("?").to_string();
                let _ = observed.send(format!("prompt {marker}"));
                if marker == "PROMPT-SLOW" && !logged_in {
                    std::thread::sleep(Duration::from_millis(200));
                }
                if logged_in {
                    send(&mut writer, json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "login-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("answer to {marker}")}}}}));
                    Ok(json!({"stopReason": "end_turn"}))
                } else if script.check_key_at_prompt {
                    Err(json!({"code": -32603, "message": "API key not valid. Please pass a valid API key."}))
                } else {
                    Err(auth_required.clone())
                }
            }
            _ => Ok(json!({})),
        };
        if method != "session/prompt" { let _ = observed.send(method.to_string()); }
        let response = match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        if !send(&mut writer, response) { break; }
    }
    // the agent's connection is gone: its process holds nothing any more
    let _ = observed.send("closed".into());
}

// --- a real daemon and relay around it -------------------------------------------------------------

struct Fixture {
    dir: PathBuf,
    session: String,
    credential: PathBuf,
    daemon: Child,
    relay: Child,
    launcher: Child,
    shutdown: Arc<AtomicBool>,
    shim: Option<std::thread::JoinHandle<()>>,
    observed: mpsc::Receiver<String>,
}

impl Fixture {
    fn environment(dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sill"));
        command.env("XDG_RUNTIME_DIR", dir.join("run")).env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CONFIG_HOME", dir.join("config")).env("HOME", dir.join("home"))
            .env("SILL_LAUNCHER", "direct")
            .env_remove("SILL_SESSION").env_remove("SILL_BOX").env_remove("INVOCATION_ID")
            .env_remove("LISTEN_PID").env_remove("LISTEN_FDS").env_remove("NOTIFY_SOCKET")
            .stdin(Stdio::null()).stdout(Stdio::null());
        command
    }

    fn new(script: Script) -> Self { Self::start(script, &[], None) }

    /// `env`: for the relay, where the agent loop runs (`SILL_SESSION_OPEN_TIMEOUT_MS`).
    fn with_relay_env(script: Script, env: &[(&str, &str)]) -> Self { Self::start(script, env, None) }

    /// The person typed `key` for `method` in an earlier session: sill kept it.
    fn with_saved_key(script: Script, method: &str, key: &str) -> Self { Self::start(script, &[], Some((method, key))) }

    fn start(script: Script, env: &[(&str, &str)], saved_key: Option<(&str, &str)>) -> Self {
        let dir = scratch("login");
        for sub in ["run/sill", "state/sill", "config/sill", "home"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let credential = dir.join("credential");
        let store = sill_core::store::Store::open(&dir.join("state/sill/sill.db")).unwrap();
        let session = store.create("claude", "default").unwrap().id;
        drop(store); // the daemon is the store's only writer from here on
        let runtime = dir.join("run/sill").join(&session);
        std::fs::create_dir_all(&runtime).unwrap();
        // a live launch process, so the daemon adopts the session as a running jail
        let launcher = Command::new("sleep").arg("300").spawn().unwrap();
        std::fs::write(runtime.join("launch.pid"), launcher.id().to_string()).unwrap();
        let listener = UnixListener::bind(runtime.join("shim.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (observe, observed) = mpsc::channel();
        let shim = {
            let (stop, credential) = (shutdown.clone(), credential.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (script, credential, observe) = (script.clone(), credential.clone(), observe.clone());
                            std::thread::spawn(move || serve(stream, script, credential, observe));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                        Err(_) => break,
                    }
                }
            })
        };
        let relay = Self::environment(&dir).envs(env.iter().copied()).args(["daemon", "relay", "--session", &session, "--socket"])
            .arg(runtime.join("relay.sock")).arg("--agent-socket").arg(runtime.join("shim.sock"))
            .arg("--cwd").arg(dir.join("home")).stderr(std::fs::File::create(dir.join("relay.log")).unwrap()).spawn().unwrap();
        let mut waited = Instant::now();
        while !runtime.join("relay.sock").exists() {
            assert!(waited.elapsed() < WAIT, "the relay did not come up");
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some((method, key)) = saved_key {
            let keys = dir.join("state/sill/keys/claude");
            std::fs::create_dir_all(&keys).unwrap();
            std::fs::write(keys.join(method), key).unwrap();
        }
        let daemon = Self::environment(&dir).arg("daemon").stderr(std::fs::File::create(dir.join("daemon.log")).unwrap()).spawn().unwrap();
        let fixture = Self { dir, session, credential, daemon, relay, launcher, shutdown, shim: Some(shim), observed };
        // adopted: the session has its public journal
        waited = Instant::now();
        while !fixture.database().is_some_and(|db| db.query_row("SELECT count(*) FROM sfp_sessions WHERE session_id=?1", [&fixture.session], |r| r.get::<_, i64>(0)).unwrap_or(0) == 1) {
            assert!(waited.elapsed() < WAIT, "the daemon did not adopt the session; {}", fixture.logs());
            std::thread::sleep(Duration::from_millis(20));
        }
        fixture
    }

    fn database(&self) -> Option<rusqlite::Connection> {
        rusqlite::Connection::open_with_flags(self.dir.join("state/sill/sill.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
    }

    fn logs(&self) -> String {
        let read = |f: &str| std::fs::read_to_string(self.dir.join(f)).unwrap_or_default();
        format!("daemon log:\n{}\nrelay log:\n{}", read("daemon.log"), read("relay.log"))
    }

    /// The agent was asked for `what` (a method name, or `prompt PROMPT-…`).
    fn expect(&self, what: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.observed.recv_timeout(left) {
                Ok(seen) if seen == what => return,
                Ok(_) => {}
                Err(_) => panic!("the agent never got {what}; {}", self.logs()),
            }
        }
    }

    /// The agent is not asked for `what` within `quiet`.
    fn never(&self, what: &str, quiet: Duration) {
        let deadline = Instant::now() + quiet;
        while let Ok(seen) = self.observed.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            assert_ne!(seen, what, "the agent got {what} too early");
        }
    }

    /// What the person did in the agent's login terminal: its login file is there now.
    fn terminal_login(&self) { std::fs::write(&self.credential, "token").unwrap(); }

    fn saved_key(&self, method: &str) -> PathBuf { self.dir.join("state/sill/keys/claude").join(method) }

    /// Files under sill's state, the logs included, that contain `secret` — not counting the saved
    /// key itself or the daemon's staged copy of its own binary.
    fn holding(&self, secret: &str) -> Vec<PathBuf> {
        fn walk(dir: &Path, skip: &[PathBuf], secret: &[u8], out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if skip.iter().any(|s| p.starts_with(s)) { continue; }
                if p.is_dir() && !p.is_symlink() { walk(&p, skip, secret, out); }
                else if std::fs::read(&p).is_ok_and(|b| b.windows(secret.len()).any(|w| w == secret)) { out.push(p); }
            }
        }
        let mut out = vec![];
        walk(&self.dir, &[self.dir.join("state/sill/keys"), self.dir.join("state/sill/bin")], secret.as_bytes(), &mut out);
        out
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill(); let _ = self.daemon.wait();
        let _ = self.relay.kill(); let _ = self.relay.wait();
        let _ = self.launcher.kill(); let _ = self.launcher.wait();
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(shim) = self.shim.take() { let _ = shim.join(); }
        if !std::thread::panicking() { let _ = std::fs::remove_dir_all(&self.dir); }
    }
}

// --- the frontend, as sill-gui sees the session --------------------------------------------------

struct Frontend {
    session: String,
    client: sill_proto::Client,
    view: sill_core::sfp_client::View,
    shown: Vec<Event>,
    /// Events before this index have been matched by an earlier wait.
    cursor: usize,
}

impl Frontend {
    fn attach(fixture: &Fixture) -> Self {
        let client = sill_proto::Client::over(UnixStream::connect(fixture.dir.join("run/sill/daemon.sock")).unwrap()).unwrap();
        client.initialize().unwrap();
        let mut view = sill_core::sfp_client::View::default();
        let shown = view.attach(&client, &fixture.session).unwrap();
        Self { session: fixture.session.clone(), client, view, shown, cursor: 0 }
    }

    fn pump(&mut self, wait: Duration) {
        let notifications = self.client.notifications.lock().unwrap();
        let mut wait = wait;
        while let Ok((method, params)) = notifications.recv_timeout(wait) {
            wait = Duration::from_millis(20);
            if method != "_sill/session.event" { continue; }
            let event: sill_proto::sfp::SessionEvent = serde_json::from_value(params).unwrap();
            let shown = self.view.accept(event).expect("the frontend's view stays in sync");
            self.shown.extend(shown);
        }
    }

    /// The frontend shows an event `what` describes; later waits look only after it.
    fn wait_for(&mut self, fixture: &Fixture, what: &str, mut matches: impl FnMut(&Event) -> bool) -> Event {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(i) = self.shown[self.cursor..].iter().position(&mut matches) {
                self.cursor += i + 1;
                return self.shown[self.cursor - 1].clone();
            }
            assert!(Instant::now() < deadline, "the frontend never showed {what}; it showed {:#?}\n{}", &self.shown[self.cursor..], fixture.logs());
            self.pump(Duration::from_millis(100));
        }
    }

    /// The frontend showed an event like that at any point.
    fn showed(&self, matches: impl Fn(&Event) -> bool) -> bool { self.shown.iter().any(matches) }

    fn login_card(&mut self, fixture: &Fixture) -> (String, Vec<String>) {
        match self.wait_for(fixture, "the login card", |e| matches!(e, Event::AuthRequired { .. })) {
            Event::AuthRequired { message, methods } => (message, methods.into_iter().map(|m| m.id).collect()),
            _ => unreachable!(),
        }
    }

    fn login_card_closes(&mut self, fixture: &Fixture) {
        self.wait_for(fixture, "the login card closing", |e| matches!(e, Event::Other(t) if t == "sill:authenticated"));
    }

    fn answer(&mut self, fixture: &Fixture, marker: &str) {
        let expected = format!("answer to {marker}");
        self.wait_for(fixture, &expected, |e| matches!(e, Event::Text(t) if *t == expected));
    }

    fn submit(&self, marker: &str) -> Value {
        self.client.call("_sill/sessions.submit", json!({"session_id": self.session, "submission_id": marker, "text": format!("{marker} please"), "attachments": []})).unwrap()
    }

    fn submission(&self, marker: &str) -> Value {
        self.client.call("_sill/submissions.get", json!({"session_id": self.session, "submission_id": marker})).unwrap()
    }

    fn control(&self, control: Value) {
        let _: Value = self.client.call("_sill/sessions.control", json!({"session_id": self.session, "control": control})).unwrap();
    }

    fn resume(&self, marker: &str) {
        let _: Value = self.client.call("_sill/submissions.resume", json!({"session_id": self.session, "submission_id": marker})).unwrap();
    }
}

fn script(asks: Ask, methods: Value) -> Script { Script { asks, methods, failing: Arc::new(Mutex::new(0)), key: None, lenient: false, check_key_at_prompt: false } }

/// An agent whose `api-key` method takes `key` in `shape`; a Gemini-like one (`Text`) accepts the
/// other shape without reading it, as Gemini CLI does.
fn key_script(shape: Shape, key: &'static str) -> Script {
    Script { key: Some((shape, key)), lenient: shape == Shape::Text, ..script(Ask::AtOpen, json!([key_method()])) }
}

// --- scenarios: when the agent asks × how the person logs in -----------------------------------

/// Codex/Gemini with an agent-side method: the session cannot open until `authenticate` succeeds.
#[test]
fn login_at_open_with_an_agent_method() {
    let fixture = Fixture::new(script(Ask::AtOpen, json!([agent_method()])));
    let mut frontend = Frontend::attach(&fixture);
    let (_, methods) = frontend.login_card(&fixture);
    assert_eq!(methods, ["browser"]);
    frontend.control(json!({"type": "authenticate", "method_id": "browser"}));
    fixture.expect("authenticate");
    fixture.expect("session/new");
    frontend.login_card_closes(&fixture);
    frontend.submit("PROMPT-ONE");
    fixture.expect("prompt PROMPT-ONE");
    frontend.answer(&fixture, "PROMPT-ONE");
}

/// A login the agent rejects shows the card again with its reason, and a second try works.
#[test]
fn a_failed_agent_login_asks_again_with_its_reason() {
    let mut agent = script(Ask::AtOpen, json!([agent_method()]));
    agent.failing = Arc::new(Mutex::new(1));
    let fixture = Fixture::new(agent);
    let mut frontend = Frontend::attach(&fixture);
    frontend.login_card(&fixture);
    frontend.control(json!({"type": "authenticate", "method_id": "browser"}));
    let (reason, methods) = frontend.login_card(&fixture);
    assert!(reason.contains("the code was not accepted"), "{reason}");
    assert_eq!(methods, ["browser"]);
    frontend.control(json!({"type": "authenticate", "method_id": "browser"}));
    frontend.login_card_closes(&fixture);
    frontend.submit("PROMPT-ONE");
    frontend.answer(&fixture, "PROMPT-ONE");
}

/// The agent's own login command, run in a terminal before the session could open: closing the
/// terminal retries the session.
#[test]
fn login_at_open_with_the_agents_terminal_command() {
    let fixture = Fixture::new(script(Ask::AtOpen, json!([terminal_method()])));
    let mut frontend = Frontend::attach(&fixture);
    let (_, methods) = frontend.login_card(&fixture);
    assert_eq!(methods, ["cli-login"]);
    fixture.terminal_login();
    frontend.control(json!({"type": "retry_session"}));
    fixture.expect("session/new");
    frontend.login_card_closes(&fixture);
    frontend.submit("PROMPT-ONE");
    frontend.answer(&fixture, "PROMPT-ONE");
}

/// Claude Code: the session opens without a login and the first prompt asks. Prompts sent while
/// it waits stay queued; the terminal login releases them (they stayed queued for good once).
#[test]
fn login_at_first_prompt_with_the_terminal_releases_queued_prompts() {
    let fixture = Fixture::new(script(Ask::AtPrompt, json!([terminal_method()])));
    let mut frontend = Frontend::attach(&fixture);
    frontend.wait_for(&fixture, "the session opening", |e| matches!(e, Event::Ready { .. }));
    frontend.submit("PROMPT-ONE");
    fixture.expect("prompt PROMPT-ONE");
    frontend.login_card(&fixture);
    frontend.submit("PROMPT-TWO");
    fixture.never("prompt PROMPT-TWO", Duration::from_millis(800));
    assert_eq!(frontend.submission("PROMPT-TWO")["delivery"], "queued", "a prompt waits for the login");
    fixture.terminal_login();
    frontend.control(json!({"type": "retry_session"}));
    frontend.login_card_closes(&fixture);
    fixture.expect("prompt PROMPT-TWO");
    frontend.answer(&fixture, "PROMPT-TWO");
}

/// A second prompt queued while the first turn is still in flight is held when that turn fails with
/// `auth_required`: the frontend gets the held-prompt card (`sill:held:…`), and resuming it after
/// login sends it and resolves the card (`sill:held-resolved:…`).
#[test]
fn a_prompt_queued_during_a_failing_turn_is_held_and_resumable_from_the_card() {
    let fixture = Fixture::new(script(Ask::AtPrompt, json!([agent_method()])));
    let mut frontend = Frontend::attach(&fixture);
    frontend.wait_for(&fixture, "the session opening", |e| matches!(e, Event::Ready { .. }));
    frontend.submit("PROMPT-SLOW");
    fixture.expect("prompt PROMPT-SLOW");
    frontend.submit("PROMPT-HELD");
    frontend.wait_for(&fixture, "the held prompt card", |e| matches!(e, Event::Other(t) if t.starts_with("sill:held:") && t.contains("PROMPT-HELD")));
    frontend.login_card(&fixture);
    assert_eq!(frontend.view.held_submission_ids(), ["PROMPT-HELD"]);
    frontend.control(json!({"type": "authenticate", "method_id": "browser"}));
    frontend.login_card_closes(&fixture);
    fixture.never("prompt PROMPT-HELD", Duration::from_millis(200));
    frontend.resume("PROMPT-HELD");
    frontend.wait_for(&fixture, "the held prompt card closing", |e| matches!(e, Event::Other(t) if t.starts_with("sill:held-resolved:") && t.contains("PROMPT-HELD")));
    fixture.expect("prompt PROMPT-HELD");
    frontend.answer(&fixture, "PROMPT-HELD");
}

/// A terminal closed without logging in: the queued prompt goes, the agent asks again, and the
/// card is back — nothing hangs in "working" or "queued".
#[test]
fn a_terminal_login_that_did_not_take_asks_again() {
    let fixture = Fixture::new(script(Ask::AtPrompt, json!([terminal_method()])));
    let mut frontend = Frontend::attach(&fixture);
    frontend.wait_for(&fixture, "the session opening", |e| matches!(e, Event::Ready { .. }));
    frontend.submit("PROMPT-ONE");
    frontend.login_card(&fixture);
    frontend.submit("PROMPT-TWO");
    frontend.control(json!({"type": "retry_session"}));
    fixture.expect("prompt PROMPT-TWO");
    frontend.login_card(&fixture);
    let two = frontend.submission("PROMPT-TWO");
    assert_ne!(two["outcome"], "completed", "{two}");
    fixture.terminal_login();
    frontend.submit("PROMPT-THREE");
    frontend.control(json!({"type": "retry_session"}));
    frontend.answer(&fixture, "PROMPT-THREE");
}

/// Antigravity: with no login, `session/new` hangs while the agent waits for a browser login it
/// started itself. A prompt sent while the session is still opening is not held when the timeout
/// asks for a login — once the login finishes, the session opens and that prompt goes out.
#[test]
fn an_agent_that_hangs_at_open_gets_the_login_card_after_the_timeout() {
    let fixture = Fixture::with_relay_env(script(Ask::HangsAtOpen, json!([agent_method()])), &[("SILL_SESSION_OPEN_TIMEOUT_MS", "300")]);
    fixture.expect("session/new unanswered");
    let mut frontend = Frontend::attach(&fixture);
    frontend.submit("PROMPT-ONE");
    let (reason, methods) = frontend.login_card(&fixture);
    assert!(reason.contains("did not open a session within 300 ms"), "{reason}");
    assert_eq!(methods, ["browser"]);
    assert_eq!(frontend.submission("PROMPT-ONE")["held"], false, "a prompt sent before the session opened is not held");
    frontend.control(json!({"type": "authenticate", "method_id": "browser"}));
    fixture.expect("authenticate");
    fixture.expect("session/new");
    frontend.login_card_closes(&fixture);
    fixture.expect("prompt PROMPT-ONE");
    frontend.answer(&fixture, "PROMPT-ONE");
}

// --- API keys ------------------------------------------------------------------------------------

/// Gemini CLI and codex-acp mark their API key method (`_meta["api-key"]`) and read the key from
/// `authenticate`'s `_meta`, each in its own shape. The card asks for the key; sill keeps it out of
/// the agent's jail, out of its journal and logs, and sends it in both shapes.
#[test]
fn an_api_key_typed_into_the_card_reaches_the_agent_in_its_shape() {
    for shape in [Shape::Text, Shape::Object] {
        let fixture = Fixture::new(key_script(shape, "sk-typed-1234"));
        let mut frontend = Frontend::attach(&fixture);
        let card = frontend.wait_for(&fixture, "the login card", |e| matches!(e, Event::AuthRequired { .. }));
        let Event::AuthRequired { methods, .. } = card else { unreachable!() };
        assert_eq!(methods[0].api_key.as_deref(), Some("google"), "the card knows to ask for a key");
        frontend.control(json!({"type": "authenticate", "method_id": "api-key", "api_key": "sk-typed-1234"}));
        if shape == Shape::Object { fixture.expect("key -"); }
        fixture.expect("key sk-typed-1234");
        frontend.login_card_closes(&fixture);
        frontend.submit("PROMPT-ONE");
        frontend.answer(&fixture, "PROMPT-ONE");
        let saved = fixture.saved_key("api-key");
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), "sk-typed-1234");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600, "{shape:?}");
        assert!(fixture.holding("sk-typed-1234").is_empty(), "the key leaked into {:?}", fixture.holding("sk-typed-1234"));
    }
}

/// Gemini CLI holds a key in memory only: every new agent asks again at session open. A key the
/// person gave before answers it, and the card never shows.
#[test]
fn a_saved_api_key_answers_a_new_agent_without_the_card() {
    let fixture = Fixture::with_saved_key(key_script(Shape::Text, "sk-saved-5678"), "api-key", "sk-saved-5678");
    let mut frontend = Frontend::attach(&fixture);
    fixture.expect("key sk-saved-5678");
    frontend.wait_for(&fixture, "the session opening", |e| matches!(e, Event::Ready { .. }));
    frontend.submit("PROMPT-ONE");
    frontend.answer(&fixture, "PROMPT-ONE");
    assert!(!frontend.showed(|e| matches!(e, Event::AuthRequired { .. })), "the card showed though a key was saved");
}

/// A saved key the agent refuses (revoked, replaced) is tried once; the card shows with the agent's
/// reason, and the key typed there replaces it.
#[test]
fn a_refused_saved_api_key_shows_the_card_with_the_reason() {
    let fixture = Fixture::with_saved_key(Script { lenient: false, ..key_script(Shape::Text, "sk-new-9999") }, "api-key", "sk-old-0000");
    let mut frontend = Frontend::attach(&fixture);
    let (reason, _) = frontend.login_card(&fixture);
    assert!(reason.contains("API key missing or not valid"), "{reason}");
    frontend.control(json!({"type": "authenticate", "method_id": "api-key", "api_key": "sk-new-9999"}));
    frontend.login_card_closes(&fixture);
    frontend.submit("PROMPT-ONE");
    frontend.answer(&fixture, "PROMPT-ONE");
    assert_eq!(std::fs::read_to_string(fixture.saved_key("api-key")).unwrap(), "sk-new-9999");
}

/// Gemini CLI and codex-acp accept any key at `authenticate` and open the session; a revoked or
/// wrong key fails only on the first `session/prompt` (often with a generic JSON-RPC error code
/// naming the invalid API key). That failure opens the login card with the provider's reason
/// instead of resending the bad saved key, and a new key recovers the session.
#[test]
fn a_saved_api_key_rejected_at_first_prompt_shows_the_card_instead_of_resending() {
    let fixture = Fixture::with_saved_key(Script { check_key_at_prompt: true, ..key_script(Shape::Text, "sk-valid-7777") }, "api-key", "sk-bad-0000");
    let mut frontend = Frontend::attach(&fixture);
    fixture.expect("key sk-bad-0000");
    frontend.wait_for(&fixture, "the session opening", |e| matches!(e, Event::Ready { .. }));
    frontend.submit("PROMPT-ONE");
    fixture.expect("prompt PROMPT-ONE");
    let (reason, _) = frontend.login_card(&fixture);
    assert!(reason.contains("API key not valid"), "{reason}");
    fixture.never("key sk-bad-0000", Duration::from_millis(300));
    frontend.control(json!({"type": "authenticate", "method_id": "api-key", "api_key": "sk-valid-7777"}));
    fixture.expect("key sk-valid-7777");
    frontend.login_card_closes(&fixture);
    frontend.submit("PROMPT-TWO");
    frontend.answer(&fixture, "PROMPT-TWO");
    assert_eq!(std::fs::read_to_string(fixture.saved_key("api-key")).unwrap(), "sk-valid-7777");
}

// --- reset ----------------------------------------------------------------------------------------

/// Reset: the agent's running sessions stop first (a running agent holds its login and writes to its
/// home), then its sill-owned home and saved API keys go — it starts as a fresh install. Again:
/// nothing left; an unknown agent is refused.
#[test]
fn resetting_an_agent_stops_its_sessions_then_removes_its_home_and_keys() {
    let fixture = Fixture::new(script(Ask::AtPrompt, json!([terminal_method()])));
    let mut frontend = Frontend::attach(&fixture);
    frontend.wait_for(&fixture, "the session opening", |e| matches!(e, Event::Ready { .. }));
    let own = fixture.dir.join("state/sill/agents/claude");
    std::fs::create_dir_all(own.join(".claude")).unwrap();
    std::fs::write(own.join(".claude/.credentials.json"), "sill's login").unwrap();
    std::fs::write(own.join(".claude/settings.json"), "{}").unwrap();
    std::fs::create_dir_all(fixture.saved_key("api-key").parent().unwrap()).unwrap();
    std::fs::write(fixture.saved_key("api-key"), "sk-saved").unwrap();
    let out: Value = frontend.client.call("_sill/agents.reset", json!({"agent_id": "claude"})).unwrap();
    assert_eq!(out["stopped"], json!([fixture.session]), "{out}");
    assert_eq!(out["removed"], 3, "{out}");
    assert!(!own.exists(), "the agent's home is gone");
    assert!(!fixture.saved_key("api-key").exists(), "the saved key is gone");
    fixture.expect("closed");
    let again: Value = frontend.client.call("_sill/agents.reset", json!({"agent_id": "claude"})).unwrap();
    assert_eq!((again["removed"].clone(), again["stopped"].clone()), (json!(0), json!([])), "{again}");
    let unknown = frontend.client.call::<Value>("_sill/agents.reset", json!({"agent_id": "nobody"}));
    assert!(unknown.is_err_and(|e| format!("{e:#}").contains("agent not found")));
}

// --- host-side logins: the real `sill login` against a scripted adapter --------------------------

/// What `sill login fakelogin` sees: a home with the fake agent's definition, sill's state with its
/// install (the adapter and a helper program under tools/), and the agent's existing sill home.
struct HostLogin { dir: PathBuf }

impl HostLogin {
    fn new(fail: bool) -> Self {
        let dir = scratch("host-login");
        let tools = dir.join("state/sill/tools/fakelogin");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(dir.join("home/.config/sill/agents")).unwrap();
        // a helper program from the agent's install, as Codex's `codex app-server` is: a copy of the
        // shell, so its executable is under tools/
        std::fs::copy(std::fs::canonicalize("/bin/sh").unwrap(), tools.join("helper")).unwrap();
        let adapter = tools.join("adapter.py");
        std::fs::write(&adapter, ADAPTER).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        let definition = format!(
            "id = \"fakelogin\"\nname = \"Fake login\"\ncommand = [\"{adapter}\"]\nhome_paths = [{{path = \"~/.fakelogin\", kind = \"directory\"}}]\nlogin_files = [\".fakelogin/auth.json\"]\nhome_env = \"FAKE_HOME\"\nhost_login = [\"browser\"]\n\n[env]\nFAKE_HELPER = \"{helper}\"\nFAKE_HELPER_PID = \"{pid}\"\n{fail}",
            adapter = adapter.display(), helper = tools.join("helper").display(), pid = dir.join("helper.pid").display(),
            fail = if fail { "FAKE_FAIL = \"1\"\n" } else { "" },
        );
        std::fs::write(dir.join("home/.config/sill/agents/fakelogin.toml"), definition).unwrap();
        // the agent's sill home already holds its history
        let own = dir.join("state/sill/agents/fakelogin/.fakelogin");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("state.sqlite"), "history").unwrap();
        Self { dir }
    }

    fn run(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_sill")).arg("login").arg("fakelogin")
            .env("HOME", self.dir.join("home")).env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_CONFIG_HOME", self.dir.join("config")).env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env_remove("SILL_SESSION").env_remove("SILL_BOX").env_remove("BROWSER")
            .stdin(Stdio::null()).output().unwrap()
    }

    fn own(&self, file: &str) -> PathBuf { self.dir.join("state/sill/agents/fakelogin/.fakelogin").join(file) }

    fn leftovers(&self) -> Vec<String> {
        let mut out = vec![];
        if self.dir.join("state/sill/login/fakelogin").exists() { out.push("the login scratch (a credential)".into()); }
        if self.dir.join("home/.fakelogin").exists() { out.push("a ~/.fakelogin on the host".into()); }
        out
    }
}

impl Drop for HostLogin {
    fn drop(&mut self) {
        if !std::thread::panicking() { let _ = std::fs::remove_dir_all(&self.dir); }
    }
}

/// The adapter: `authenticate` starts a helper that keeps rewriting files in the login's home (the
/// copy back failed on them once), writes the fresh databases a new home gets (copied back, they
/// replaced the agent's history once), then the login file — or fails.
const ADAPTER: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
home = os.environ["FAKE_HOME"]
def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    if message.get("method") == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": [{"id": "browser", "name": "Browser"}]}})
    elif message.get("method") == "authenticate":
        busy = os.path.join(home, ".tmp")
        os.makedirs(busy, exist_ok=True)
        helper = subprocess.Popen([os.environ["FAKE_HELPER"], "-c", 'while :; do for i in 1 2 3 4 5 6 7 8; do : > "$0/f$i"; done; rm -f "$0"/f*; done', busy])
        # A login may not write outside its scratch home.
        try:
            with open(os.environ["FAKE_HELPER_PID"], "w") as f:
                f.write("escaped")
        except OSError:
            pass
        else:
            raise AssertionError("login wrote outside its jail")
        with open(os.path.join(home, "state.sqlite"), "w") as f:
            f.write("fresh")
        if os.environ.get("FAKE_FAIL"):
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32603, "message": "the provider refused the login"}})
        else:
            with open(os.path.join(home, "auth.json"), "w") as f:
                f.write("token")
            send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
"#;

#[test]
#[ignore = "requires working bubblewrap on a host"]
fn a_jailed_login_copies_only_credentials_and_cannot_write_outside() {
    let login = HostLogin::new(false);
    let out = login.run();
    assert!(out.status.success(), "sill login failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(login.own("auth.json")).unwrap(), "token", "the login reached the agent's sill home");
    assert_eq!(std::fs::read_to_string(login.own("state.sqlite")).unwrap(), "history", "the agent's history is untouched");
    assert!(!login.own(".tmp").exists(), "temp files are not the login");
    assert!(!login.dir.join("helper.pid").exists(), "login escaped its writable home");
    assert!(login.leftovers().is_empty(), "left behind: {:?}", login.leftovers());
}

#[test]
#[ignore = "requires working bubblewrap on a host"]
fn a_host_login_propagates_to_existing_ghost_session_homes() {
    let login = if let Some(dir) = std::env::var_os("SILL_GHOST_LOGIN_TEST_CHILD") {
        HostLogin { dir: dir.into() }
    } else {
        let login = HostLogin::new(false);
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "a_host_login_propagates_to_existing_ghost_session_homes", "--nocapture"])
            .env("SILL_GHOST_LOGIN_TEST_CHILD", &login.dir)
            .env("XDG_STATE_HOME", login.dir.join("state"))
            .status().unwrap();
        assert!(status.success());
        return;
    };
    let ghost = login.dir.join("ghost-home/fakelogin");
    std::fs::create_dir_all(ghost.join(".fakelogin")).unwrap();
    std::fs::write(ghost.join(".fakelogin/state.sqlite"), "ghost history").unwrap();
    let home = login.dir.join("home");
    // The subprocess owns its environment; the parallel test runner is never mutated.
    let agent = sill_core::agents::load(&home).unwrap().remove("fakelogin").unwrap();
    let mut env_agent = agent;
    env_agent.env.insert("XDG_STATE_HOME".into(), login.dir.join("state").display().to_string());
    // Run `run_into` with `ghost` as an existing ghost session's agent home copy.

    sill_core::login::run_into(&env_agent, &home, &[(ghost.clone(), sill_core::credentials::Rules::for_agent(&env_agent).unwrap())], "browser", |_| {}).unwrap();
    assert_eq!(std::fs::read_to_string(sill_core::session::agent_home(&home, "fakelogin").join(".fakelogin/auth.json")).unwrap(), "token");
    assert_eq!(std::fs::read_to_string(ghost.join(".fakelogin/auth.json")).unwrap(), "token", "the ghost home got the new login");
    assert_eq!(std::fs::read_to_string(ghost.join(".fakelogin/state.sqlite")).unwrap(), "ghost history", "the ghost's history is untouched");
    // The mount plan must expose registry credentials from the private ghost home, while
    // neither mounting the normal agent home nor importing its config or history.
    let mut registry_agent = env_agent.clone();
    registry_agent.home_paths = Some(vec![sill_core::agents::HomePath::directory("~/.fakelogin")]);
    let policy = sill_core::policy::Policy::builtin_default();
    let state = login.dir.join("ghost-state");
    let plan = sill_core::agent_storage::resolve(&registry_agent, &Default::default(), &policy, &state, &home, &[], true).unwrap();
    plan.prepare().unwrap();
    let binds = plan.binds().unwrap();
    let inside = home.join(".fakelogin/auth.json");
    let (source, target) = binds.iter().find(|(_, target)| inside.starts_with(target)).expect("ghost credential directory is mounted");
    let mounted = source.join(inside.strip_prefix(target).unwrap());
    assert_eq!(std::fs::read_to_string(mounted).unwrap(), "token");
    assert!(!source.join("state.sqlite").exists());
    assert!(!source.join("config.toml").exists());
    assert!(source.starts_with(&state));
    // A first Claude login needs a regular .claude.json even when no host copy exists.
    let claude = sill_core::agents::builtin().into_iter().find(|a| a.id == "claude").unwrap();
    let claude_state = login.dir.join("claude-ghost-state");
    let plan = sill_core::agent_storage::resolve(&claude, &Default::default(), &policy, &claude_state, &home, &[], true).unwrap();
    plan.prepare().unwrap();
    let binds = plan.binds().unwrap();
    let (source, _) = binds.iter().find(|(_, target)| *target == home.join(".claude.json")).unwrap();
    assert!(source.is_file());
    assert_eq!(std::fs::read(source).unwrap(), b"{}");
    assert!(!login.dir.join("helper.pid").exists());
}

#[test]
#[ignore = "requires working bubblewrap on a host"]
fn a_failed_host_login_leaves_nothing_behind() {
    let login = HostLogin::new(true);
    let out = login.run();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("the provider refused the login"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!login.own("auth.json").exists());
    assert_eq!(std::fs::read_to_string(login.own("state.sqlite")).unwrap(), "history");
    assert!(!login.dir.join("helper.pid").exists(), "login escaped its writable home");
    assert!(login.leftovers().is_empty(), "left behind: {:?}", login.leftovers());
}
