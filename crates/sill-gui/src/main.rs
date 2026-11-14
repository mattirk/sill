mod backend;
mod demo;
mod fonts;
mod icons;
mod model;
#[cfg(test)]
mod snapshots;
mod style;
mod terminal;
#[cfg(test)]
mod tests;
mod ui;
mod viewer;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::Parser;
use iced::widget::{markdown, text_editor};
use iced::{Font, Subscription, Task};
use serde_json::json;
use sill_proto::sfp::{
    Answer, AnswerQuestion, CancelTurn, ConfigValue, Control, SessionControl, SessionEvent,
    SubmissionKey, Submit,
};

use backend::{Command, Update};
use model::{Conversation, item_text};

#[derive(Parser)]
#[command(about = "Experimental Iced conversation frontend for Sill (runs beside the shelf)")]
struct Options {
    /// Attach to an existing session by exact ID, unique prefix or title.
    #[arg(long, conflicts_with = "demo")]
    session: Option<String>,
    /// Show a synthetic streaming conversation without connecting to a daemon.
    #[arg(long)]
    demo: bool,
}

fn main() -> iced::Result {
    let options = Options::parse();
    fonts::install();
    iced::application(
        move || App::new(options.demo, options.session.clone()),
        App::update,
        App::view,
    )
    .title("Sill · Iced preview")
    .theme(|_: &App| style::theme())
    .subscription(App::subscription)
    .default_font(Font::with_name(fonts::SANS_FAMILY))
    .window_size((720.0, 860.0))
    // no title bar, like the shelf: the top bar's empty space moves the window, Ctrl+Q quits
    .decorations(false)
    .run()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionChoice {
    id: String,
    title: String,
    agent: String,
}

impl std::fmt::Display for SessionChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.title)
    }
}

/// One entry of an ACP control's dropdown: a mode, an option's choice, or On/Off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    value: String,
    title: String,
    boolean: Option<bool>,
}

impl std::fmt::Display for Pick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.title)
    }
}

#[derive(Debug, Clone)]
enum Message {
    Backend(u64, Update),
    Choose(SessionChoice),
    Refresh,
    Copy(String),
    Link(String),
    ToggleText,
    RefreshText,
    TextAction(text_editor::Action),
    DemoTick,
    Tick,
    Scrolled(bool),
    Latest,
    Prompt(text_editor::Action),
    Send,
    Stop,
    Answer {
        question_id: String,
        option_id: String,
    },
    Unqueue(String),
    ToggleTool(String),
    ToggleAnswered(String),
    SetMode(Pick),
    SetOption(String, Pick),
    /// A press on the top bar's empty space: the window manager moves the window.
    DragWindow,
    Quit,
    /// `>_`: the session's jail shell in place of the prompt, or the prompt again.
    ToggleTerminal,
    Terminal(u64, terminal::Event),
    TermAction(terminal::Action),
    TermPaste(Option<String>),
    /// End the shell itself (hiding the pane only detaches from it).
    EndTerminal,
    ReopenTerminal,
}

struct Rendered {
    source: String,
    markdown: markdown::Content,
}

struct App {
    commands: Option<mpsc::Sender<Command>>,
    choices: Vec<SessionChoice>,
    selected: Option<SessionChoice>,
    conversation: Conversation,
    rendered: BTreeMap<String, Rendered>,
    status: String,
    error: Option<String>,
    /// A request the person made that the daemon refused; shown until the next one.
    failed: Option<String>,
    text_view: Option<text_editor::Content>,
    prompt: text_editor::Content,
    /// Tool rows opened to show their output and diff.
    open_tools: BTreeSet<String>,
    /// Answered questions opened to show what was asked.
    open_answers: BTreeSet<String>,
    demo: bool,
    demo_step: usize,
    loading: bool,
    generation: u64,
    follow_tail: bool,
    /// Set by each tick while the agent is busy; the spinner's angle comes from it.
    now: Instant,
    /// The session's jail shell; kept attached while hidden, dropped (detached) on switching.
    terminal: Option<terminal::Pane>,
    terminal_shown: bool,
    terminal_generation: u64,
}

