//! The stdio endpoint relays a persistent daemon and closes only its own connection.
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use serde_json::{Value, json};

struct Fixture { dir: PathBuf, daemon: Child }
impl Fixture {
    fn command(&self) -> Command { command(&self.dir) }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill(); let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn command(dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sill"));
    command.env_remove("SILL_SESSION").env_remove("SILL_BOX").env_remove("LISTEN_PID").env_remove("LISTEN_FDS")
        .env_remove("INVOCATION_ID").env_remove("NOTIFY_SOCKET").env("HOME", dir.join("home"))
        .env("XDG_STATE_HOME", dir.join("state")).env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_RUNTIME_DIR", dir.join("run")).env("SILL_LAUNCHER", "direct")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    command
}
fn fixture() -> Fixture {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("stdio-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    for sub in ["home", "state", "config", "run"] { std::fs::create_dir_all(dir.join(sub)).unwrap(); }
    let daemon = command(&dir).arg("daemon").spawn().unwrap();
    let mut fixture = Fixture { dir, daemon };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::os::unix::net::UnixStream::connect(fixture.dir.join("run/sill/daemon.sock")).is_ok() { return fixture; }
        assert!(fixture.daemon.try_wait().unwrap().is_none(), "daemon exited before listening");
        assert!(Instant::now() < deadline, "daemon did not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
fn stdio_connection_close_leaves_the_daemon_running() {
    let mut fixture = fixture();
    let mut proxy = fixture.command().args(["daemon", "--stdio"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = proxy.stdin.take().unwrap();
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":0,"method":"_sill/initialize","params":{"protocols":[sill_proto::initialize::PROTOCOL_ID],"client":{"name":"test","version":"1"},"capabilities":{}}})).unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":1,"method":sill_proto::method::DAEMON_STATUS,"params":{}})).unwrap();
    let reader = BufReader::new(proxy.stdout.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || { let mut lines=reader.lines();
        let init:Value=serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(init["result"]["protocol"],sill_proto::initialize::PROTOCOL_ID);
        let _ = tx.send(lines.next().unwrap().unwrap()); });
    let answer: Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
    assert_eq!(answer["result"]["pid"], fixture.daemon.id());
    drop(input);
    worker.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = proxy.try_wait().unwrap() { assert!(status.success()); break; }
        if Instant::now() >= deadline { let _ = proxy.kill(); panic!("stdio proxy stayed alive after EOF"); }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(fixture.daemon.try_wait().unwrap().is_none(), "closing stdio stopped daemon");
}

#[test]
fn selected_box_uses_a_pipe_and_does_not_start_a_local_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let mut remote = fixture();
    let local = remote.dir.join("frontend");
    for sub in ["home", "state", "config/sill", "run", "bin"] { std::fs::create_dir_all(local.join(sub)).unwrap(); }
    let ssh = local.join("bin/ssh");
    std::fs::write(&ssh, "#!/bin/sh\ncase \" $* \" in *\" -O \"*) exit 0;; esac\nexport HOME=\"$SILL_TEST_REMOTE_ROOT/home\" XDG_RUNTIME_DIR=\"$SILL_TEST_REMOTE_ROOT/run\" XDG_STATE_HOME=\"$SILL_TEST_REMOTE_ROOT/state\" XDG_CONFIG_HOME=\"$SILL_TEST_REMOTE_ROOT/config\"\nunset SILL_BOX SILL_SESSION\nexec \"$SILL_TEST_BINARY\" daemon --stdio\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(local.join("config/sill/boxes.json"), serde_json::to_vec(&json!({"test":{"host":"test-host","binary":"sill"}})).unwrap()).unwrap();
    let output = command(&local).args(["--box", "test", "daemon", "status"])
        .env("PATH", format!("{}:{}", local.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .env("SILL_TEST_REMOTE_ROOT", &remote.dir).env("SILL_TEST_BINARY", env!("CARGO_BIN_EXE_sill"))
        .stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("pid {}", remote.daemon.id())));
    for args in [vec!["memory", "export", "exported"], vec!["memory", "refresh"]] {
        let output = command(&local).args(["--box", "test"]).args(args).current_dir(&local)
            .env("PATH", format!("{}:{}", local.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
            .env("SILL_TEST_REMOTE_ROOT", &remote.dir).env("SILL_TEST_BINARY", env!("CARGO_BIN_EXE_sill"))
            .stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(!local.join("state/sill/sill.db").exists(), "remote memory operation opened a frontend store");
    assert!(!local.join("run/sill/daemon.sock").exists(), "remote operation started a local daemon");
    assert!(remote.daemon.try_wait().unwrap().is_none(), "remote client killed server");
}

#[test]
fn box_bootstrap_installs_daemon_and_shim_before_saving_connection() {
    use std::os::unix::fs::PermissionsExt;
    let remote = fixture();
    let local = remote.dir.join("bootstrap-client");
    for sub in ["home", "state", "config/sill", "run", "bin", "source"] { std::fs::create_dir_all(local.join(sub)).unwrap(); }
    let ssh = local.join("bin/ssh");
    // Execute precisely the remote command selected by OpenSSH, while keeping this test local.
    std::fs::write(&ssh, "#!/bin/sh\ncase \" $* \" in *\" -O \"*) exit 0;; esac\nfor sill_arg do :; done\nexport HOME=\"$SILL_TEST_REMOTE_ROOT/home\" XDG_RUNTIME_DIR=\"$SILL_TEST_REMOTE_ROOT/run\" XDG_STATE_HOME=\"$SILL_TEST_REMOTE_ROOT/state\" XDG_CONFIG_HOME=\"$SILL_TEST_REMOTE_ROOT/config\"\nunset SILL_BOX SILL_SESSION\nexec /bin/sh -c \"$sill_arg\"\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_sill"), local.join("source/sill")).unwrap();
    std::fs::write(local.join("source/sill-shim"), b"shim bootstrap fixture\n").unwrap();
    let output = command(&local).args(["box", "add", "test-host", "--name", "installed", "--install"])
        .arg(local.join("source/sill"))
        .env("PATH", format!("{}:{}", local.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .env("SILL_TEST_REMOTE_ROOT", &remote.dir).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read(remote.dir.join("home/.local/bin/sill-shim")).unwrap(), b"shim bootstrap fixture\n");
    assert_eq!(std::fs::metadata(remote.dir.join("home/.local/bin/sill")).unwrap().len(), std::fs::metadata(local.join("source/sill")).unwrap().len());
    let saved: Value = serde_json::from_slice(&std::fs::read(local.join("config/sill/boxes.json")).unwrap()).unwrap();
    assert_eq!(saved["installed"]["binary"], "~/.local/bin/sill");
}

#[test]
fn stdio_does_not_replace_an_existing_daemon_with_another_protocol() {
    let mut fixture = fixture();
    fixture.daemon.kill().unwrap();
    fixture.daemon.wait().unwrap();
    let path = fixture.dir.join("run/sill/daemon.sock");
    std::fs::remove_file(&path).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut writer = socket.try_clone().unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["method"], "_sill/initialize");
        writeln!(writer, "{}", json!({"jsonrpc":"2.0","id":req["id"],"result":{"protocol":999}})).unwrap();
    });
    let mut proxy = fixture.command().args(["daemon", "--stdio"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    writeln!(proxy.stdin.as_mut().unwrap(), "{}", json!({"jsonrpc":"2.0","id":8,"method":"_sill/initialize","params":{"protocols":["sfp/1-dev.1"],"client":{"name":"test","version":"1"}}})).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let output = proxy.stdout.take().unwrap();
    let reader = std::thread::spawn(move || { let _ = tx.send(BufReader::new(output).lines().next()); });
    let received = rx.recv_timeout(Duration::from_secs(5));
    if received.is_err() { let _ = proxy.kill(); }
    let answer: Value = serde_json::from_str(&received.unwrap().unwrap().unwrap()).unwrap();
    assert_eq!(answer["result"]["protocol"], 999);
    drop(proxy.stdin.take());
    assert!(proxy.wait().unwrap().success());
    reader.join().unwrap();
    server.join().unwrap();
}

#[test]
#[ignore = "requires OpenSSH client/server and Python jsonschema; creates disposable loopback credentials"]
fn real_ssh_control_and_terminal_reconnection() {
    let output = Command::new(std::env::var("SILL_SFP_PYTHON").unwrap_or_else(|_| "python3".into()))
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sfp_ssh_scenario.py"))
        .arg(env!("CARGO_BIN_EXE_sill")).output().unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
