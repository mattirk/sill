//! Host control in sill-gui (docs/permission-modes.md, Host control): the Host actions group of
//! the Permissions panel, where the person ticks the operations and activates or revokes; the
//! permanent "HOST CONTROL · actions can run outside the jail" caption over a session's
//! composer; and a window-level band naming every other session with active control, so switching to
//! a Default session cannot hide it. sill-gui declares `sill.host_control`: while a tab is
//! attached to a session, this window displays that session's control and keeps it alive.
use std::collections::BTreeSet;

use iced::widget::{column, container, row, rule, space, text};
use iced::{Alignment, Element, Fill, Task};
use sill_client::actions::Outcome;
use sill_proto::host_control::{OPERATIONS, State, describe};
use sill_proto::sessions::Session;

use crate::style::{self, P, alpha};
use crate::{App, Message, SessionView, blocking, json};

pub const CAPABILITY: &str = sill_proto::host_control::CAPABILITY;

/// The caption's fixed words; the operations follow.
pub const CAPTION: &str = "HOST CONTROL · actions can run outside the jail";

#[derive(Debug, Clone)]
pub enum Event {
    /// An operation ticked or unticked for the activation.
    Select(String, String, bool),
    Activate(String),
    Revoke(String),
    Done(String, u64, Result<State, String>),
}

/// One session's Host control in this window.
#[derive(Default)]
pub struct View {
    /// The operations ticked: the next activation's scope.
    pub selected: BTreeSet<String>,
    /// The last answer; the session record's brief says whether control is active meanwhile.
    pub state: Option<State>,
    pub busy: bool,
    pub error: Option<String>,
    serial: u64,
}

/// What the person can read of an operation name.
fn label(operation: &str) -> &'static str {
    match operation {
        "view" => "Display viewing",
        "input" => "Display input",
        "inspect" => "Process inspection",
        "launch" => "Process launch",
        "stop" => "Process stop",
        "stop_existing" => "Stop existing processes",
        "commands" => "General host commands",
        other => describe(other),
    }
}

impl App {
    pub(crate) fn host_control_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Select(id, operation, on) => {
                if let Some(view) = self.views.get_mut(&id) {
                    if on {
                        view.host_control.selected.insert(operation);
                    } else {
                        view.host_control.selected.remove(&operation);
                    }
                }
                Task::none()
            }
            Event::Activate(id) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                let operations: Vec<String> = view.host_control.selected.iter().cloned().collect();
                if operations.is_empty() {
                    view.host_control.error = Some("Tick at least one operation.".into());
                    return Task::none();
                }
                let params = json!({"session_id": id, "operations": operations});
                self.host_control_request(id, "_sill/host_control.activate", params)
            }
            Event::Revoke(id) => {
                let params = json!({"session_id": id});
                self.host_control_request(id, "_sill/host_control.revoke", params)
            }
            Event::Done(id, serial, outcome) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                if view.host_control.serial != serial {
                    return Task::none();
                }
                view.host_control.busy = false;
                match outcome {
                    Ok(state) => {
                        if state.active {
                            view.host_control.selected = state.operations.iter().cloned().collect();
                        }
                        view.host_control.state = Some(state);
                        view.host_control.error = None;
                    }
                    Err(error) => view.host_control.error = Some(error),
                }
                // the session records carry the active brief: the caption and the band read it
                self.list()
            }
        }
    }

    fn host_control_request(&mut self, id: String, method: &'static str, params: serde_json::Value) -> Task<Message> {
        let (Some(actions), Some(view)) = (self.actions.clone(), self.views.get_mut(&id)) else {
            return Task::none();
        };
        if view.host_control.busy {
            return Task::none();
        }
        view.host_control.busy = true;
        view.host_control.error = None;
        view.host_control.serial += 1;
        let serial = view.host_control.serial;
        blocking(
            move || actions.call_needing(CAPABILITY, method, params),
            move |outcome: Outcome| {
                let result = match outcome {
                    Outcome::Done(value) => serde_json::from_value::<State>(value)
                        .map_err(|e| format!("the daemon's answer could not be read: {e}")),
                    other => Err(other.problem().unwrap_or("failed").to_string()),
                };
                Message::HostControl(Event::Done(id, serial, result))
            },
        )
    }
}

