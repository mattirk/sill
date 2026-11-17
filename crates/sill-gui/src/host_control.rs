//! Host control in sill-gui (docs/permission-modes.md, Host control): the Host actions group of
//! the Permissions panel, where the person ticks the operations and activates or revokes; the
//! "HOST CONTROL · actions can run outside the jail" caption over a session's composer (its
//! words for a while, its line in the control's colour as long as the control); and a window-level band naming every other session with active control, so switching to
//! a Default session cannot hide it. sill-gui declares `sill.host_control`: while a tab is
//! attached to a session, this window displays that session's control and keeps it alive.
use sill_proto::method;
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
/// The caption's words while only the VM's operations are active in Sill.app.
pub const VM_CAPTION: &str = "VM CONTROL · actions can run outside the jail, inside Sill's VM";

#[derive(Debug, Clone)]
pub enum Event {
    /// An operation ticked or unticked for the activation.
    Select(String, String, bool),
    Activate(String),
    /// A Basic switch: a whole group of operations on or off, at once.
    SetGroup(String, Group, bool),
    Revoke(String),
    Done(String, u64, Result<State, String>),
    /// The state read with the Permissions panel: the activation and the policy's ban.
    Read(String, Result<State, String>),
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
    /// The active control this window last saw on the session's record, and when it first did:
    /// the caption's words go after `CAPTION_SHOWN`.
    seen: Option<(sill_proto::host_control::Brief, std::time::Instant)>,
}

/// How long the caption says its words; its line, in the control's colour, stays as long as
/// the control.
pub const CAPTION_SHOWN: std::time::Duration = std::time::Duration::from_secs(120);

impl View {
    /// The session's record came: note when its control (this scope of it) was first seen.
    pub fn saw(&mut self, brief: Option<&sill_proto::host_control::Brief>, now: std::time::Instant) {
        match brief {
            Some(brief) if self.seen.as_ref().is_some_and(|(seen, _)| seen == brief) => {}
            Some(brief) => self.seen = Some((brief.clone(), now)),
            None => self.seen = None,
        }
    }

    /// Whether the caption has said its words long enough. Not before `saw` noted the control.
    fn words_done(&self, now: std::time::Instant) -> bool {
        self.seen.as_ref().is_some_and(|(_, since)| now.duration_since(*since) >= CAPTION_SHOWN)
    }
}

/// What the person can read of an operation name (and "Process ended" for a managed process's
/// end, as the conversation shows Host's actions).
pub(crate) fn operation_label(operation: &str) -> String {
    if operation == "process" {
        return "Process ended".into();
    }
    label(operation).to_string()
}

/// What the person can read of an operation name.
fn label(operation: &str) -> &'static str {
    match operation {
        "view" => "Display viewing",
        "input" => "Display input",
        "mac_view" => "View Mac screen",
        "mac_input" => "Control Mac",
        "mac_commands" => "Run Mac commands",
        "inspect" => "Process inspection",
        "launch" => "Process launch",
        "stop" => "Process stop",
        "stop_existing" => "Stop existing processes",
        "commands" => "General host commands",
        other => describe(other),
    }
}

/// The operations this window offers: the Mac's in Sill.app with its own VM, where the daemon's
/// host is the VM and has no desktop of its own; the host desktop's everywhere else.
pub fn offered() -> Vec<&'static str> {
    let mac = cfg!(target_os = "macos") && mac_attached();
    OPERATIONS
        .iter()
        .copied()
        .filter(|op| match *op {
            "mac_view" | "mac_input" | "mac_commands" => mac,
            "view" | "input" => !mac,
            _ => true,
        })
        .collect()
}

/// The Basic switches' groups of operations (the person, 2026-11-13). In Sill.app with its own
/// VM: VM (what acts in the VM, outside the jail) and Host (the Mac). Elsewhere the daemon's
/// host is the machine itself: Host alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Vm,
    Host,
}

