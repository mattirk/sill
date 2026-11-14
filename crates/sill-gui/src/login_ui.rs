//! The login card: when the session says its
//! agent needs a login (`SessionState::authentication`), a card at the end of the conversation
//! with one button per method the agent offers. A method that takes an API key opens a field for
//! it first. Picking a method runs it the way `sill_client::login` routes it: on the daemon's
//! host (its progress in the card, then the session starts again), in a terminal of the jail (the
//! pane opens with the agent's login command; when it exits the session is told to go on), or as
//! the agent's own `authenticate`. Once the agent no longer asks, the card says it is logged in.
use std::collections::BTreeMap;

use iced::widget::{Column, button, container, operation, row, text, text_input, tooltip};
use iced::{Alignment, Element, Fill, Task};
use serde_json::json;
use sill_client::login::{self, Route};
use sill_proto::sfp::{AuthMethod, Control, SessionControl};

use crate::style::{self, P, alpha};
use crate::{Act, App, Message, SessionView, keys, terminal};

/// One session's login under way, if any.
#[derive(Default)]
pub struct Login {
    /// The method waiting for its API key.
    keyed: Option<String>,
    key: String,
    /// The method picked: the card says so and offers nothing else until the agent answers.
    chosen: Option<String>,
    /// What the login reported, oldest first; `true` for a failure.
    notes: Vec<(String, bool)>,
    /// The terminal pane (by its generation) that runs the login command.
    pub terminal: Option<u64>,
}

#[derive(Clone)]
pub enum Event {
    Pick(String),
    Key(String),
    SubmitKey,
    /// Where the picked method runs: the agent's id, the route and whether the session's jail
    /// was started again first (its policy had changed), or why it is not known.
    Routed(String, Result<(String, Route, bool), String>),
    /// A line of the host login's progress.
    Line(String),
    HostDone(Result<(), String>),
}

// by hand: a key being typed never reaches a trace or a log
impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Pick(m) => write!(f, "Pick({m:?})"),
            Event::Key(_) => write!(f, "Key(…)"),
            Event::SubmitKey => write!(f, "SubmitKey"),
            Event::Routed(m, r) => write!(f, "Routed({m:?}, {r:?})"),
            Event::Line(l) => write!(f, "Line({l:?})"),
            Event::HostDone(r) => write!(f, "HostDone({r:?})"),
        }
    }
}

fn methods(view: &SessionView) -> Vec<AuthMethod> {
    view.conversation
        .session
        .as_ref()
        .and_then(|s| s.authentication.as_ref())
        .map(|a| a.methods.clone())
        .unwrap_or_default()
}