/// A fresh identity for a submission or an answer (1–128 bytes, unique for this daemon).
fn request_id(kind: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "gui-{kind}-{}-{nanos:x}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

impl App {
    fn new(demo: bool, initial: Option<String>) -> (Self, Task<Message>) {
        let mut app = Self {
            commands: None,
            choices: Vec::new(),
            selected: None,
            conversation: Conversation::default(),
            rendered: BTreeMap::new(),
            status: "Connecting…".into(),
            error: None,
            failed: None,
            text_view: None,
            prompt: text_editor::Content::new(),
            open_tools: BTreeSet::new(),
            open_answers: BTreeSet::new(),
            demo,
            demo_step: 0,
            loading: false,
            generation: 0,
            follow_tail: true,
            now: Instant::now(),
            terminal: None,
            terminal_shown: false,
            terminal_generation: 0,
        };
        if demo {
            app.conversation = demo::conversation();
            let choice = SessionChoice {
                id: "demo".into(),
                title: "witty-willow".into(),
                agent: "claude".into(),
            };
            app.choices = vec![choice.clone()];
            app.selected = Some(choice);
            app.status = "Demo".into();
            app.sync_rendered();
            return (app, Task::none());
        }
        let (commands, events) = backend::start(initial);
        app.commands = Some(commands);
        let stream = iced::futures::stream::unfold(events, |events| async move {
            events.recv().await.ok().map(|event| (event, events))
        });
        (
            app,
            Task::run(stream, |(generation, update)| {
                Message::Backend(generation, update)
            }),
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![];
        if self.demo && self.demo_step <= demo::CHUNKS.len() {
            subscriptions
                .push(iced::time::every(Duration::from_millis(600)).map(|_| Message::DemoTick));
        }
        // the spinner and the timer move only while the agent is busy
        if self.busy() {
            subscriptions.push(iced::time::every(Duration::from_millis(80)).map(|_| Message::Tick));
        }
        // sessions started or ended elsewhere appear and go
        if !self.demo {
            subscriptions
                .push(iced::time::every(Duration::from_secs(15)).map(|_| Message::Refresh));
        }
        subscriptions.push(iced::keyboard::listen().filter_map(quit_key));
        Subscription::batch(subscriptions)
    }

    /// The agent is working or waiting on a question.
    fn busy(&self) -> bool {
        self.conversation
            .session
            .as_ref()
            .is_some_and(|s| s.activity == "working" || s.activity == "waiting")
    }

    fn command(&mut self, command: Command) {
        if self
            .commands
            .as_ref()
            .is_some_and(|tx| tx.send(command).is_err())
        {
            self.error = Some("Connection worker stopped. Reopen the preview.".into());
        }
    }

    fn call(&mut self, method: &'static str, params: serde_json::Value, what: &'static str) {
        self.failed = None;
        self.command(Command::Call {
            method,
            params,
            what,
        });
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if let Message::Backend(generation, _) = &message
            && *generation != self.generation
        {
            return Task::none();
        }
        let scroll_to_tail = self.follow_tail
            && matches!(
                &message,
                Message::Backend(_, Update::Snapshot(_) | Update::Event(_))
                    | Message::DemoTick
                    | Message::Send
                    | Message::Answer { .. }
            );
        match message {
            Message::Backend(_, Update::Sessions(sessions)) => {
                self.choices = sessions
                    .into_iter()
                    .filter(|s| s.ended_at.is_none())
                    .map(|s| SessionChoice {
                        id: s.session_id,
                        title: s.title,
                        agent: s.agent,
                    })
                    .collect();
                if let Some(selected) = &self.selected
                    && let Some(fresh) = self.choices.iter().find(|c| c.id == selected.id)
                {
                    self.selected = Some(fresh.clone());
                }
            }
            Message::Backend(_, Update::Snapshot(snapshot)) => {
                self.selected = self
                    .choices
                    .iter()
                    .find(|s| s.id == snapshot.session_id)
                    .cloned();
                self.conversation = *snapshot;
                self.rendered.clear();
                self.text_view = None;
                self.loading = false;
                self.error = None;
                self.sync_rendered();
            }
            Message::Backend(_, Update::Event(event)) => self.accept(*event),
            Message::Backend(_, Update::Status(status)) => {
                if status == "Connected" {
                    self.loading = false;
                    self.error = None;
                }
                self.status = status;
            }
            Message::Backend(_, Update::Error(error)) => {
                self.status = "Disconnected · retrying".into();
                self.error = Some(error);
                self.loading = false;
            }
            Message::Backend(_, Update::Failed(failure)) => self.failed = Some(failure),
            Message::Choose(choice) => {
                if self.selected.as_ref().is_some_and(|s| s.id == choice.id)
                    && !self.conversation.session_id.is_empty()
                {
                    return Task::none();
                }
                self.generation += 1;
                self.command(Command::Attach {
                    id: choice.id.clone(),
                    generation: self.generation,
                });
                self.selected = Some(choice);
                self.loading = true;
                self.follow_tail = true;
                self.conversation = Conversation::default();
                self.rendered.clear();
                self.text_view = None;
                self.open_tools.clear();
                self.open_answers.clear();
                self.terminal = None;
                self.terminal_shown = false;
                self.status = "Loading conversation…".into();
                self.error = None;
                self.failed = None;
            }
            Message::Refresh => self.command(Command::Refresh),
            Message::Scrolled(at_bottom) => self.follow_tail = at_bottom,
            Message::Latest => {
                self.follow_tail = true;
                return iced::widget::operation::snap_to_end("conversation");
            }
            Message::Copy(value) => return iced::clipboard::write(value),
            Message::Link(url) => {
                self.status = "Link copied".into();
                return iced::clipboard::write(url);
            }
            Message::ToggleText => {
                self.text_view = if self.text_view.is_some() {
                    None
                } else {
                    Some(text_editor::Content::with_text(
                        &self.conversation.transcript(),
                    ))
                };
            }
            Message::RefreshText => {
                self.text_view = Some(text_editor::Content::with_text(
                    &self.conversation.transcript(),
                ));
            }
            Message::TextAction(action) => {
                if !matches!(action, text_editor::Action::Edit(_))
                    && let Some(content) = &mut self.text_view
                {
                    content.perform(action);
                }
            }
            Message::DemoTick => {
                if let Some(chunk) = demo::CHUNKS.get(self.demo_step) {
                    self.accept(demo::append(self.conversation.seq + 1, chunk));
                } else if self.demo_step == demo::CHUNKS.len() {
                    for event in demo::waiting(self.conversation.seq + 1) {
                        self.accept(event);
                    }
                }
                self.demo_step += 1;
            }
            Message::Tick => self.now = Instant::now(),
            Message::Prompt(action) => self.prompt.perform(action),
            Message::Send => self.send(),
            Message::Stop => {
                if let Some(turn_id) = self
                    .conversation
                    .session
                    .as_ref()
                    .and_then(|s| s.turn_id.clone())
                    && !self.demo
                {
                    let params = CancelTurn {
                        session_id: self.conversation.session_id.clone(),
                        turn_id,
                    };
                    self.call("_sill/sessions.cancel", json!(params), "Stopping the turn");
                }
            }
            Message::Answer {
                question_id,
                option_id,
            } => {
                if self.demo {
                    for event in demo::answered(self.conversation.seq + 1, &option_id) {
                        self.accept(event);
                    }
                } else {
                    let params = AnswerQuestion {
                        session_id: self.conversation.session_id.clone(),
                        question_id,
                        answer_id: request_id("answer"),
                        answer: Answer::Permission { option_id },
                    };
                    self.call(
                        "_sill/questions.answer",
                        json!(params),
                        "Answering the question",
                    );
                }
            }
            Message::Unqueue(submission_id) => {
                let params = SubmissionKey {
                    session_id: self.conversation.session_id.clone(),
                    submission_id,
                };
                self.call(
                    "_sill/submissions.cancel",
                    json!(params),
                    "Removing the queued prompt",
                );
            }
            Message::ToggleTool(id) => toggle(&mut self.open_tools, id),
            Message::ToggleAnswered(id) => toggle(&mut self.open_answers, id),
            Message::SetMode(pick) => self.control(Control::SetMode {
                mode_id: pick.value,
            }),
            Message::SetOption(option_id, pick) => {
                let value = match pick.boolean {
                    Some(value) => ConfigValue::Bool { value },
                    None => ConfigValue::Id { value: pick.value },
                };
                self.control(Control::SetConfig { option_id, value });
            }
            Message::DragWindow => return iced::window::latest().and_then(iced::window::drag),
            Message::Quit => return iced::exit(),
            Message::ToggleTerminal => {
                if self.terminal_shown {
                    self.terminal_shown = false;
                    return iced::widget::operation::focus("prompt");
                }
                self.terminal_shown = true;
                let reusable = self.terminal.as_ref().is_some_and(|t| {
                    matches!(
                        t.status,
                        terminal::Status::Opening | terminal::Status::Running
                    )
                });
                if reusable {
                    if let Some(pane) = &mut self.terminal {
                        pane.focused = true;
                    }
                    return Task::none();
                }
                return self.open_terminal();
            }
            Message::ReopenTerminal => return self.open_terminal(),
            Message::EndTerminal => {
                if let Some(pane) = &self.terminal {
                    pane.close();
                }
            }
            Message::Terminal(generation, event) => {
                if generation == self.terminal_generation
                    && let Some(pane) = &mut self.terminal
                {
                    pane.accept(event);
                }
            }
            Message::TermAction(action) => {
                let Some(pane) = &mut self.terminal else {
                    return Task::none();
                };
                match action {
                    terminal::Action::Copy => {
                        if let Some(selected) = pane.selection() {
                            return iced::clipboard::write(selected);
                        }
                    }
                    terminal::Action::Paste => {
                        return iced::clipboard::read().map(Message::TermPaste);
                    }
                    action => pane.perform(action),
                }
            }
            Message::TermPaste(text) => {
                if let (Some(pane), Some(text)) = (&mut self.terminal, text) {
                    pane.paste(&text);
                }
            }
        }
        if scroll_to_tail && self.text_view.is_none() {
            iced::widget::operation::snap_to_end("conversation")
        } else {
            Task::none()
        }
    }

    /// A pane on the session's jail shell (the pretend one in `--demo`); its events come tagged
    /// so a replaced pane's late ones are dropped.
    fn open_terminal(&mut self) -> Task<Message> {
        if self.conversation.session_id.is_empty() {
            return Task::none();
        }
        self.terminal_generation += 1;
        let generation = self.terminal_generation;
        let (cols, rows) = self
            .terminal
            .as_ref()
            .map_or((80, 16), |t| (t.cols, t.rows));
        let (pane, events) = if self.demo {
            terminal::Pane::open(cols, rows, terminal::demo_shell)
        } else {
            terminal::Pane::open(
                cols,
                rows,
                terminal::jail_shell(self.conversation.session_id.clone()),
            )
        };
        self.terminal = Some(pane);
        let stream = iced::futures::stream::unfold(events, |events| async move {
            events.recv().await.ok().map(|event| (event, events))
        });
        Task::run(stream, move |event| Message::Terminal(generation, event))
    }

    /// The prompt goes to the agent (the daemon queues it while a turn runs); the composer clears.
    fn send(&mut self) {
        let text = self.prompt.text().trim_end().to_string();
        if text.trim().is_empty() || self.conversation.session_id.is_empty() {
            return;
        }
        self.prompt = text_editor::Content::new();
        self.follow_tail = true;
        if self.demo {
            for event in demo::prompt(self.conversation.seq + 1, &text) {
                self.accept(event);
            }
            return;
        }
        let params = Submit {
            session_id: self.conversation.session_id.clone(),
            submission_id: request_id("prompt"),
            text,
            attachments: vec![],
        };
        self.call("_sill/sessions.submit", json!(params), "Sending the prompt");
    }

    fn control(&mut self, control: Control) {
        if self.demo {
            return;
        }
        let params = SessionControl {
            session_id: self.conversation.session_id.clone(),
            control,
        };
        self.call(
            "_sill/sessions.control",
            json!(params),
            "Changing the agent's setting",
        );
    }

    fn reconnect(&mut self) {
        self.generation += 1;
        self.command(Command::Reconnect(self.generation));
        self.status = "Restoring conversation…".into();
    }

    fn accept(&mut self, event: SessionEvent) {
        match self.conversation.accept(event) {
            Ok(true) => self.sync_rendered(),
            Ok(false) => {}
            Err(error) => {
                self.error = Some(format!("{error:#}. Restoring the conversation."));
                self.reconnect();
            }
        }
    }

    fn sync_rendered(&mut self) {
        self.rendered
            .retain(|id, _| self.conversation.items.contains_key(id));
        for item in self.conversation.items.values() {
            if item.kind == "tool" {
                continue; // a tool row is drawn from its fields, not as Markdown
            }
            let source = item_text(item);
            match self.rendered.get_mut(&item.item_id) {
                Some(cached) if cached.source == source => {}
                Some(cached) if source.starts_with(&cached.source) => {
                    cached.markdown.push_str(&source[cached.source.len()..]);
                    cached.source = source;
                }
                _ => {
                    self.rendered.insert(
                        item.item_id.clone(),
                        Rendered {
                            markdown: markdown::Content::parse(&source),
                            source,
                        },
                    );
                }
            }
        }
    }
}

/// Ctrl+Q (Cmd+Q on a Mac) quits: with no title bar there is no close button.
fn quit_key(event: iced::keyboard::Event) -> Option<Message> {
    match event {
        iced::keyboard::Event::KeyPressed { key, modifiers, .. }
            if modifiers.command()
                && matches!(key.as_ref(), iced::keyboard::Key::Character("q")) =>
        {
            Some(Message::Quit)
        }
        _ => None,
    }
}

fn toggle(set: &mut BTreeSet<String>, id: String) {
    if !set.remove(&id) {
        set.insert(id);
    }
}
