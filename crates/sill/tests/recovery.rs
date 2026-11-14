//! A real daemon and its real persistent ACP worker, with a scripted shim/agent instead of a jail.
//! The test deliberately restarts the daemon with nobody attached: surviving processes alone are
//! insufficient unless the transcript, question and closing summary are recovered too.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sill_proto::method;

struct Connection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
    events: Vec<Value>,
}

impl Connection {
    fn connect(socket: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut connection=Self { reader: BufReader::new(stream.try_clone()?), writer: stream, next: 1, events: Vec::new() };
        connection.call(sill_proto::initialize::METHOD,json!({"protocols":[sill_proto::initialize::PROTOCOL_ID],"client":{"name":"recovery-test","version":"0"},"capabilities":{"sill.internal":1}}));
        Ok(connection)
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        writeln!(self.writer, "{}", json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).unwrap();
        loop {
            let mut line = String::new();
            assert!(self.reader.read_line(&mut line).unwrap() > 0, "daemon closed the connection before answering {method}");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message.get("method").is_some() {
                self.events.push(message);
                continue;
            }
            assert_eq!(message["id"], id, "{message}");
            assert!(message.get("error").is_none(), "{method}: {message}");
            return message["result"].clone();
        }
    }
}

struct Fixture {
    dir: PathBuf,
    session: String,
    daemon: Option<Child>,
    relay: Child,
    launcher: Child,
    shutdown: Arc<AtomicBool>,
    shim: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn environment(dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sill"));
        command.env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("HOME", dir.join("home"))
            .env("SILL_LAUNCHER", "direct")
            .env_remove("SILL_SESSION").env_remove("INVOCATION_ID")
            .env_remove("LISTEN_PID").env_remove("LISTEN_FDS").env_remove("NOTIFY_SOCKET")
            .stdin(Stdio::null()).stdout(Stdio::null());
        command
    }

    fn new() -> (Self, mpsc::Sender<()>, mpsc::Receiver<String>) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("sill-live-recovery-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["run/sill", "state/sill", "config/sill", "home"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("config/sill/instructions.md"), "Keep the recovery test's instructions.").unwrap();
        let store = sill_core::store::Store::open(&dir.join("state/sill/sill.db")).unwrap();
        let session = store.create("claude", "default").unwrap().id;
        let pin = dir.join("home/pinned.txt");
        std::fs::write(&pin, "focus").unwrap();
        store.pin(&session, &pin).unwrap();
        let global = dir.join("home/global-only.txt");
        std::fs::write(&global, "mount only").unwrap();
        store.global_pin(&global).unwrap();
        drop(store); // the daemon remains the sole store writer from here on
        let runtime = dir.join("run/sill").join(&session);
        std::fs::create_dir_all(&runtime).unwrap();
        // A separate, harmless launch process lets adoption and stop exercise real PID handling.
        let launcher = Command::new("sleep").arg("120").spawn().unwrap();
        std::fs::write(runtime.join("launch.pid"), launcher.id().to_string()).unwrap();
        let listener = UnixListener::bind(runtime.join("shim.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let (release, released) = mpsc::channel();
        let released = Arc::new(std::sync::Mutex::new(released));
        let (observed, observations) = mpsc::channel();
        let shim = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let released = released.clone();
                        let observed = observed.clone();
                        std::thread::spawn(move || serve_shim(stream, released, observed));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        });
        let log = std::fs::File::create(dir.join("relay.log")).unwrap();
        let relay = Self::environment(&dir).args(["daemon", "relay", "--session", &session, "--socket"])
            .arg(runtime.join("relay.sock")).arg("--agent-socket").arg(runtime.join("shim.sock"))
            .arg("--cwd").arg(dir.join("home")).stderr(log).spawn().unwrap();
        let fixture = Self { dir, session, daemon: None, relay, launcher, shutdown, shim: Some(shim) };
        fixture.wait("relay socket", || runtime.join("relay.sock").exists());
        (fixture, release, observations)
    }

    fn socket(&self) -> PathBuf { self.dir.join("run/sill/daemon.sock") }

    fn logs(&self) -> String {
        format!("daemon:\n{}\nrelay:\n{}", std::fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default(), std::fs::read_to_string(self.dir.join("relay.log")).unwrap_or_default())
    }