impl App {
    pub fn login_event(&mut self, session: String, event: Event) -> Task<Message> {
        let demo = self.demo;
        let actions = self.actions.clone();
        let Some(view) = self.views.get_mut(&session) else {
            return Task::none();
        };
        let login = &mut view.login;
        match event {
            Event::Pick(method_id) => {
                let Some(method) = methods(view).into_iter().find(|m| m.method_id == method_id)
                else {
                    return Task::none();
                };
                let login = &mut view.login;
                login.notes.clear();
                if method.api_key.is_some() {
                    login.keyed = Some(method_id);
                    login.key.clear();
                    return operation::focus("login-key");
                }
                login.keyed = None;
                let Some(actions) = actions.filter(|_| !demo) else {
                    login
                        .notes
                        .push(("A login needs the daemon; the demo has none.".into(), true));
                    return Task::none();
                };
                login.chosen = Some(method.title.clone());
                let id = session.clone();
                return crate::blocking(
                    move || {
                        actions
                            .once(|client| login::prepare(client, &id))
                            .map(|(agent, restarted)| {
                                (agent.id.clone(), login::route(&agent, &method), restarted)
                            })
                            .map_err(|e| format!("{e:#}"))
                    },
                    move |routed| Message::Login(session, Event::Routed(method_id, routed)),
                );
            }
            Event::Key(key) => login.key = key,
            Event::SubmitKey => {
                let Some(method_id) = login.keyed.clone() else {
                    return Task::none();
                };
                let key = login.key.trim().to_string();
                if key.is_empty() {
                    return operation::focus("login-key");
                }
                login.key.clear();
                login.keyed = None;
                let chosen = methods(view)
                    .into_iter()
                    .find(|m| m.method_id == method_id)
                    .map(|m| m.title);
                view.login.chosen = chosen;
                return self.login_control(
                    &session,
                    Control::Authenticate {
                        method_id,
                        api_key: Some(sill_proto::wire::Secret(key)),
                    },
                );
            }
            Event::Routed(_, Err(why)) => {
                login.chosen = None;
                login.notes.push((format!("Login: {why}"), true));
            }
            Event::Routed(method_id, Ok((agent_id, route, restarted))) => {
                if restarted {
                    login.notes.push((
                        "The session started again first: its policy had changed since its jail started."
                            .into(),
                        false,
                    ));
                    // its agent comes back up with the attachment
                    self.reattach(&session);
                }
                return match route {
                    Route::Agent => self.login_control(
                        &session,
                        Control::Authenticate {
                            method_id,
                            api_key: None,
                        },
                    ),
                    Route::Terminal { argv, env } => self.login_terminal(&session, argv, env),
                    Route::Host => self.host_login(&session, agent_id, method_id),
                };
            }
            Event::Line(line) => login.notes.push((line, false)),
            Event::HostDone(Ok(())) => {
                login.notes.push((
                    "Logged in; the session starts again with the new login.".into(),
                    false,
                ));
                self.reattach(&session);
            }
            Event::HostDone(Err(why)) => {
                login.chosen = None;
                login.notes.push((format!("Login failed: {why}"), true));
                // the agent asks again, and the card offers its methods again
                return self.login_control(&session, Control::RetrySession);
            }
        }
        Task::none()
    }

    /// The login terminal's pane has ended (`ended`) or failed: the session goes on, and the
    /// agent says whether the login took.
    pub fn login_terminal_closed(
        &mut self,
        session: &str,
        generation: u64,
        failed: Option<String>,
    ) -> Task<Message> {
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        if view.login.terminal != Some(generation) {
            return Task::none();
        }
        view.login.terminal = None;
        if let Some(why) = failed {
            view.login.chosen = None;
            view.login
                .notes
                .push((format!("The login terminal failed: {why}"), true));
            return Task::none();
        }
        if view.terminal_generation == generation {
            view.terminal_shown = false;
        }
        view.login.notes.push((
            "The login terminal closed; the session goes on.".into(),
            false,
        ));
        self.login_control(session, Control::RetrySession)
    }

    fn login_control(&self, session: &str, control: Control) -> Task<Message> {
        let params = SessionControl {
            session_id: session.to_string(),
            control,
        };
        self.act(session, Act::Login, "_sill/sessions.control", json!(params))
    }

