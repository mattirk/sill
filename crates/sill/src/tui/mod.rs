//! Line mode (docs/daemon.md decision 11, docs/protocol.md): `sill attach <session>` — the
//! protocol's second consumer. A prompt per line in, the agent's text and tool calls out, a
//! permission request answered on the next line. Nothing here reads the store or a jail: it is a
//! `sill_proto::Client` and a terminal, the same as a frontend in another language would be.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use sill_proto::wire::{Answer, Command, Event, FieldKind, PermissionOptionKind};
use sill_proto::{Attach, Client, Key, method};

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
    let s = c.resolve_session(key)?;
    let id = s.session_id;
    if fresh {
        c.call::<serde_json::Value>(method::SESSIONS_ATTACH, Attach { key:id.clone(),fresh:true })?;
        c.call::<serde_json::Value>(method::SESSIONS_DETACH, Key { key:id.clone() })?;
    }
    eprintln!("sill: {} — {}; /cancel stops the turn, /quit detaches", id, s.title);
    let pending: Arc<Mutex<Option<Pending>>> = Arc::default();
    let view = Arc::new(Mutex::new(sill_core::sfp_client::View::default()));
    for event in view.lock().unwrap().attach(&c,&id).context("attaching")? { render_public(event,verbose,&pending); }
    let current = Arc::new(Mutex::new(c));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let current=current.clone(); let view=view.clone(); let pending=pending.clone(); let done=done.clone(); let id=id.clone();
        std::thread::Builder::new().name("sill-attach-events".into()).spawn(move || {
            loop {
                let c=current.lock().unwrap().clone();
                while let Ok((method,params))=c.notifications.lock().unwrap().recv() {
                    if method!="_sill/session.event" { continue; }
                    let notification=match serde_json::from_value::<sill_proto::sfp::SessionEvent>(params) {
                        Ok(notification)=>notification,
                        Err(e)=>{ eprintln!("sill: unsupported SFP event; synchronization stopped: {e}"); std::process::exit(1); }
                    };
                    let mut state=view.lock().unwrap();
                    match state.accept(notification).or_else(|_|state.attach(&c,&id)) {
                        Ok(events)=>for event in events { render_public(event,verbose,&pending); },
                        Err(e)=>eprintln!("sill: synchronizing: {e:#}"),
                    }
                }
                if done.load(std::sync::atomic::Ordering::Acquire) { return; }
                *pending.lock().unwrap()=None;
                eprintln!("sill: connection lost; reconnecting");
                let mut delay=std::time::Duration::from_millis(100);
                loop {
                    if done.load(std::sync::atomic::Ordering::Acquire) { return; }
                    std::thread::sleep(delay);
                    let result=(||->Result<Arc<Client>> {
                        let c=Arc::new(Client::connect_or_start()?);
                        for event in view.lock().unwrap().attach(&c,&id)? { render_public(event,verbose,&pending); }
                        Ok(c)
                    })();
                    match result { Ok(c)=>{*current.lock().unwrap()=c;break;},Err(e)=>{if verbose {eprintln!("sill: reconnecting: {e:#}");}delay=(delay*2).min(std::time::Duration::from_secs(5));} }
                }
            }
        })?;
    }
    prompt_mark();
    for line in std::io::stdin().lock().lines() {
        let line=line?; let text=line.trim();
        let question = pending.lock().unwrap().take();
        if let Some(q)=question {
            let choice=match text {
                ""=>None,
                "y"|"yes"=>q.options.iter().find(|(_,k)|matches!(k,PermissionOptionKind::AllowOnce)).or(q.options.iter().find(|(_,k)|matches!(k,PermissionOptionKind::AllowAlways))).map(|(id,_)|id.clone()),
                "n"|"no"=>q.options.iter().find(|(_,k)|matches!(k,PermissionOptionKind::RejectOnce)).or(q.options.iter().find(|(_,k)|matches!(k,PermissionOptionKind::RejectAlways))).map(|(id,_)|id.clone()),
                n=>n.parse::<usize>().ok().and_then(|n|q.options.get(n.wrapping_sub(1))).map(|(id,_)|id.clone()),
            };
            let question_id=view.lock().unwrap().question_id(q.request).map(str::to_owned);
            if let Some(question_id)=question_id {
                let input=sill_core::sfp_client::answer(question_id,id.clone(),Answer::Permission(choice));
                if let Err(e)=current.lock().unwrap().call::<serde_json::Value>("_sill/questions.answer",input) {eprintln!("sill: {e:#}");}
            }
            continue;
        }
        if matches!(text,"/quit"|"/q") {break;}
        if text.is_empty() {prompt_mark();continue;}
        let command=if text=="/cancel" { Command::Cancel } else { Command::Prompt {text:text.into(),attachments:vec![],entry:None} };
        let request=sill_core::sfp_client::command_request(&id,&command,Some(&view.lock().unwrap()));
        let result=request.and_then(|(method,params)|current.lock().unwrap().call::<serde_json::Value>(method,params));
        if let Err(e)=result {eprintln!("sill: {e:#}");prompt_mark();}
    }
    done.store(true,std::sync::atomic::Ordering::Release);
    let _=view.lock().unwrap().detach(&current.lock().unwrap());
    eprintln!("sill: detached; the session keeps running");
    Ok(())
}
fn render_public(event: Event, verbose: bool, pending: &Mutex<Option<Pending>>) {
    if let Event::Other(text)=&event {
        if text.starts_with("sill:active-time:") || text.starts_with("sill:held:") || text.starts_with("sill:held-resolved:") { return; }
        if let Some(json)=text.strip_prefix("sill:replay:") {
            if let Ok(value)=serde_json::from_str(json) {render_replay(&value);}
            return;
        }
    }
    if !render(event,verbose,pending) { std::process::exit(0); }
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
        Event::UserMessage { text, attachments } => {
            let names: Vec<&str> = attachments.iter().map(|a| a.name.as_str()).collect();
            let _ = writeln!(out, "\x1b[2m> {text}\n  [{}]\x1b[0m", names.join(", "));
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


/// Stored entries precede the live snapshot on attach, including reconnect. Showing the
/// transcript explicitly makes terminal reconnection understandable without hiding missed output.
fn render_replay(replay: &serde_json::Value) {
    let Some(entries) = replay["entries"].as_array() else { return };
    if entries.is_empty() { return; }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "\n\x1b[2m[session history]\x1b[0m");
    for entry in entries {
        let text = entry["text"].as_str().unwrap_or_default();
        match entry["kind"].as_str().unwrap_or_default() {
            "user" => { let _ = writeln!(out, "> {text}"); }
            "agent" => { let _ = writeln!(out, "{text}"); }
            "note" => { let _ = writeln!(out, "\x1b[2m· {text}\x1b[0m"); }
            "tool" => { let _ = writeln!(out, "\x1b[36m⚙ {text}\x1b[0m"); }
            _ => {}
        }
    }
    let _ = writeln!(out, "\x1b[2m[live session]\x1b[0m");
    let _ = out.flush();
}