/// The groups this window offers, with their names and operations.
pub fn groups() -> Vec<(Group, &'static str, Vec<&'static str>)> {
    let (mac, rest): (Vec<_>, Vec<_>) = offered().into_iter().partition(|op| sill_proto::host_control::MAC_OPERATIONS.contains(op));
    if mac.is_empty() { vec![(Group::Host, "Host", rest)] } else { vec![(Group::Vm, "VM", rest), (Group::Host, "Host", mac)] }
}

/// The groups with an operation in the session's active control.
pub fn groups_on(session: &Session) -> Vec<Group> {
    let Some(brief) = &session.host_control else { return vec![] };
    groups().into_iter().filter(|(_, _, operations)| brief.operations.iter().any(|op| operations.contains(&op.as_str()))).map(|(group, _, _)| group).collect()
}

fn mac_attached() -> bool {
    #[cfg(target_os = "macos")]
    return crate::local_vm::phase().is_some();
    #[cfg(not(target_os = "macos"))]
    false
}

impl App {
    pub(crate) fn host_control_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Select(id, operation, on) => {
                if let Some(view) = self.views.get_mut(&id) {
                    let selected = &mut view.host_control.selected;
                    if on {
                        // Mac control is aimed in pictures: it brings Mac viewing along
                        if operation == "mac_input" {
                            selected.insert("mac_view".into());
                        }
                        selected.insert(operation);
                    } else {
                        // and goes when viewing goes
                        if operation == "mac_view" {
                            selected.remove("mac_input");
                        }
                        selected.remove(&operation);
                    }
                }
                Task::none()
            }
            Event::SetGroup(id, group, on) => {
                let Some((_, _, operations)) = groups().into_iter().find(|(g, _, _)| *g == group) else {
                    return Task::none();
                };
                // from what is active now: the other group stays as it is
                let mut next: BTreeSet<String> = self
                    .all_sessions
                    .iter()
                    .find(|s| s.session_id == id)
                    .and_then(|s| s.host_control.as_ref())
                    .map(|brief| brief.operations.iter().cloned().collect())
                    .unwrap_or_default();
                for operation in operations {
                    if on {
                        next.insert(operation.to_string());
                    } else {
                        next.remove(operation);
                    }
                }
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                if view.host_control.busy {
                    return Task::none();
                }
                let revoke = next.is_empty();
                view.host_control.selected = next;
                self.host_control_event(if revoke { Event::Revoke(id) } else { Event::Activate(id) })
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
                // viewing the Mac needs macOS's consent too: asked now, by the person's action,
                // never by the agent's request
                #[cfg(target_os = "macos")]
                if operations.iter().any(|op| op == "mac_view") && !crate::mac_screen::allowed() {
                    crate::mac_screen::ask();
                }
                #[cfg(target_os = "macos")]
                if operations.iter().any(|op| op == "mac_input") && !crate::mac_input::allowed() {
                    crate::mac_input::ask();
                }
                // the Mac acts only on what the person switched on here, not on the daemon's word
                #[cfg(target_os = "macos")]
                if !view.host_control.busy {
                    crate::local_vm::person_asked(&id, &operations);
                }
                let params = json!({"session_id": id, "operations": operations});
                self.host_control_request(id, method::HOST_CONTROL_ACTIVATE, params)
            }
            Event::Read(id, result) => {
                if let Some(view) = self.views.get_mut(&id)
                    && !view.host_control.busy
                    && let Ok(state) = result
                {
                    if state.active && view.host_control.state.is_none() {
                        view.host_control.selected = state.operations.iter().cloned().collect();
                    }
                    view.host_control.state = Some(state);
                }
                Task::none()
            }
            Event::Revoke(id) => {
                let params = json!({"session_id": id});
                self.host_control_request(id, method::HOST_CONTROL_REVOKE, params)
            }
            Event::Done(id, serial, outcome) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                if view.host_control.serial != serial {
                    return Task::none();
                }
                view.host_control.busy = false;
                #[cfg(target_os = "macos")]
                crate::local_vm::person_answered(&id, outcome.as_ref().ok().filter(|s| s.active).map(|s| (s.generation, s.since.clone())), outcome.is_err());
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

