//! Headless probe of `acp::run_session` against a real agent: `cargo run -p sill-core --example
//! acp_probe -- claude-agent-acp`. On Ready it sends what the daemon's restore of remembered controls
//! would (model, mode, effort) and prints every Controls event, so the drive loop can be watched
//! without a frontend.
//! With `ACP_PROBE_PROMPT=hi` it sends one prompt instead — against `fake-agent.py` next to this
//! file, which answers with a usage update and an update kind sill doesn't know, both of which the
//! loop must survive.
use std::os::unix::net::UnixStream;
use std::process::{Command as Proc, Stdio};

use sill_core::acp::{AutoPermission, Command, ConfigKind, ConfigValue, Controls, Event, SessionOptions};

fn show(tag: &str, c: &Controls) {
    let mode = c.modes.as_ref().map(|(cur, _)| cur.as_str()).unwrap_or("-");
    let opts: Vec<String> = c
        .options
        .iter()
        .map(|o| match &o.kind {
            ConfigKind::Select(cur, _) => format!("{}[{}]={cur}", o.id, o.category),
            ConfigKind::Boolean(b) => format!("{}[{}]={b}", o.id, o.category),
        })
        .collect();
    println!("{tag}: mode={mode} {}", opts.join(" "));
}

fn main() {
    println!("probe start");
    let agent = std::env::args().nth(1).unwrap_or_else(|| "claude-agent-acp".into());
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut child = Proc::new(&agent)
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs.try_clone().unwrap())))
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn agent");
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    let t = std::thread::spawn(move || {
        let _ = sill_core::acp::run_session(ours, SessionOptions { cwd: home, roots: vec![], auto: AutoPermission::Ask, resume: None }, cmd_rx, ev_tx);
    });
    let started = std::time::Instant::now();
    while let Ok(ev) = ev_rx.recv_blocking() {
        let ms = started.elapsed().as_millis();
        match &ev {
            Event::Ready { controls, resumed, .. } => {
                show(&format!("{ms:>5} READY(resumed={resumed})"), controls);
                // ACP_PROBE_PROMPT=text: one prompt turn instead of the settings (for a fake agent that
                // emits updates the drive loop must survive)
                let cmds = match std::env::var("ACP_PROBE_PROMPT") {
                    Ok(text) => vec![Command::Prompt(text, vec![])],
                    Err(_) => vec![
                        Command::SetConfig("model".into(), ConfigValue::Id("sonnet".into())),
                        Command::SetMode("auto".into()),
                        Command::SetConfig("effort".into(), ConfigValue::Id("high".into())),
                    ],
                };
                for c in cmds {
                    println!("{ms:>5} send {c:?}");
                    cmd_tx.try_send(c).unwrap();
                }
                let tx = cmd_tx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(6));
                    let _ = tx.send_blocking(Command::Quit);
                });
            }
            Event::Controls(c) => show(&format!("{ms:>5} CONTROLS"), c),
            Event::Usage { used, size } => println!("{ms:>5} usage {used} / {size}"),
            Event::Elicitation { message, fields, .. } => {
                println!("{ms:>5} form: {message} — {} field(s): {:?}", fields.len(), fields.iter().map(|f| f.key.as_str()).collect::<Vec<_>>());
                // answered with the first choice, like a person clicking the first option
                let Event::Elicitation { fields, reply, .. } = ev else { unreachable!() };
                let mut values = std::collections::BTreeMap::new();
                for f in fields {
                    if let sill_core::acp::FieldKind::Choice(o) = f.kind
                        && let Some((v, _, _)) = o.first()
                    {
                        values.insert(f.key, serde_json::Value::String(v.clone()));
                    }
                }
                let _ = reply.send(Some(values));
                continue;
            }
            Event::Other(s) => println!("{ms:>5} other: {s}"),
            Event::Ended(e) => {
                println!("{ms:>5} ended: {e:?}");
                break;
            }
            e => println!("{ms:>5} {}", format!("{e:?}").chars().take(80).collect::<String>()),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = t.join();
}