/// The host column's rows of the Permissions panel: one per operation, its switch the choice
/// of scope to activate, "active" beside the ones the running activation grants.
pub fn rows<'a>(view: &'a SessionView, session: &'a Session, can_change: bool) -> Vec<Element<'a, Message>> {
    let id = view.id.as_str();
    let hc = &view.host_control;
    let brief: BTreeSet<String> = session.host_control.as_ref().map(|b| b.operations.iter().cloned().collect()).unwrap_or_default();
    OPERATIONS
        .iter()
        .map(|operation| {
            let ticked = hc.selected.contains(*operation);
            let set = can_change.then(|| {
                let (id, operation) = (id.to_string(), operation.to_string());
                move |on: bool| Message::HostControl(Event::Select(id.clone(), operation.clone(), on))
            });
            let status = if brief.contains(*operation) { "active" } else { "" };
            let mut said = describe(operation).to_string();
            said.push('.');
            if *operation == "commands" {
                said.push_str(" Unrestricted: Sill records the command, not what it did or left running.");
            }
            if *operation == "input" {
                said.push_str(" Input can invoke anything on your desktop; a window scope is not a boundary.");
            }
            row![
                crate::permissions::name(label(operation), said),
                crate::permissions::switch(ticked, set),
                text(status).size(style::SMALL).color(P.error).wrapping(text::Wrapping::None).width(Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
        })
        .collect()
}

/// What the host column says over its rows, and whether it is the warning colour.
pub fn heading(session: &Session) -> (&'static str, bool) {
    if session.host_control.is_some() { ("ON THE HOST · ACTIVE", true) } else { ("ON THE HOST", false) }
}

/// Activate and Revoke under the host column's rows, with the last answer or refusal beside
/// them (whole on hover).
pub fn buttons<'a>(view: &'a SessionView, session: &'a Session, can_change: bool) -> Element<'a, Message> {
    let id = view.id.as_str();
    let hc = &view.host_control;
    let brief: BTreeSet<String> = session.host_control.as_ref().map(|b| b.operations.iter().cloned().collect()).unwrap_or_default();
    let is_active = session.host_control.is_some();
    let selection_differs = hc.selected != brief;
    let can_activate = can_change && !hc.busy && !hc.selected.is_empty() && (!is_active || selection_differs);
    let activate_tip = if is_active && !selection_differs {
        "Active with exactly these operations.".to_string()
    } else if is_active {
        "Changes the active control's scope to the switched-on operations: a new generation.".to_string()
    } else {
        "Activates Host control for this session with the switched-on operations: the agent may delegate them to run on this machine, outside the jail, as you, until you revoke, this window stops showing the session, or the session stops. Never saved. Effects on the host are real and not undone by Sill; ACP approvals stay separate.".to_string()
    };
    let said: String = match (&hc.error, &hc.state) {
        (Some(error), _) => error.clone(),
        (None, Some(state)) if state.active => format!("Active, generation {}, {} displaying.", state.generation, state.displaying),
        (None, Some(state)) => match &state.ended {
            Some(ended) => format!("Inactive: {}.", ended.reason),
            None => "Inactive.".into(),
        },
        (None, None) if is_active => "Active.".into(),
        (None, None) => String::new(),
    };
    row![
        crate::permissions::tip(
            crate::permissions::small(
                "Activate host control",
                can_activate.then(|| Message::HostControl(Event::Activate(id.into()))),
            ),
            activate_tip,
        ),
        crate::permissions::tip(
            crate::permissions::small(
                "Revoke",
                (can_change && !hc.busy && is_active).then(|| Message::HostControl(Event::Revoke(id.into()))),
            ),
            "Ends Host control for this session. Processes it launched are stopped unless kept; what already happened on the host stays.".into(),
        ),
        crate::permissions::tip(
            container(
                text(said.clone())
                    .size(style::TINY)
                    .color(if hc.error.is_some() { P.error } else { alpha(P.fg, 0.7) })
                    .wrapping(text::Wrapping::None),
            )
            .width(Fill)
            .clip(true),
            said,
        ),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

/// The permanent caption over the composer while the session has active Host control.
pub fn caption<'a>(session: &Session) -> Option<Element<'a, Message>> {
    let brief = session.host_control.as_ref()?;
    Some(
        column![
            rule::horizontal(1).style(style::rule_style(P.error)),
            container(
                text(format!("{CAPTION} · {}", brief.operations.join(", ")))
                    .size(style::TINY)
                    .color(P.error)
                    .wrapping(text::Wrapping::None)
            )
            .width(Fill)
            .padding([2, 8])
            .style(style::fill(alpha(P.error, 0.08))),
        ]
        .into(),
    )
}

/// The window-level band naming every *other* session with active Host control, so switching
/// to a session without it cannot hide elevated activity. The shown session's own control is the
/// caption over the composer; the band does not repeat it (the person asked, 2026-11-07).
pub fn band(app: &App) -> Option<Element<'_, Message>> {
    let shown = app.current().map(|v| v.id.as_str());
    let names: Vec<String> = app
        .all_sessions
        .iter()
        .filter(|s| s.host_control.is_some() && Some(s.session_id.as_str()) != shown)
        .map(|s| s.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| s.title.clone()))
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(
        container(
            row![
                text("HOST CONTROL active:").size(style::TINY).color(P.error),
                text(names.join(" · ")).size(style::TINY).color(P.fg).wrapping(text::Wrapping::None),
                space().width(Fill),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([2, 10])
        .style(style::fill(alpha(P.error, 0.12)))
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{live_app, session_record, settle_task, simulator};
    use serde_json::json;

    /// A client whose daemon answered both frontend-declared capabilities.
    fn hosting_client() -> (sill_proto::Client, crate::tests::Daemon) {
        sill_proto::client::declare_capability(CAPABILITY);
        let (client, mut daemon) = crate::tests::daemon();
        let initializing = std::thread::spawn(move || {
            client.initialize().unwrap();
            client
        });
        let request = daemon.expect("_sill/initialize");
        assert_eq!(request["params"]["capabilities"][CAPABILITY], 1, "declared by sill-gui");
        let answer = sill_proto::initialize::Initialized {
            protocol: sill_proto::initialize::PROTOCOL_ID.into(),
            daemon: sill_proto::initialize::Peer { name: "sill".into(), version: "test".into() },
            capabilities: [(CAPABILITY.to_string(), 1), (crate::permissions::CAPABILITY.to_string(), 1)].into(),
            limits: Default::default(),
        };
        daemon.reply(&request, serde_json::to_value(answer).unwrap());
        (initializing.join().unwrap(), daemon)
    }

    fn with_control(mut session: Session, operations: &[&str]) -> Session {
        session.host_control = Some(sill_proto::host_control::Brief {
            generation: 1,
            operations: operations.iter().map(|s| s.to_string()).collect(),
        });
        session
    }

    /// The group activates with exactly the ticked operations; the answer and the refreshed
    /// session record then show the caption; the band is for other sessions.
    #[test]
    fn activating_sends_the_ticked_operations_and_shows_the_caption_and_band() {
        let (client, mut daemon) = hosting_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        app.views.get_mut("demo").unwrap().permissions.open = true;
        {
            let mut ui = simulator(&app, (720.0, 900.0));
            ui.find("Activate host control").unwrap();
            ui.find("ON THE HOST").unwrap();
            assert!(ui.find(CAPTION).is_err(), "nothing active yet");
        }
        for operation in ["launch", "stop"] {
            let task = app.update(Message::HostControl(Event::Select("demo".into(), operation.into(), true)));
            settle_task(&mut app, task);
        }
        let fake = std::thread::spawn(move || {
            loop {
                let request = daemon.next();
                match request["method"].as_str().unwrap() {
                    "_sill/host_control.activate" => {
                        assert_eq!(request["params"], json!({"session_id": "demo", "operations": ["launch", "stop"]}));
                        daemon.reply(&request, json!({"session_id": "demo", "active": true, "generation": 1,
                            "operations": ["launch", "stop"], "displaying": 1, "since": "1", "ended": null}));
                    }
                    "_sill/sessions.list" => {
                        let session = with_control(session_record("demo", "demo", false, false), &["launch", "stop"]);
                        daemon.reply(&request, json!({"sessions": [session], "next": null}));
                        return;
                    }
                    "_sill/policy.preview" => daemon.refuse(&request, "unavailable", "later"),
                    other => panic!("unexpected {other}"),
                }
            }
        });
        let task = app.update(Message::HostControl(Event::Activate("demo".into())));
        settle_task(&mut app, task);
        fake.join().unwrap();
        let state = app.views["demo"].host_control.state.as_ref().unwrap();
        assert!(state.active && state.generation == 1);
        assert!(app.all_sessions[0].host_control.is_some(), "the record was refreshed");
        let mut ui = simulator(&app, (720.0, 900.0));
        ui.find(format!("{CAPTION} · launch, stop")).unwrap();
        assert!(ui.find("HOST CONTROL active:").is_err(), "the shown session's caption is enough");
        ui.find("ON THE HOST · ACTIVE").unwrap();
        ui.find("Revoke").unwrap();
    }

    /// A long Host control notice (an inspection's process list) is one line until clicked.
    #[test]
    fn long_notices_fold_to_a_line_until_opened() {
        let (mut app, _) = App::new(true, None);
        let _ = app.update(Message::DemoTick);
        let long = format!("HOST action inspect completed: {}", "{\"pid\":1,\"name\":\"systemd\"},".repeat(40));
        let short = "HOST action on host · generation 1: inspect {}";
        for (id, text) in [("long", long.as_str()), ("short", short)] {
            let view = app.current_mut().unwrap();
            let mut item = view.conversation.item("user").unwrap().clone();
            item.item_id = id.into();
            item.created_seq = format!("9{}", id.len());
            item.kind = "notice".into();
            item.content = vec![sill_proto::sfp::Content::text(text)];
            let event = crate::tests::event(&view.conversation, sill_proto::sfp::Event::ItemCreated { item });
            assert!(view.accept(event));
        }
        assert!(crate::ui::is_long_notice(&long) && !crate::ui::is_long_notice(short));
        // selectable text is not a plain text widget: the fold line and its chevron are
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find(format!("{} chars", long.len())).unwrap();
        ui.find("▸").unwrap();
        assert!(ui.find("▾").is_err(), "folded until clicked");
        ui.click(format!("{} chars", long.len())).unwrap();
        let messages: Vec<Message> = ui.into_messages().collect();
        assert!(messages.iter().any(|m| matches!(m, Message::ToggleTool(id) if id == "long")), "{messages:?}");
        for m in messages {
            let _ = app.update(m);
        }
        assert!(app.current().unwrap().open_tools.contains("long"));
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("▾").unwrap();
    }

    /// The band names every session with active control, whichever one shows.
    #[test]
    fn the_band_names_other_sessions_with_active_control() {
        let (mut app, _) = App::new(true, None);
        let mut elevated = session_record("other", "other", false, false);
        elevated.name = Some("Deploy helper".into());
        app.all_sessions = vec![session_record("demo", "demo", false, false), with_control(elevated, &["commands"])];
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("HOST CONTROL active:").unwrap();
        ui.find("Deploy helper").unwrap();
        assert!(ui.find(CAPTION).is_err(), "the shown session has none");
    }
}