    /// Read the session's Host control state: the activation and the policy's ban.
    pub(crate) fn read_host_control(&self, id: &str) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let id = id.to_string();
        let key = json!({"session_id": id});
        blocking(
            move || actions.read::<State>(method::HOST_CONTROL_STATE, key).map_err(|e| format!("{e:#}")),
            move |result| Message::HostControl(Event::Read(id, result)),
        )
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
                    Outcome::Done(value) => serde_json::from_value::<State>(value).map_err(|e| format!("the daemon's answer could not be read: {e}")),
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
    offered()
        .into_iter()
        .map(|operation| {
            let operation = &operation;
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
            if *operation == "mac_commands" {
                said.push_str(" Unrestricted: programs run as you on the Mac, from a shared folder; Sill records the command, not what it did. They stop when this is revoked and when Sill quits.");
            }
            if *operation == "mac_input" {
                said.push_str(" The whole Mac desktop: clicks and keys reach whatever is in front, as yours would. Needs View Mac screen; your own mouse or keyboard pauses it.");
                #[cfg(target_os = "macos")]
                if !crate::mac_input::allowed() {
                    said.push_str(" macOS does not allow Sill to control the mouse and keyboard yet: activating asks; turn Sill on in System Settings → Privacy & Security → Accessibility.");
                }
            }
            if *operation == "mac_view" {
                said.push_str(" Everything shown on the Mac, other apps and private windows included.");
                #[cfg(target_os = "macos")]
                if !crate::mac_screen::allowed() {
                    said.push_str(" macOS does not allow Sill to record the screen yet: activating asks; turn Sill on in System Settings → Privacy & Security → Screen & System Audio Recording.");
                }
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

/// Under the host column's rows: the session policy's ban, when it forbids Host control.
pub fn ban<'a>(view: &'a SessionView) -> Option<Element<'a, Message>> {
    let forbidden = view.host_control.state.as_ref()?.forbidden.as_ref()?;
    Some(text(format!("{forbidden}.")).size(style::TINY).color(P.error).into())
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
    let forbidden = hc.state.as_ref().and_then(|s| s.forbidden.clone());
    let can_activate = can_change && !hc.busy && !hc.selected.is_empty() && (!is_active || selection_differs) && forbidden.is_none();
    let activate_tip = if let Some(forbidden) = forbidden {
        forbidden
    } else if is_active && !selection_differs {
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
            crate::permissions::small("Activate host control", can_activate.then(|| Message::HostControl(Event::Activate(id.into()))),),
            activate_tip,
        ),
        crate::permissions::tip(
            crate::permissions::small("Revoke", (can_change && !hc.busy && is_active).then(|| Message::HostControl(Event::Revoke(id.into()))),),
            "Ends Host control for this session. Processes it launched are stopped unless kept; what already happened on the host stays.".into(),
        ),
        crate::permissions::tip(
            container(text(said.clone()).size(style::TINY).color(if hc.error.is_some() { P.error } else { alpha(P.fg, 0.7) }).wrapping(text::Wrapping::None),)
                .width(Fill)
                .clip(true),
            said,
        ),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

/// The caption's words and colour for these active operations: Host control in the warning's
/// stronger colour; in Sill.app, with only the VM's operations active, VM control in yellow (the
/// agent acts outside its jail but inside Sill's VM; the person, 2026-11-13).
fn caption_words(operations: &[String]) -> (&'static str, iced::Color) {
    let vm_only = groups().len() > 1 && !operations.iter().any(|op| sill_proto::host_control::MAC_OPERATIONS.contains(&op.as_str()));
    if vm_only { (VM_CAPTION, style::yellow()) } else { (CAPTION, P.error) }
}

/// The colour of the daemon's "Host control activated" notice in the conversation: yellow when
/// it activated only the VM's operations in Sill.app, else `color` as given.
pub fn notice_color(text: &str, color: iced::Color) -> iced::Color {
    if text.starts_with("Host control activated") && groups().len() > 1 && !text.contains("mac_") { style::yellow() } else { color }
}

/// The caption over the composer while the session has active Host control.
///
/// The words go after `CAPTION_SHOWN`; the line stays as long as the control, yellow for VM
/// control and the warning's colour for Host control (the person, 2026-11-13).
pub fn caption<'a>(session: &Session, view: &View) -> Option<Element<'a, Message>> {
    let brief = session.host_control.as_ref()?;
    let (words, color) = caption_words(&brief.operations);
    if view.words_done(std::time::Instant::now()) {
        return Some(rule::horizontal(1).style(style::rule_style(color)).into());
    }
    Some(
        column![
            rule::horizontal(1).style(style::rule_style(color)),
            container(
                text(format!("{words} · {}", brief.operations.join(", "))).size(style::TINY).color(color).width(Fill).wrapping(text::Wrapping::WordOrGlyph)
            )
            .width(Fill)
            .padding([2, 8])
            .clip(true)
            .style(style::fill(alpha(color, 0.08))),
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

/// Keep the audit record available on expansion, but lead with the action people recognize.
pub(crate) fn notice_summary(message: &str) -> String {
    if let Some(rest) = message.strip_prefix("HOST action on host · generation ")
        && let Some((_, action)) = rest.split_once(": ")
    {
        let (operation, args) = action.split_once(' ').unwrap_or((action, ""));
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(args)
            && let Some(argv) = value["argv"].as_array()
        {
            let words: Vec<&str> = argv.iter().filter_map(|v| v.as_str()).collect();
            // Shell scripts are the command, not the shell's JSON invocation.
            let command = if words.len() == 3 && words[1] == "-c" {
                words[2].to_owned()
            } else {
                words
                    .iter()
                    .map(|word| {
                        if word.chars().all(|c| c.is_ascii_alphanumeric() || "_-./=:".contains(c)) && !word.is_empty() {
                            (*word).to_owned()
                        } else {
                            format!("'{}'", word.replace('\'', "'\\''"))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            return format!("HOST · {command}");
        }
        return format!("HOST · {}", label(operation));
    }
    if let Some(rest) = message.strip_prefix("HOST action ") {
        if let Some((operation, _)) = rest.split_once(" completed:") {
            return format!("HOST · {} completed", label(operation));
        }
        if let Some((operation, _)) = rest.split_once(" failed:") {
            return format!("HOST · {} failed — expand for details", label(operation));
        }
    }
    if message.starts_with("HOST process ") {
        return if message.contains("revocation stop:") {
            "HOST · Process stopped after control ended".into()
        } else {
            "HOST · Process ended — expand for details".into()
        };
    }
    message.to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_captions_words_go_after_a_while_and_return_with_a_new_scope() {
        use sill_proto::host_control::Brief;
        let mut view = View::default();
        let start = std::time::Instant::now();
        let later = start + CAPTION_SHOWN;
        assert!(!view.words_done(later), "not before the control was seen");
        let brief = Brief { generation: 1, operations: vec!["commands".into()] };
        view.saw(Some(&brief), start);
        assert!(!view.words_done(start + CAPTION_SHOWN - std::time::Duration::from_secs(1)));
        assert!(view.words_done(later));
        // the same control seen again keeps its time; another scope starts over
        view.saw(Some(&brief), later);
        assert!(view.words_done(later));
        view.saw(Some(&Brief { generation: 2, operations: vec!["commands".into(), "launch".into()] }), later);
        assert!(!view.words_done(later));
        view.saw(None, later);
        assert!(!view.words_done(later + CAPTION_SHOWN));
    }

    #[test]
    fn mac_control_brings_mac_viewing_along_and_goes_with_it() {
        let (mut app, _) = crate::App::with(true, None, None);
        let select = |app: &mut crate::App, op: &str, on: bool| {
            let _ = app.update(crate::Message::HostControl(super::Event::Select("demo".into(), op.into(), on)));
        };
        select(&mut app, "mac_input", true);
        let selected = &app.views["demo"].host_control.selected;
        assert!(selected.contains("mac_input") && selected.contains("mac_view"));
        select(&mut app, "mac_view", false);
        assert!(app.views["demo"].host_control.selected.is_empty());
    }

    use super::*;
    use crate::tests::{live_app, session_record, settle_task, simulator};
    use serde_json::json;

    #[test]
    fn host_summaries_show_commands_without_audit_metadata() {
        assert_eq!(notice_summary(r#"HOST action on host · generation 7: commands {"argv":["/bin/sh","-c","echo hello"]}"#), "HOST · echo hello");
        assert_eq!(notice_summary(r#"HOST action on host · generation 7: launch {"argv":["echo","two words"]}"#), "HOST · echo 'two words'");
        assert_eq!(notice_summary(r#"HOST action stop completed: {"stopped":"host:123"}"#), "HOST · Process stop completed");
        assert_eq!(notice_summary("HOST action commands failed: denied"), "HOST · General host commands failed — expand for details");
    }

    /// A client whose daemon answered both frontend-declared capabilities.
    fn hosting_client() -> (sill_proto::Client, crate::tests::Daemon) {
        sill_proto::client::declare_capability(CAPABILITY);
        let (client, mut daemon) = crate::tests::daemon();
        let initializing = std::thread::spawn(move || {
            client.initialize().unwrap();
            client
        });
        let request = daemon.expect(sill_proto::method::INITIALIZE);
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
        session.host_control = Some(sill_proto::host_control::Brief { generation: 1, operations: operations.iter().map(|s| s.to_string()).collect() });
        session
    }

    /// A policy that forbids Host control: the ban shows under the host column and Activate
    /// stays closed whatever is ticked.
    #[test]
    fn a_policy_ban_shows_and_keeps_activate_closed() {
        let (client, _daemon) = hosting_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        app.views.get_mut("demo").unwrap().permissions.open = true;
        let task = app.update(Message::HostControl(Event::Select("demo".into(), "launch".into(), true)));
        settle_task(&mut app, task);
        let activates = |app: &App| {
            let mut ui = simulator(app, (900.0, 1100.0));
            ui.click("Activate host control").unwrap();
            ui.into_messages().any(|m| matches!(m, Message::HostControl(Event::Activate(_))))
        };
        assert!(activates(&app), "open before the ban is known");
        let state: State = serde_json::from_value(json!({"session_id": "demo", "active": false, "generation": 0,
            "operations": [], "displaying": 0, "since": null, "ended": null,
            "forbidden": "Host control is forbidden by the session's policy"}))
        .unwrap();
        let task = app.update(Message::HostControl(Event::Read("demo".into(), Ok(state))));
        settle_task(&mut app, task);
        simulator(&app, (900.0, 1100.0)).find("Host control is forbidden by the session's policy.").unwrap();
        assert!(!activates(&app));
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
                    method::HOST_CONTROL_ACTIVATE => {
                        assert_eq!(request["params"], json!({"session_id": "demo", "operations": ["launch", "stop"]}));
                        daemon.reply(
                            &request,
                            json!({"session_id": "demo", "active": true, "generation": 1,
                            "operations": ["launch", "stop"], "displaying": 1, "since": "1", "ended": null}),
                        );
                    }
                    method::SESSIONS_LIST => {
                        let session = with_control(session_record("demo", "demo", false, false), &["launch", "stop"]);
                        daemon.reply(&request, json!({"sessions": [session], "next": null}));
                        return;
                    }
                    method::POLICY_PREVIEW => daemon.refuse(&request, "unavailable", "later"),
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

    /// The Host switch (here, without a Mac's VM, the only group) activates every operation the
    /// window offers at once, from the shield menu without opening the panel.
    #[test]
    fn the_host_switch_activates_its_group_at_once() {
        let (client, mut daemon) = hosting_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let _ = app.update(Message::Perms(crate::permissions::Event::ShieldMenu("demo".into())));
        assert!(app.views["demo"].permissions.menu && !app.views["demo"].permissions.open);
        // the selection is a sorted set; the daemon receives every operation this window offers
        // (no Mac here: everything but the Mac's operations) in that order
        let mut all: Vec<&str> = super::offered();
        let mac = sill_proto::host_control::MAC_OPERATIONS.len();
        assert!(!all.contains(&"mac_view") && all.len() == OPERATIONS.len() - mac);
        all.sort_unstable();
        let fake = std::thread::spawn(move || {
            let mut listed = false;
            loop {
                let request = daemon.next();
                match request["method"].as_str().unwrap() {
                    method::HOST_CONTROL_ACTIVATE => {
                        assert_eq!(request["params"], json!({"session_id": "demo", "operations": all}));
                        daemon.reply(
                            &request,
                            json!({"session_id": "demo", "active": true, "generation": 1,
                            "operations": all, "displaying": 1, "since": "1", "ended": null}),
                        );
                        if listed {
                            return;
                        }
                        listed = true;
                    }
                    method::SESSIONS_LIST => {
                        let session = with_control(session_record("demo", "demo", false, false), &all);
                        daemon.reply(&request, json!({"sessions": [session], "next": null}));
                        if listed {
                            return;
                        }
                        listed = true;
                    }
                    method::HOST_CONTROL_STATE => daemon.refuse(&request, "unavailable", "later"),
                    method::POLICY_PREVIEW => daemon.refuse(&request, "unavailable", "later"),
                    other => panic!("unexpected {other}"),
                }
            }
        });
        let task = app.update(Message::Perms(crate::permissions::Event::HostGroup("demo".into(), super::Group::Host, true)));
        settle_task(&mut app, task);
        fake.join().unwrap();
        let perms = &app.views["demo"].permissions;
        assert!(!perms.open && perms.menu, "the dropdown stays for the next switch; the panel stays closed");
        let state = app.views["demo"].host_control.state.as_ref().unwrap();
        assert!(state.active && state.operations.len() == OPERATIONS.len() - mac);
        assert_eq!(app.views["demo"].host_control.selected.len(), OPERATIONS.len() - mac);
    }

    /// Host's actions fold into a block of their own (`host_runs`): a request and its outcome
    /// are one line under their operation, closed until clicked; opened, the full result shows.
    #[test]
    fn host_actions_are_one_line_each_in_a_block_until_opened() {
        let (mut app, _) = App::new(true, None);
        let _ = app.update(Message::DemoTick);
        let long = format!("HOST action inspect completed: {}", "{\"pid\":1,\"name\":\"systemd\"},".repeat(40));
        let request = "HOST action on host · generation 1: inspect {}";
        for (id, seq, text) in [("asked", "91", request), ("answered", "92", long.as_str())] {
            let view = app.current_mut().unwrap();
            let mut item = view.conversation.item("user").unwrap().clone();
            item.item_id = id.into();
            item.created_seq = seq.into();
            item.kind = "notice".into();
            item.content = vec![sill_proto::sfp::Content::text(text)];
            let event = crate::tests::event(&view.conversation, sill_proto::sfp::Event::ItemCreated { item });
            assert!(view.accept(event));
        }
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("Process inspection").unwrap();
        assert!(ui.find("HOST · Process inspection completed").is_err(), "no notice line of its own any more");
        ui.click("Process inspection").unwrap();
        let messages: Vec<Message> = ui.into_messages().collect();
        assert!(messages.iter().any(|m| matches!(m, Message::Session(_, crate::session::Event::ToggleTool(id)) if id == "asked")), "{messages:?}");
        for m in messages {
            let _ = app.update(m);
        }
        assert!(app.current().unwrap().open_tools.contains("asked"));
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("inspect {}").unwrap();
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
