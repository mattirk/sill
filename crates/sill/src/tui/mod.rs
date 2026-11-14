//! Line mode (docs/daemon.md decision 11, docs/protocol.md): `sill attach <session>` — the
//! protocol's second consumer. A prompt per line in, the agent's text and tool calls out, a
//! permission request answered on the next line. Nothing here reads the store or a jail: it is a
//! `sill_proto::Client` and a terminal, the same as a frontend in another language would be.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use sill_proto::wire::{Answer, Command, Entry, Event, FieldKind, PermissionOptionKind};
use sill_proto::{Attach, Client, Key, Session, SessionAnswer, SessionCommand, SessionEvent, method};

/// Whether `> ` is on the screen waiting for a line: a note that arrives then (the daemon's
/// "N changes pending" after a turn) takes the line over and the mark is shown again after it.
static AT_PROMPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A question the agent asked, waiting for the next line.
struct Pending {
    request: u64,
    /// Permission options by number; empty for a form (not answered here, see below).
    options: Vec<(String, PermissionOptionKind)>,
}

pub fn line_mode(key: &str, fresh: bool, verbose: bool) -> Result<()> {
    let c = Arc::new(Client::connect_or_start()?);
    let s: Session = c.call(method::SESSIONS_GET, Key { key: key.to_string() })?;
    let id = s.id.clone();
    eprintln!("sill: {} {} — {} ({}); a line is a prompt, /cancel stops the turn, /quit or Ctrl-D detaches (the session keeps running)", &id[..8.min(id.len())], s.agent, s.title, if s.running { "running" } else { "starting" });
    let pending: Arc<Mutex<Option<Pending>>> = Arc::default();
    {
        // the daemon's notifications, rendered as they come; registered before the attach so the
        // session's current state (the turn in flight, an unanswered question) is not missed
        let c = c.clone();
        let id = id.clone();
        let pending = pending.clone();
        std::thread::Builder::new().name("sill-attach-events".into()).spawn(move || {
            let rx = c.notifications.lock().unwrap();
            while let Ok((m, params)) = rx.recv() {
                if m != method::SESSION_EVENT {
                    continue;
                }
                let Ok(n) = serde_json::from_value::<SessionEvent>(params) else { continue };
                if n.key != id {
                    continue;
                }
                if !render(n.event, verbose, &pending) {
                    std::process::exit(0);
                }
            }
            eprintln!("\nsill: the daemon closed the connection");
            std::process::exit(1);
        })?;
    }
    c.call::<serde_json::Value>(method::SESSIONS_ATTACH, Attach { key: id.clone(), fresh }).context("attaching")?;
    prompt_mark();
    let command = |cmd: Command| -> Result<()> {
        c.call::<serde_json::Value>(method::SESSIONS_COMMAND, SessionCommand { key: id.clone(), command: cmd })?;
        Ok(())
    };
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let text = line.trim();
        // a question first: this line answers it
        if let Some(q) = pending.lock().unwrap().take() {
            let answer = match text {
                "" => None,
                "y" | "yes" => q.options.iter().find(|(_, k)| matches!(k, PermissionOptionKind::AllowOnce)).or(q.options.iter().find(|(_, k)| matches!(k, PermissionOptionKind::AllowAlways))).map(|(i, _)| i.clone()),
                "n" | "no" => q.options.iter().find(|(_, k)| matches!(k, PermissionOptionKind::RejectOnce)).or(q.options.iter().find(|(_, k)| matches!(k, PermissionOptionKind::RejectAlways))).map(|(i, _)| i.clone()),
                n => n.parse::<usize>().ok().and_then(|n| q.options.get(n.wrapping_sub(1))).map(|(i, _)| i.clone()),
            };
            if answer.is_none() && !text.is_empty() {
                eprintln!("sill: not an option; the request is cancelled");
            }
            let r = c.call::<serde_json::Value>(method::SESSIONS_ANSWER, SessionAnswer { key: id.clone(), request: q.request, answer: Answer::Permission(answer) });
            if let Err(e) = r {
                eprintln!("sill: {e:#}");
            }
            continue;
        }
        match text {
            "" => prompt_mark(),
            "/quit" | "/q" => break,
            "/cancel" => {
                if let Err(e) = command(Command::Cancel) {
                    eprintln!("sill: {e:#}");
                }
            }
            _ => {
                let cmd = Command::Prompt { text: text.to_string(), attachments: vec![], entry: Some(Entry::User { text: text.to_string(), names: vec![] }) };
                if let Err(e) = command(cmd) {
                    eprintln!("sill: {e:#}");
                    prompt_mark();
                }
            }
        }
    }
    let _ = command(Command::Quit);
    eprintln!("sill: detached; the session keeps running");
    Ok(())
}

fn prompt_mark() {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"> ");
    let _ = out.flush();
    AT_PROMPT.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Before a line of output: the prompt mark, when shown, is erased so the line starts clean.
fn take_line(out: &mut impl Write) -> bool {
    let was = AT_PROMPT.swap(false, std::sync::atomic::Ordering::Relaxed);
    if was {
        let _ = out.write_all(b"\r\x1b[K");
    }
    was
}