    fn wait(&self, what: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}; {}", self.logs());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn start(&mut self) {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("daemon.log")).unwrap();
        self.daemon = Some(Self::environment(&self.dir).arg("daemon").stderr(log).spawn().unwrap());
        self.wait("daemon status", || {
            let Ok(mut connection) = Connection::connect(&self.socket()) else { return false };
            let status = connection.call(method::DAEMON_STATUS, json!({}));
            status["pid"] == self.daemon.as_ref().unwrap().id()
        });
        self.wait("recovered conversation", || {
            self.snapshot().get("ready").is_some_and(|ready| !ready.is_null())
                || self.database().query_row("SELECT ended_at IS NOT NULL FROM sessions WHERE id=?1", [&self.session], |row| row.get::<_, bool>(0)).unwrap_or(false)
                || self.database().query_row("SELECT count(*)=0 FROM sessions WHERE id=?1", [&self.session], |row| row.get::<_, bool>(0)).unwrap_or(false)
        });
    }

    fn database(&self) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(self.dir.join("state/sill/sill.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }

    fn snapshot(&self) -> Value {
        self.database().query_row("SELECT state FROM daemon_runners WHERE id=?1", [&self.session], |row| row.get::<_, String>(0))
            .ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null)
    }

    fn stop(&mut self, crash: bool) {
        let mut child = self.daemon.take().unwrap();
        if crash {
            child.kill().unwrap();
        } else {
            Connection::connect(&self.socket()).unwrap().call(method::DAEMON_SHUTDOWN, json!({}));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.success(), !crash);
                break;
            }
            assert!(Instant::now() < deadline, "daemon did not stop; {}", self.logs());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(self.relay.try_wait().unwrap().is_none(), "relay died with main daemon; {}", self.logs());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() { let _ = child.kill(); let _ = child.wait(); }
        let _ = self.relay.kill(); let _ = self.relay.wait();
        let _ = self.launcher.kill(); let _ = self.launcher.wait();
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(shim) = self.shim.take() { let _ = shim.join(); }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn send(writer: &mut UnixStream, message: Value) { writeln!(writer, "{message}").unwrap(); }

fn read(reader: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut line = String::new();
    (reader.read_line(&mut line).ok()? > 0).then(|| serde_json::from_str(&line).unwrap())
}

fn chunk(writer: &mut UnixStream, text: &str) {
    send(writer, json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}}));
}