    /// The agent's login command in a terminal of the session's jail, in the terminal pane: the
    /// person finishes it there (a link to open, a code to paste).
    fn login_terminal(
        &mut self,
        session: &str,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Task<Message> {
        let Some(connect) = self.connect.clone() else {
            return Task::none();
        };
        self.next_terminal += 1;
        let generation = self.next_terminal;
        let Some(view) = self.views.get_mut(session) else {
            return Task::none();
        };
        view.select_shell(false);
        view.terminal_generation = generation;
        let (cols, rows) = view
            .terminal
            .as_ref()
            .map_or((80, 16), |t| (t.cols, t.rows));
        // the shell's pane detaches (the shell goes on); the login has the pane
        view.terminal = None;
        let (pane, events) = terminal::Pane::open(
            cols,
            rows,
            sill_client::terminal::jail_terminal(
                connect,
                session.to_string(),
                login::TERMINAL_LABEL,
                argv,
                env,
            ),
        );
        view.terminal = Some(pane);
        view.terminal_shown = true;
        view.login.terminal = Some(generation);
        view.login.notes.push((
            "Finish the login in the terminal below: Ctrl+click its link to open it, and paste the code back with Ctrl+V. The session goes on when it closes.".into(),
            false,
        ));
        let session = session.to_string();
        let stream = iced::futures::stream::unfold(events, |events| async move {
            events.recv().await.ok().map(|event| (event, events))
        });
        Task::run(stream, move |event| {
            Message::Terminal(session.clone(), generation, event)
        })
    }

    /// The method on the daemon's host, its progress into the card; then the session's jail
    /// starts again with the login it wrote.
    fn host_login(&mut self, session: &str, agent_id: String, method_id: String) -> Task<Message> {
        let Some(connect) = self.connect.clone() else {
            return Task::none();
        };
        if let Some(view) = self.views.get_mut(session) {
            view.login.notes.push((
                "The login runs on the daemon's host; follow the link it reports.".into(),
                false,
            ));
        }
        let (tx, rx) = async_channel::unbounded();
        let id = session.to_string();
        let spawned = std::thread::Builder::new()
            .name("sill-gui-login".into())
            .spawn(move || {
                let done = (|| -> anyhow::Result<()> {
                    let client = connect()?;
                    login::host_login(&client, &agent_id, &method_id, |line| {
                        let _ = tx.send_blocking(Event::Line(line));
                    })?;
                    login::restart(&client, &id)
                })();
                let _ = tx.send_blocking(Event::HostDone(done.map_err(|e| format!("{e:#}"))));
            });
        if spawned.is_err() {
            return Task::none();
        }
        let session = session.to_string();
        let stream = iced::futures::stream::unfold(rx, |rx| async move {
            rx.recv().await.ok().map(|event| (event, rx))
        });
        Task::run(stream, move |event| Message::Login(session.clone(), event))
    }

    /// The card, while the agent asks for a login or one is under way; none otherwise.
    pub fn login_card<'a>(&'a self, view: &'a SessionView) -> Option<Element<'a, Message>> {
        let login = &view.login;
        let auth = view
            .conversation
            .session
            .as_ref()
            .and_then(|s| s.authentication.as_ref());
        if auth.is_none() && login.chosen.is_none() && login.notes.is_empty() {
            return None;
        }
        let session = view.id.clone();
        let event = move |event: Event| Message::Login(session.clone(), event);
        let mut card = Column::new().spacing(8);
        match auth {
            Some(auth) => {
                let said = if auth.message.is_empty() {
                    "Login needed".to_string()
                } else {
                    format!("Login needed: {}", auth.message)
                };
                let said = match &login.chosen {
                    Some(chosen) => format!("{said} → {chosen}"),
                    None => said,
                };
                card = card.push(text(said).size(style::TEXT));
                if login.chosen.is_none() {
                    let mut methods = row![].spacing(8);
                    for method in &auth.methods {
                        let pick = keys::press(
                            button(text(&method.title).size(style::TEXT))
                                .padding([6, 12])
                                .style(style::solid(P.accent)),
                            event(Event::Pick(method.method_id.clone())),
                        );
                        methods = methods.push(match &method.description {
                            Some(about) if !about.trim().is_empty() => tooltip(
                                pick,
                                container(text(about).size(style::TINY))
                                    .padding([3, 8])
                                    .style(style::fill(P.surface)),
                                tooltip::Position::Top,
                            )
                            .into(),
                            _ => pick,
                        });
                    }
                    card = card.push(methods.wrap().vertical_spacing(8));
                }
                if let Some(keyed) = &login.keyed {
                    let provider = auth
                        .methods
                        .iter()
                        .find(|m| &m.method_id == keyed)
                        .and_then(|m| m.api_key.clone())
                        .unwrap_or_default();
                    let who = match provider.as_str() {
                        "google" => "Google",
                        "openai" => "OpenAI",
                        "anthropic" => "Anthropic",
                        other => other,
                    };
                    let placeholder = if who.is_empty() {
                        "Paste your API key".to_string()
                    } else {
                        format!("Paste your {who} API key")
                    };
                    card = card.push(
                        row![
                            text_input(&placeholder, &login.key)
                                .id("login-key")
                                .secure(true)
                                .size(style::SMALL)
                                .padding(6)
                                .on_input({
                                    let event = event.clone();
                                    move |key| event(Event::Key(key))
                                })
                                .on_submit(event(Event::SubmitKey)),
                            keys::press(
                                button(text("Use key").size(style::TEXT))
                                    .padding([6, 12])
                                    .style(style::solid(P.accent)),
                                event(Event::SubmitKey),
                            ),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    );
                    card = card.push(
                        text("sill keeps the key outside the agent's jail and gives it to the agent only to log in")
                            .size(style::TINY)
                            .color(P.fg_dim),
                    );
                }
            }
            None if login.chosen.is_some() => {
                card = card.push(text("Logged in; the agent is ready").size(style::TEXT));
            }
            None => {}
        }
        for (note, failed) in &login.notes {
            card = card.push(text(note).size(style::SMALL).color(if *failed {
                P.error
            } else {
                alpha(P.fg, 0.7)
            }));
        }
        Some(crate::ui::ruled(
            2.0,
            P.warning,
            container(card)
                .padding([8, 10])
                .width(Fill)
                .style(style::fill(alpha(P.warning, 0.10))),
        ))
    }
}

impl Login {
    /// The person goes on: a finished login's card goes.
    pub fn settle(&mut self, logged_in: bool) {
        if logged_in && self.terminal.is_none() {
            *self = Self::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{connector, daemon, settle_task};
    use serde_json::Value;
    use sill_client::actions::Actions;

    /// The demo's session on a fake daemon, asking for a login with `methods`.
    fn asking(clients: Vec<sill_proto::Client>, methods: Value) -> App {
        let (mut app, _) = App::new(true, None);
        app.demo = false;
        app.actions = Some(Actions::new(connector(clients)));
        let view = app.views.get_mut("demo").unwrap();
        view.conversation.session.as_mut().unwrap().authentication = Some(
            serde_json::from_value(json!({"message": "token expired", "methods": methods}))
                .unwrap(),
        );
        app
    }

    fn update(app: &mut App, event: Event) -> Task<Message> {
        app.update(Message::Login("demo".into(), event))
    }

    #[test]
    fn a_key_goes_to_the_agent_and_never_into_a_trace() {
        let (client, mut daemon) = daemon();
        let methods = json!([{"method_id": "key", "title": "API key", "description": null,
            "terminal": null, "api_key": "anthropic"}]);
        let mut app = asking(vec![client], methods);
        assert!(
            app.login_card(&app.views["demo"]).is_some(),
            "the card shows"
        );
        let _ = update(&mut app, Event::Pick("key".into()));
        let _ = update(&mut app, Event::Key("sk-secret".into()));
        assert_eq!(format!("{:?}", Event::Key("sk-secret".into())), "Key(…)");
        let task = update(&mut app, Event::SubmitKey);
        let request = daemon.expect("_sill/sessions.control");
        assert_eq!(
            request["params"]["control"],
            json!({"type": "authenticate", "method_id": "key", "api_key": "sk-secret"})
        );
        daemon.reply(&request, json!({}));
        settle_task(&mut app, task);
        assert!(app.views["demo"].failed.is_none());
        assert!(
            app.views["demo"].login.key.is_empty(),
            "the field forgets the key"
        );
    }

    /// A method without a terminal of a curated agent is the agent's own `authenticate`, after the
    /// session's agent is read.
    #[test]
    fn a_plain_method_authenticates_through_the_agent() {
        let (client, mut daemon) = daemon();
        let methods = json!([{"method_id": "plain", "title": "Plain", "description": "d",
            "terminal": null}]);
        let mut app = asking(vec![client], methods);
        let fake = std::thread::spawn(move || {
            let get = daemon.expect("_sill/sessions.get");
            daemon.reply(
                &get,
                json!({"session_id": "demo", "agent": "claude", "title": "t",
                "title_kind": "", "created_at": "1", "last_used": "1", "ended_at": null}),
            );
            let agent = daemon.expect("_sill/agents.get");
            assert_eq!(agent["params"]["agent_id"], "claude");
            daemon.reply(
                &agent,
                json!({"id": "claude", "name": "Claude",
                "command": ["claude-acp"], "home_paths": [{"path": "~/.claude", "kind": "directory"}]}),
            );
            let control = daemon.expect("_sill/sessions.control");
            daemon.reply(&control, json!({}));
            control["params"]["control"].clone()
        });
        let task = update(&mut app, Event::Pick("plain".into()));
        settle_task(&mut app, task);
        assert_eq!(
            fake.join().unwrap(),
            json!({"type": "authenticate", "method_id": "plain"})
        );
        assert_eq!(app.views["demo"].login.chosen.as_deref(), Some("Plain"));
    }

    /// A session whose policy changed under its running jail starts again before the login: the
    /// jail keeps the network rules it was built with (a login refused its endpoint otherwise).
    #[test]
    fn a_changed_policy_restarts_the_session_before_the_login() {
        let (client, mut daemon) = daemon();
        let methods = json!([{"method_id": "plain", "title": "Plain", "description": null,
            "terminal": null}]);
        let mut app = asking(vec![client], methods);
        let fake = std::thread::spawn(move || {
            let get = daemon.expect("_sill/sessions.get");
            daemon.reply(
                &get,
                json!({"session_id": "demo", "agent": "claude",
                "running": true, "policy_changed": true}),
            );
            let agent = daemon.expect("_sill/agents.get");
            daemon.reply(
                &agent,
                json!({"id": "claude", "name": "Claude",
                "command": ["claude-acp"], "home_paths": [{"path": "~/.claude", "kind": "directory"}]}),
            );
            let mut seen = vec![];
            for method in [
                "_sill/sessions.stop",
                "_sill/sessions.start",
                "_sill/sessions.control",
            ] {
                let request = daemon.expect(method);
                daemon.reply(&request, json!({}));
                seen.push(method);
            }
            seen
        });
        let task = update(&mut app, Event::Pick("plain".into()));
        settle_task(&mut app, task);
        assert_eq!(fake.join().unwrap().len(), 3, "stop, start, then the login");
        assert!(
            app.views["demo"]
                .login
                .notes
                .iter()
                .any(|(n, _)| n.contains("started again")),
            "the card says why"
        );
    }

    /// The login command's terminal closing lets the session go on; another pane's end does not.
    #[test]
    fn the_login_terminal_closing_retries_the_session() {
        let (client, mut daemon) = daemon();
        let mut app = asking(vec![client], json!([]));
        let view = app.views.get_mut("demo").unwrap();
        view.terminal_generation = 7;
        view.login.terminal = Some(7);
        let other = app.update(Message::Terminal("demo".into(), 6, terminal::Event::Ended));
        settle_task(&mut app, other);
        assert_eq!(app.views["demo"].login.terminal, Some(7));
        let task = app.update(Message::Terminal("demo".into(), 7, terminal::Event::Ended));
        let request = daemon.expect("_sill/sessions.control");
        assert_eq!(
            request["params"]["control"],
            json!({"type": "retry_session"})
        );
        daemon.reply(&request, json!({}));
        settle_task(&mut app, task);
        assert_eq!(app.views["demo"].login.terminal, None);
    }
}
