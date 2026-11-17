//! The box's shared SSH connection against a real OpenSSH server on the loopback, with a
//! passphrase-protected key and no agent: the case where only the person can sign in.
//! Skipped where `sshd` or `ssh-keygen` is missing. This program is also its own askpass helper
//! (`harness = false`): ssh starts it for every question, as it starts sill-gui.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sill_proto::boxlink::{self, Problem, Prompt, PromptKind, Prompter, Unavailable};

const HOST: &str = "sill-test-box";
const PASSPHRASE: &str = "open sesame";

fn main() {
    if boxlink::is_askpass() {
        boxlink::askpass_main();
    }
    let Some(sshd) = ["/usr/sbin/sshd", "/usr/bin/sshd"].into_iter().find(|p| Path::new(p).exists()) else {
        return println!("boxlink: skipped, no sshd");
    };
    let dir = PathBuf::from(format!("/tmp/sbl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("run")).unwrap();
    let keygen = |name: &str, passphrase: &str| {
        Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", passphrase, "-f"]).arg(dir.join(name)).status().is_ok_and(|s| s.success())
    };
    if !keygen("hostkey", "") || !keygen("userkey", PASSPHRASE) {
        return println!("boxlink: skipped, no ssh-keygen");
    }
    std::fs::copy(dir.join("userkey.pub"), dir.join("authorized_keys")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let d = dir.display();
    std::fs::write(dir.join("sshd_config"), format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/hostkey\nAuthorizedKeysFile {d}/authorized_keys\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes no\nPidFile {d}/sshd.pid\n")).unwrap();
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    std::fs::write(dir.join("ssh_config"), format!("Host {HOST}\n  HostName 127.0.0.1\n  Port {port}\n  User {}\n  IdentityFile {d}/userkey\n  IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile {d}/known_hosts\n", user.trim())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(dir.join("bin/ssh"), format!("#!/bin/sh\nexec /usr/bin/ssh -F {d}/ssh_config \"$@\"\n")).unwrap();
    std::fs::set_permissions(dir.join("bin/ssh"), std::fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: nothing else runs yet.
    unsafe {
        std::env::set_var("PATH", format!("{d}/bin:{}", std::env::var("PATH").unwrap_or_default()));
        std::env::set_var("XDG_RUNTIME_DIR", dir.join("run"));
        std::env::remove_var("SSH_AUTH_SOCK");
    }
    let start_server = || {
        let started = Command::new(sshd).arg("-f").arg(dir.join("sshd_config")).arg("-E").arg(dir.join("sshd.log")).status().is_ok_and(|s| s.success());
        let deadline = Instant::now() + Duration::from_secs(5);
        while started && std::net::TcpStream::connect(("127.0.0.1", port)).is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        started && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    };
    let stop_server = || {
        let pid = std::fs::read_to_string(dir.join("sshd.pid")).unwrap_or_default();
        let _ = Command::new("kill").arg(pid.trim()).status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    if !start_server() {
        return println!("boxlink: skipped, sshd did not start here");
    }
    let result = std::panic::catch_unwind(|| scenario(&start_server, &stop_server));
    let _ = boxlink::disconnect(HOST);
    stop_server();
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    println!("boxlink: ok");
}

fn problem(error: &anyhow::Error) -> Problem {
    error.downcast_ref::<Unavailable>().unwrap_or_else(|| panic!("not a box state: {error:#}")).problem
}

fn session(command: &str) -> std::process::Output {
    sill_proto::transport::ssh_base(HOST).unwrap().arg(command).stdin(Stdio::null()).output().unwrap()
}

fn scenario(start_server: &dyn Fn() -> bool, stop_server: &dyn Fn()) {
    let asked: Arc<Mutex<Vec<Prompt>>> = Arc::default();
    let answering = |answers: &'static [(&'static str, Option<&'static str>)]| -> Prompter {
        let asked = asked.clone();
        Arc::new(move |prompt: Prompt| {
            asked.lock().unwrap().push(prompt.clone());
            answers.iter().find(|(words, _)| prompt.text.contains(words)).and_then(|(_, answer)| answer.map(str::to_string))
        })
    };

    // unknown host key, no agent: the background attempt cannot succeed and says why
    assert_eq!(problem(&boxlink::ensure(HOST).unwrap_err()), Problem::HostKey);
    let shown = boxlink::status().unwrap();
    assert_eq!((shown.problem, shown.retry_in), (Problem::HostKey, None), "{shown:?}");
    // and nothing more reaches the server until the person acts
    let before = Instant::now();
    for _ in 0..20 {
        assert_eq!(problem(&boxlink::ensure(HOST).unwrap_err()), Problem::HostKey);
    }
    assert!(before.elapsed() < Duration::from_secs(2), "waiting for the person costs no network attempts");

    // one Cancel ends the sign-in, whatever SSH would have asked next
    let error = boxlink::sign_in(HOST, Some(answering(&[("yes/no", Some("yes")), ("passphrase", None)]))).unwrap_err();
    assert_eq!(problem(&error), Problem::SignIn, "{error:#}");
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    {
        let mut asked = asked.lock().unwrap();
        assert_eq!(asked.iter().map(|p| p.kind).collect::<Vec<_>>(), [PromptKind::Text, PromptKind::Secret], "{asked:?}");
        assert!(asked.iter().all(|p| p.host == HOST));
        asked.clear();
    }

    // a wrong passphrase is SSH's to judge; it asks again and the right one signs in
    static TRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let asked_too = asked.clone();
    let retyping: Prompter = Arc::new(move |prompt: Prompt| {
        asked_too.lock().unwrap().push(prompt);
        Some(if TRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 { "wrong".into() } else { PASSPHRASE.into() })
    });
    assert!(boxlink::sign_in(HOST, Some(retyping)).unwrap(), "this sign-in made the master");
    assert_eq!(asked.lock().unwrap().len(), 2, "the host key was accepted before; the passphrase twice");
    assert_eq!(boxlink::status(), None);
    asked.lock().unwrap().clear();

    // sessions ride the master: many at once, no prompt, no second master
    let workers: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                boxlink::ensure(HOST).unwrap();
                let output = session(&format!("echo rode-{i}"));
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), format!("rode-{i}"), "{}", String::from_utf8_lossy(&output.stderr));
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(asked.lock().unwrap().is_empty());
    let sockets = || {
        std::fs::read_dir(sill_proto::runtime_dir().unwrap().join("ssh"))
            .unwrap()
            .flatten()
            .filter(|e| {
                use std::os::unix::fs::FileTypeExt;
                e.file_type().is_ok_and(|t| t.is_socket())
            })
            .count()
    };
    assert_eq!(sockets(), 1, "one control socket for the box");

    // a second sign-in while connected asks nothing, and says the master was not its own (a
    // failed Add in the window must leave it to whoever made it)
    assert!(!boxlink::sign_in(HOST, Some(answering(&[]))).unwrap());
    assert!(asked.lock().unwrap().is_empty());

    // the master goes (a reboot, a sleep): the key needs its passphrase, so the box waits for
    // the person instead of retrying
    boxlink::disconnect(HOST).unwrap();
    assert_eq!(sockets(), 0);
    assert_eq!(problem(&boxlink::ensure(HOST).unwrap_err()), Problem::SignIn);
    assert_eq!(boxlink::status().unwrap().retry_in, None);
    boxlink::sign_in(HOST, Some(answering(&[("passphrase", Some(PASSPHRASE))]))).unwrap();
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert_eq!(String::from_utf8_lossy(&session("echo back").stdout).trim(), "back");

    // the server is gone: offline, with a retry planned, and no attempt before it is due
    boxlink::disconnect(HOST).unwrap();
    stop_server();
    let error = boxlink::sign_in(HOST, Some(answering(&[]))).unwrap_err();
    assert_eq!(problem(&error), Problem::Offline, "{error:#}");
    let wait = boxlink::status().unwrap().retry_in.expect("a retry is planned");
    assert!(wait > Duration::from_secs(1) && wait < Duration::from_secs(3), "{wait:?}");
    assert!(start_server());
    assert_eq!(problem(&boxlink::ensure(HOST).unwrap_err()), Problem::Offline, "not due yet: no attempt, though the server is back");
    // Retry now: the attempt is made, reaches the server and finds the key locked
    boxlink::retry_now(HOST);
    assert_eq!(problem(&boxlink::ensure(HOST).unwrap_err()), Problem::SignIn);
}