fn serve_shim(mut stream: UnixStream, release: Arc<std::sync::Mutex<mpsc::Receiver<()>>>, observed: mpsc::Sender<String>) {
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let Some(role) = read(&mut reader) else { return };
    if role["role"] == "control" {
        while let Some(request) = read(&mut reader) {
            let response=if request["op"] == "status" {
                json!({"ok":true,"running":true,"pid":std::process::id(),"exit":null,"buffered_lines":0})
            } else { json!({"ok":true}) };
            // Presence control connections can close after sending during daemon shutdown.
            if writeln!(stream,"{response}").is_err() {return;}
        }
        return;
    }
    assert_eq!(role["role"], "agent");
    let initialize = read(&mut reader).unwrap();
    assert_eq!(initialize["method"], "initialize");
    observed.send("initialize".into()).unwrap();
    send(&mut stream, json!({"jsonrpc":"2.0","id":initialize["id"],"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"authMethods":[]}}));
    let new_session = read(&mut reader).unwrap();
    assert_eq!(new_session["method"], "session/new");
    send(&mut stream, json!({"jsonrpc":"2.0","id":new_session["id"],"result":{"sessionId":"fake-session"}}));
    let Some(prompt) = read(&mut reader) else {
        observed.send("finished".into()).unwrap();
        return;
    };
    assert_eq!(prompt["method"], "session/prompt");
    observed.send(prompt["params"]["prompt"][0]["text"].as_str().unwrap().to_string()).unwrap();
    chunk(&mut stream, "before restart ");
    release.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
    send(&mut stream, json!({"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"fake-session","toolCall":{"toolCallId":"tool","title":"continue","status":"pending"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}));
    let answer = read(&mut reader).unwrap();
    assert_eq!(answer["id"], 99);
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow");
    chunk(&mut stream, "after restart");
    send(&mut stream, json!({"jsonrpc":"2.0","id":prompt["id"],"result":{"stopReason":"end_turn"}}));
    let summary = read(&mut reader).unwrap();
    assert_eq!(summary["method"], "session/prompt");
    chunk(&mut stream, "Title: Recovered summary\n\nSummary before restart ");
    release.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();
    chunk(&mut stream, "and after restart.");
    send(&mut stream, json!({"jsonrpc":"2.0","id":summary["id"],"result":{"stopReason":"end_turn"}}));
    observed.send("summary-complete".into()).unwrap();
    // Closing the session asks the worker to quit, which closes its one ACP connection.
    assert!(read(&mut reader).is_none(), "the SDK was restarted or submitted an extra prompt");
    observed.send("finished".into()).unwrap();
}

#[test]
fn daemon_restarts_recover_turn_permission_and_closing_summary_without_a_frontend() {
    exercise_recovery(false, false);
}

#[test]
fn a_checkpoint_adopted_as_the_end_survives_a_daemon_restart() {
    exercise_recovery(true, false);
}

/// The first deploy after the relay exists (docs/daemon.md §3.9 item 1): the surviving relay is
/// another build's. Here it answers Hello the way relays did before the version existed — no
/// `version`, which reads as 0 — from a listener in this test, with a process named like a relay
/// behind `relay.pid`. The daemon must evict it, start a real relay and reach the agent.
#[test]
fn a_relay_of_another_version_is_evicted_and_replaced_at_recovery() {
    let (mut fixture, _release, observed) = Fixture::new();
    let runtime = fixture.dir.join("run/sill").join(&fixture.session);
    // the fixture's real relay makes way for the old one: killed, its files gone
    let _ = fixture.relay.kill();
    let _ = fixture.relay.wait();
    for f in ["relay.sock", "relay.pid", "relay.lock"] {
        let _ = std::fs::remove_file(runtime.join(f));
    }
    // a process whose command line names it as this session's relay, for relay.pid; `; true`
    // keeps the shell from exec-ing sleep in its place. The fake socket below goes when it dies,
    // as a real relay's does with its process.
    let mut stale = Command::new("sh").args(["-c", "sleep 30; true", "sill", "daemon", "relay", "--session", &fixture.session]).spawn().unwrap();
    std::fs::write(runtime.join("relay.pid"), stale.id().to_string()).unwrap();
    std::fs::write(runtime.join("relay.lock"), "").unwrap();
    let socket_path = runtime.join("relay.sock");
    let old = UnixListener::bind(&socket_path).unwrap();
    old.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let serving = stop.clone();
    let stale_pid = stale.id();
    let listener = std::thread::spawn(move || {
        let alive = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z"));
        while !serving.load(Ordering::Relaxed) {
            if !alive(stale_pid) {
                let _ = std::fs::remove_file(&socket_path);
                break;
            }
            match old.accept() {
                Ok((stream, _)) => {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut hello = String::new();
                    let _ = reader.read_line(&mut hello);
                    let mut writer = stream;
                    let _ = writeln!(writer, "{}", json!({"Hello": {"acknowledged": 0, "last": 0}}));
                    std::thread::sleep(Duration::from_millis(200)); // the daemon reads before this closes
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
    });
    fixture.start();
    // recovery runs after READY: the eviction, then a real relay that reaches the fake agent
    assert_eq!(observed.recv_timeout(Duration::from_secs(15)).unwrap(), "initialize", "{}", fixture.logs());
    let deadline = Instant::now() + Duration::from_secs(5);
    while stale.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the stale relay process was not killed; {}", fixture.logs());
        std::thread::sleep(Duration::from_millis(20));
    }
    let new_pid: u32 = std::fs::read_to_string(runtime.join("relay.pid")).unwrap().trim().parse().unwrap();
    assert_ne!(new_pid, stale.id(), "relay.pid still names the evicted process");
    let log = fixture.logs();
    assert!(log.contains("relay protocol 0") && log.contains("replacing it") && log.contains("evicted ACP relay pid"), "{log}");
    stop.store(true, Ordering::Relaxed);
    let _ = listener.join();
}

#[test]
fn a_lost_worker_finishes_the_persisted_end_instead_of_starting_another_summary() {
    exercise_recovery(false, true);
}

#[test]
fn an_unowned_dead_ghost_worker_stops_before_its_runtime_and_memory_are_scrubbed() {
    let (mut fixture, _release, observed) = Fixture::new();
    // The live agent stream must already be open before adoption removes the dead jail's socket.
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), "initialize");
    // No main daemon has run yet; this simulates its prior death together with the ghost jail.
    rusqlite::Connection::open(fixture.dir.join("state/sill/sill.db")).unwrap()
        .execute("UPDATE sessions SET ghost=1 WHERE id=?1", [&fixture.session]).unwrap();
    fixture.launcher.kill().unwrap();
    fixture.launcher.wait().unwrap();
    fixture.start();
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), "finished");
    assert_eq!(fixture.database().query_row("SELECT count(*) FROM sessions WHERE id=?1", [&fixture.session], |row| row.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(fixture.database().query_row("SELECT count(*) FROM daemon_runners WHERE id=?1", [&fixture.session], |row| row.get::<_, i64>(0)).unwrap(), 0);
    assert!(!fixture.dir.join("run/sill").join(&fixture.session).exists());
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.relay.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "deleted ghost left a worker alive; {}", fixture.logs());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn exercise_recovery(adopt_checkpoint: bool, lose_worker: bool) {
    let (mut fixture, release, observed) = Fixture::new();
    fixture.start();
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), "initialize");
    let mut origin = Connection::connect(&fixture.socket()).unwrap();
    origin.call(method::SESSIONS_ATTACH, json!({"key":fixture.session,"fresh":false}));
    let mut follower = Connection::connect(&fixture.socket()).unwrap();
    follower.call(method::SESSIONS_ATTACH, json!({"key":fixture.session,"fresh":false}));
    assert_eq!(origin.events.first().unwrap()["method"], method::SESSION_REPLAY);
    assert_eq!(follower.events.first().unwrap()["method"], method::SESSION_REPLAY);
    origin.call(method::SESSIONS_COMMAND, json!({"key":fixture.session,"command":sill_proto::wire::Command::Prompt {
        text: "work".into(), attachments: vec![], entry: Some(sill_proto::wire::Entry::User { text: "work".into(), names: vec![] }),
    }}));
    origin.call(method::DAEMON_STATUS, json!({}));
    follower.call(method::DAEMON_STATUS, json!({}));
    assert!(follower.events.iter().any(|event| event["params"]["event"]["UserText"] == "work"), "the second frontend sees the plain accepted prompt");
    assert!(!origin.events.iter().any(|event| event["params"]["event"].get("UserText").is_some()), "the sending frontend must not receive its optimistic user entry twice");
    let prepared = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(prepared.contains(sill_core::instructions::BASE));
    assert!(prepared.contains("Keep the recovery test's instructions."));
    assert!(prepared.contains("pinned.txt"));
    assert!(!prepared.contains("global-only.txt"));
    origin.call(method::SESSIONS_DETACH, json!({"key":fixture.session}));
    follower.call(method::SESSIONS_DETACH, json!({"key":fixture.session}));
    drop(origin);
    drop(follower);
    fixture.wait("partial streamed text committed", || fixture.snapshot()["agent_md"] == "before restart ");
    fixture.stop(true);
    release.send(()).unwrap(); // the worker receives the permission request while the daemon is absent
    fixture.start();
    fixture.wait("unattended permission recovered", || fixture.snapshot()["pending"].as_object().is_some_and(|questions| questions.len() == 1));
    let mut frontend = Connection::connect(&fixture.socket()).unwrap();
    frontend.call(method::SESSIONS_ATTACH, json!({"key":fixture.session,"fresh":false}));
    let question = frontend.events.iter().find_map(|event| event["params"]["event"].get("PermissionRequest")).expect("pending permission delivered on attach");
    let request = question["request"].as_u64().unwrap();
    frontend.call(method::SESSIONS_ANSWER, json!({"key":fixture.session,"request":request,"answer":{"Permission":"allow"}}));
    fixture.wait("first turn complete", || fixture.snapshot()["busy"] == false);
    let user: String = fixture.database().query_row("SELECT text FROM entries WHERE session_id=?1 AND kind='user'", [&fixture.session], |row| row.get(0)).unwrap();
    assert_eq!(user, "work", "the expanded prompt must never replace the plain transcript");
    let (count, text): (i64, String) = fixture.database().query_row("SELECT count(*), group_concat(text,'') FROM entries WHERE session_id=?1 AND kind='agent'", [&fixture.session], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!((count, text.as_str()), (1, "before restart after restart"));
    frontend.call(method::SESSIONS_DETACH, json!({"key":fixture.session}));
    drop(frontend);
    if adopt_checkpoint {
        Connection::connect(&fixture.socket()).unwrap().call(method::SESSIONS_CHECKPOINT, json!({"key":fixture.session}));
        fixture.wait("partial checkpoint committed", || fixture.snapshot()["agent_md"].as_str().is_some_and(|text| text.contains("Summary before restart")));
    }
    let ending = Connection::connect(&fixture.socket()).unwrap().call(method::SESSIONS_END, json!({"key":fixture.session,"reason":"close"}));
    assert_eq!(ending["pending"], true, "the agent should write the final summary");
    fixture.wait("partial closing summary committed", || fixture.snapshot()["agent_md"].as_str().is_some_and(|text| text.contains("Summary before restart")));
    fixture.stop(false);
    release.send(()).unwrap(); // the summary completes with nobody attached, including no daemon
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), "summary-complete");
    if lose_worker {
        fixture.relay.kill().unwrap();
        fixture.relay.wait().unwrap();
    }
    fixture.start();
    fixture.wait("closing summary finalized without an attach", || fixture.database().query_row("SELECT ended_at IS NOT NULL FROM sessions WHERE id=?1", [&fixture.session], |row| row.get::<_, bool>(0)).unwrap_or(false));
    let (title, summary, kind): (String, String, String) = fixture.database().query_row("SELECT title,summary,summary_kind FROM sessions WHERE id=?1", [&fixture.session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
    if lose_worker {
        assert_eq!(kind, "auto");
        assert!(!summary.is_empty());
        assert!(fixture.snapshot()["retired"] == true);
    } else {
        assert_eq!(title, "Recovered summary");
        assert_eq!(summary, "Summary before restart and after restart.");
        assert_eq!(kind, "agent");
    }
    let count: i64 = fixture.database().query_row("SELECT count(*) FROM entries WHERE session_id=?1 AND kind='agent'", [&fixture.session], |row| row.get(0)).unwrap();
    assert_eq!(count, if lose_worker { 1 } else { 2 }, "neither turn nor summary may be appended twice");
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), "finished");
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.relay.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "finished worker did not exit; {}", fixture.logs());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn public_snapshot(connection:&mut Connection, session:&str)->(Value,Vec<Value>) {
    let attached=connection.call("_sill/sessions.attach",json!({"session_id":session}));
    assert_eq!(attached["mode"],"snapshot");
    let lease=&attached["snapshot"];
    let mut records=vec![];let mut page=Value::Null;
    loop {
        let response=connection.call("_sill/sessions.snapshot",json!({"token":lease["token"],"page":page}));
        assert_eq!(response["at"],lease["at"]);
        records.extend(response["records"].as_array().unwrap().iter().cloned());
        page=response["next_page"].clone();
        if page.is_null(){break;}
    }
    (attached,records)
}

#[test]
fn sfp_submission_question_and_item_identity_survive_main_daemon_restart() {
    let (mut fixture,release,observed)=Fixture::new();
    fixture.start();
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(),"initialize");
    let mut client=Connection::connect(&fixture.socket()).unwrap();
    let (_,initial)=public_snapshot(&mut client,&fixture.session);
    let generation=initial.iter().find(|r|r["type"]=="session").unwrap()["session"]["agent_generation_id"].clone();
    let input=json!({"session_id":fixture.session,"submission_id":"recovery-submission","text":"work","attachments":[]});
    let accepted=client.call("_sill/sessions.submit",input.clone());
    let repeated=client.call("_sill/sessions.submit",input.clone());
    assert_eq!(accepted["submission_id"],repeated["submission_id"]);
    assert_eq!(accepted["accepted_seq"],repeated["accepted_seq"]);
    let prepared=observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(prepared.contains("work") && prepared.contains(sill_core::instructions::BASE));
    fixture.wait("SFP partial text committed",||fixture.snapshot()["agent_md"]=="before restart ");
    let (before,records)=public_snapshot(&mut client,&fixture.session);
    let agent_item=records.iter().find(|r|r["type"]=="item" && r["item"]["kind"]=="agent").unwrap()["item"].clone();
    assert_eq!(agent_item["content"][0]["text"],"before restart ");
    let cursor=before["snapshot"]["at"].clone();
    drop(client);
    fixture.stop(true);
    release.send(()).unwrap();
    fixture.start();
    fixture.wait("SFP unattended question recovered",||fixture.snapshot()["pending"].as_object().is_some_and(|questions|questions.len()==1));
    let mut client=Connection::connect(&fixture.socket()).unwrap();
    let attached=client.call("_sill/sessions.attach",json!({"session_id":fixture.session,"since":cursor}));
    assert_eq!(attached["mode"],"replay");
    assert_eq!(attached["after"],cursor);
    assert!(client.events.is_empty(),"attach response precedes this subscription's notifications");
    client.call(method::DAEMON_STATUS,json!({})); // drain the replay already queued after attach's reply
    let replay:Vec<_>=client.events.iter().filter(|e|e["method"]=="_sill/session.event" && e["params"]["subscription_id"]==attached["subscription_id"]).collect();
    let mut seq=cursor["seq"].as_str().unwrap().parse::<u64>().unwrap();
    for event in replay {
        seq+=1;
        assert_eq!(event["params"]["seq"],seq.to_string(),"replay must be contiguous");
    }
    assert_eq!(attached["through"]["seq"],seq.to_string());
    let (_,after)=public_snapshot(&mut client,&fixture.session);
    let recovered_session=&after.iter().find(|r|r["type"]=="session").unwrap()["session"];
    assert_eq!(recovered_session["agent_generation_id"],generation,"adopting the live relay preserves its generation");
    let recovered_item=&after.iter().find(|r|r["type"]=="item" && r["item"]["item_id"]==agent_item["item_id"]).unwrap()["item"];
    assert_eq!(recovered_item,&agent_item,"replay must not append the first text twice or reidentify its item");
    let repeated=client.call("_sill/sessions.submit",input);
    assert_eq!(repeated["accepted_seq"],accepted["accepted_seq"],"retry after restart retains original acceptance");
    let question=&after.iter().find(|r|r["type"]=="question" && r["question"]["state"]=="open").unwrap()["question"];
    let answer=json!({"session_id":fixture.session,"question_id":question["question_id"],"answer_id":"recovery-answer","answer":{"type":"permission","option_id":"allow"}});
    let answered=client.call("_sill/questions.answer",answer.clone());
    let retried=client.call("_sill/questions.answer",answer);
    assert_eq!(answered["answer_id"],"recovery-answer");
    assert_eq!(retried["answer_id"],answered["answer_id"]);
    fixture.wait("SFP turn complete",||fixture.snapshot()["busy"]==false);
    let (_,completed)=public_snapshot(&mut client,&fixture.session);
    let final_item=&completed.iter().find(|r|r["type"]=="item" && r["item"]["item_id"]==agent_item["item_id"]).unwrap()["item"];
    assert_eq!(final_item["content"][0]["text"],"before restart after restart");
    let result=client.call("_sill/submissions.get",json!({"session_id":fixture.session,"submission_id":"recovery-submission"}));
    assert_eq!(result["outcome"],"completed");
    assert_eq!(fixture.database().query_row("SELECT count(*) FROM entries WHERE session_id=?1 AND kind='user'",[&fixture.session],|r|r.get::<_,i64>(0)).unwrap(),1);
    let ending=client.call(method::SESSIONS_END,json!({"key":fixture.session,"reason":"close"}));
    assert_eq!(ending["pending"],true);
    fixture.wait("SFP closing summary started",||fixture.snapshot()["agent_md"].as_str().is_some_and(|t|t.contains("Summary before restart")));
    release.send(()).unwrap();
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(),"summary-complete");
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(),"finished");
}