/// One event to the terminal; false when the session is over and the process should leave.
fn render(ev: Event, verbose: bool, pending: &Mutex<Option<Pending>>) -> bool {
    let mut out = std::io::stdout().lock();
    // a turn's output means the person's line was taken: the mark is gone, not erased
    if !matches!(ev, Event::Note(_) | Event::Resolved { .. } | Event::SessionEnded { .. } | Event::Summarized { .. }) {
        AT_PROMPT.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    match ev {
        Event::Text(t) => {
            let _ = out.write_all(t.as_bytes());
        }
        Event::Thought(t) if verbose => {
            let _ = write!(out, "\x1b[2m{t}\x1b[0m");
        }
        Event::UserText(t) => {
            let _ = writeln!(out, "\x1b[2m> {t}\x1b[0m");
        }
        Event::ToolCall { title, status, .. } => {
            let _ = writeln!(out, "\n\x1b[36m⚙ {title} [{status}]\x1b[0m");
        }
        Event::ToolCallUpdate { id, status, title, .. } if verbose => {
            let _ = writeln!(out, "\x1b[36m  ↳ {id} {status:?} {title:?}\x1b[0m");
        }
        Event::SubagentText { text, .. } if verbose => {
            let _ = write!(out, "\x1b[2m{text}\x1b[0m");
        }
        Event::PermissionRequest { request, title, options, .. } => {
            let _ = writeln!(out, "\n\x1b[33m? {title}\x1b[0m");
            for (i, o) in options.iter().enumerate() {
                let _ = writeln!(out, "  {}. {} ({})", i + 1, o.name, o.id);
            }
            let _ = write!(out, "\x1b[33m  number, y (allow once) or n (reject once); Enter cancels\x1b[0m\n? ");
            *pending.lock().unwrap() = Some(Pending { request, options: options.into_iter().map(|o| (o.id, o.kind)).collect() });
        }
        Event::Elicitation { message, fields, .. } => {
            // forms are not answered in line mode: shown, left waiting for a frontend that can
            let _ = writeln!(out, "\n\x1b[33m? {message}\x1b[0m");
            for f in &fields {
                let kind = match &f.kind {
                    FieldKind::Choice(o) | FieldKind::Choices(o) => o.iter().map(|(v, _, _)| v.as_str()).collect::<Vec<_>>().join(" | "),
                    FieldKind::Text => "text".into(),
                    FieldKind::Bool => "yes/no".into(),
                    FieldKind::Number => "number".into(),
                };
                let _ = writeln!(out, "  {}: {kind}", f.title);
            }
            let _ = writeln!(out, "\x1b[33m  (a form: answer it in the shelf; it waits)\x1b[0m");
        }
        Event::Resolved { request } => {
            let mut p = pending.lock().unwrap();
            if p.as_ref().is_some_and(|q| q.request == request) {
                *p = None;
                let _ = writeln!(out, "\n\x1b[2m  (answered elsewhere)\x1b[0m");
            }
        }
        Event::AutoAnswered { title, allowed, .. } => {
            let _ = writeln!(out, "\x1b[2m· {title}: {}\x1b[0m", if allowed { "allowed" } else { "rejected" });
        }
        Event::AuthRequired { message, methods } => {
            let names: Vec<String> = methods.iter().map(|m| format!("{} ({})", m.name, m.id)).collect();
            let _ = writeln!(out, "\n\x1b[33mlogin needed: {message} — methods: {}\x1b[0m", names.join(", "));
        }
        Event::Stopped(r) => {
            let _ = writeln!(out, "\n\x1b[2m[{r:?}]\x1b[0m");
            let _ = out.write_all(b"> ");
            AT_PROMPT.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Event::Ended(Some(e)) => {
            let _ = writeln!(out, "\x1b[31m{e}\x1b[0m");
        }
        Event::Note(n) => {
            let again = take_line(&mut out);
            let _ = writeln!(out, "\x1b[2m· {n}\x1b[0m");
            if again {
                let _ = out.write_all(b"> ");
                AT_PROMPT.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Event::Summarized { text, title } => {
            let again = take_line(&mut out);
            let _ = writeln!(out, "\x1b[2m· summary{}: {}\x1b[0m", title.map(|t| format!(" ({t})")).unwrap_or_default(), text.unwrap_or_default());
            if again {
                let _ = out.write_all(b"> ");
                AT_PROMPT.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Event::SessionEnded { deleted, reason } => {
            let _ = writeln!(out, "\n\x1b[2msession ended ({reason}){}\x1b[0m", if deleted { ", deleted" } else { "" });
            let _ = out.flush();
            return false;
        }
        Event::Other(s) if verbose => {
            let _ = writeln!(out, "\x1b[2m· {s}\x1b[0m");
        }
        Event::Usage { used, size } if verbose => {
            let _ = writeln!(out, "\x1b[2m· context {used}/{size}\x1b[0m");
        }
        Event::Ready { .. } | Event::Capabilities { .. } | Event::Controls(_) | Event::TurnStarted | Event::Ended(None) => {}
        _ => {}
    }
    let _ = out.flush();
    true
}
