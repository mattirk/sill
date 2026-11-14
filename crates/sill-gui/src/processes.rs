//! Managed processes and exposed ports (docs/tasks.md §2.4, §2.5): every open session's processes
//! are read every few seconds; the composer's bar shows how many run (warning colour while an
//! ended one failed), and pressing it
//! shows the list above the composer — name, state and uptime, the command, the last output line,
//! the address when its port is exposed — with Log, Stop / Dismiss per row and Stop all.
//!
//! A running process that declared a port is exposed on this machine's loopback without asking
//! when the session's launch policy lists the port under `[processes] expose`; otherwise a card
//! asks once per session. The bridges belong to the session's view: they close when the process
//! ends, the session stops or its tab goes. sill-client's `processes` does the relaying.
use std::collections::{BTreeMap, BTreeSet};

use iced::widget::{
    Column, button, column, container, row, rule, scrollable, space, text, text_editor,
};
use iced::{Alignment, Element, Fill, Font, Task};
use sill_client::actions::Outcome;
use sill_client::processes::{self as procs, Bridge, Process};
use sill_proto::admin::{JailProcess, JailProcesses};

use crate::style::{self, P, alpha};
use crate::{App, Message, SessionView, blocking, icons, json, keys};

/// How long a note on an exposed port or a refused action stays.
const NOTICE: std::time::Duration = std::time::Duration::from_secs(if cfg!(test) { 0 } else { 8 });

#[derive(Debug, Clone)]
pub enum Event {
    Poll,
    /// Stop a command the agent left running in the jail: its root's pid there.
    StopCommand(String, u32),
    /// A session's list (an error: no jail to ask, so no ports either) and, when a running port
    /// waited for a decision, the expose specs of its launch policy.
    Read(
        String,
        u64,
        Result<Vec<Process>, String>,
        Option<Vec<String>>,
        JailProcesses,
    ),
    /// Show or hide the list.
    Toggle(String),
    /// Expose a port the person was asked about, or one shown as not exposed.
    Expose(String, u16),
    NotNow(String, u16),
    /// Show or hide a process's output.
    Log(String, String),
    LogRead(String, String, u64, Result<String, String>),
    LogText(String, text_editor::Action),
    /// Stop one (every running one, with an empty name) or dismiss an ended one.
    Stop(String, String),
    Dismiss(String, String),
    Done(String, Outcome),
    OpenUrl(String),
    NoticeExpired(String, u64),
}

/// A session's managed processes in this window.
#[derive(Default)]
pub struct Procs {
    pub list: Vec<Process>,
    /// What runs in the jail now, as the last poll read it (docs/jail-processes.md).
    pub jail: JailProcesses,
    reading: Option<u64>,
    refresh_pending: bool,
    log_reading: Option<u64>,
    pub bridges: BTreeMap<u16, Bridge>,
    /// Ports asked about once, and those the person said yes to, this session.
    asked: BTreeSet<u16>,
    approved: BTreeSet<u16>,
    /// The cards waiting for an answer: port and process name.
    pub questions: Vec<(u16, String)>,
    pub open: bool,
    /// The output shown: its process, and the text read last.
    pub log: Option<(String, text_editor::Content)>,
    /// A note on the last exposure or action: its number, the text, whether it is a failure.
    notice: Option<(u64, String, bool)>,
}

impl Procs {
    fn running_ports(&self) -> Vec<(u16, String)> {
        self.list
            .iter()
            .filter(|p| procs::running(p))
            .filter_map(|p| p.port.map(|port| (port, p.name.clone())))
            .collect()
    }

    /// Ports already bridged or asked about: the policy is read only for a port not among them.
    fn known(&self) -> BTreeSet<u16> {
        self.bridges
            .keys()
            .copied()
            .chain(self.asked.iter().copied())
            .collect()
    }

    /// The list came back: ports of ended processes close, new ones are exposed or asked about.
    /// What to expose comes back, for the caller to bridge (it holds the connection).
    pub(crate) fn read(
        &mut self,
        list: Vec<Process>,
        specs: Option<&[String]>,
    ) -> Vec<(u16, String)> {
        self.reading = None;
        self.refresh_pending = false;
        self.list = list;
        let running = self.running_ports();
        self.bridges
            .retain(|port, _| running.iter().any(|(p, _)| p == port));
        self.questions
            .retain(|(port, _)| running.iter().any(|(p, _)| p == port));
        let mut expose = vec![];
        for (port, name) in running {
            if self.bridges.contains_key(&port) {
                continue;
            }
            if self.approved.contains(&port) || specs.is_some_and(|s| procs::allows(s, port)) {
                expose.push((port, name));
            } else if specs.is_some() && self.asked.insert(port) {
                self.questions.push((port, name));
            }
        }
        if self
            .log
            .as_ref()
            .is_some_and(|(name, _)| !self.list.iter().any(|p| &p.name == name))
        {
            self.log = None;
            self.log_reading = None;
        }
        expose
    }

    fn note(&mut self, serial: u64, text: String, failed: bool) {
        self.notice = Some((serial, text, failed));
    }
}

/// A running port neither bridged nor asked about: reading the policy is needed for it.
fn undecided(known: &BTreeSet<u16>, list: &[Process]) -> bool {
    list.iter()
        .filter(|p| procs::running(p))
        .filter_map(|p| p.port)
        .any(|port| !known.contains(&port))
}

impl App {
    fn session_running(&self, id: &str) -> bool {
        self.all_sessions
            .iter()
            .any(|s| s.session_id == id && s.running && s.ended_at.is_none())
    }

    pub(crate) fn processes_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Poll => return self.poll_processes(),
            Event::Read(id, serial, result, specs, jail) => {
                let running = self.demo || self.session_running(&id);
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                if view.processes.reading != Some(serial) {
                    return Task::none();
                }
                if !running {
                    view.processes.read(vec![], None);
                    view.processes.jail = JailProcesses::default();
                    return Task::none();
                }
                if view.processes.refresh_pending {
                    // A completed action supersedes the list this poll started reading.
                    view.processes.reading = None;
                    view.processes.refresh_pending = false;
                    return self.poll_one(&id);
                }
                view.processes.jail = jail;
                let expose = view
                    .processes
                    .read(result.unwrap_or_default(), specs.as_deref());
                let mut tasks = vec![];
                for (port, name) in expose {
                    tasks.push(self.expose_port(&id, port, &name));
                }
                if let Some(name) = self
                    .views
                    .get(&id)
                    .and_then(|view| view.processes.log.as_ref().map(|(name, _)| name.clone()))
                {
                    tasks.push(self.read_log(&id, name));
                }
                return Task::batch(tasks);
            }
            Event::Toggle(id) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.processes.open = !view.processes.open;
                    if !view.processes.open {
                        view.processes.log = None;
                        view.processes.log_reading = None;
                    }
                }
            }
            Event::Expose(id, port) => {
                if !self.demo && !self.session_running(&id) {
                    return Task::none();
                }
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                if !view
                    .processes
                    .running_ports()
                    .iter()
                    .any(|(p, _)| *p == port)
                {
                    return Task::none();
                }
                view.processes.approved.insert(port);
                view.processes.questions.retain(|(p, _)| *p != port);
                let name = view
                    .processes
                    .list
                    .iter()
                    .find(|p| p.port == Some(port))
                    .map_or_else(|| format!("port {port}"), |p| p.name.clone());
                return self.expose_port(&id, port, &name);
            }
            Event::NotNow(id, port) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.processes.questions.retain(|(p, _)| *p != port);
                }
            }
            Event::Log(id, name) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                if view.processes.log.as_ref().is_some_and(|(n, _)| *n == name) {
                    view.processes.log = None;
                    view.processes.log_reading = None;
                    return Task::none();
                }
                view.processes.log_reading = None;
                view.processes.log =
                    Some((name.clone(), text_editor::Content::with_text("Reading…")));
                return self.read_log(&id, name);
            }
            Event::LogRead(id, name, serial, result) => {
                if let Some(view) = self.views.get_mut(&id)
                    && view.processes.log_reading == Some(serial)
                    && let Some((shown, content)) = &mut view.processes.log
                    && *shown == name
                {
                    view.processes.log_reading = None;
                    let text =
                        result.unwrap_or_else(|e| format!("The output could not be read: {e}"));
                    let text = if text.trim().is_empty() {
                        "No output yet.".to_string()
                    } else {
                        text
                    };
                    let text = text.trim_end();
                    if content.text() != text {
                        *content = text_editor::Content::with_text(text);
                        content
                            .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
                    }
                }
            }
            Event::LogText(id, action) => {
                // read-only: selecting, moving and scrolling, never editing
                if !matches!(action, text_editor::Action::Edit(_))
                    && let Some(view) = self.views.get_mut(&id)
                    && let Some((_, content)) = &mut view.processes.log
                {
                    content.perform(action);
                }
            }
            Event::Stop(id, name) => {
                // Stop all: each running one by name (the daemon takes no empty name)
                let names = if name.is_empty() {
                    self.views.get(&id).map_or_else(Vec::new, |v| {
                        v.processes
                            .list
                            .iter()
                            .filter(|p| procs::running(p))
                            .map(|p| p.name.clone())
                            .collect()
                    })
                } else {
                    vec![name]
                };
                return self.process_action(id, "_sill/procs.stop", names);
            }
            Event::StopCommand(id, pid) => {
                let Some(actions) = self.actions.clone() else {
                    return Task::none();
                };
                let session = id.clone();
                return blocking(
                    move || actions.call_needing("sill.jail_processes", "_sill/procs.stop_command", json!({"session_id": session, "pid": pid})),
                    move |outcome| Message::Procs(Event::Done(id, outcome)),
                );
            }
            Event::Dismiss(id, name) => {
                return self.process_action(id, "_sill/procs.dismiss", vec![name]);
            }
            Event::Done(id, outcome) => {
                let mut tasks = vec![];
                if let Some(problem) = outcome.problem() {
                    tasks.push(self.process_note(&id, format!("process: {problem}"), true));
                }
                if let Some(view) = self.views.get_mut(&id)
                    && view.processes.reading.is_some()
                {
                    view.processes.refresh_pending = true;
                } else {
                    tasks.push(self.poll_one(&id));
                }
                return Task::batch(tasks);
            }
            Event::OpenUrl(url) => crate::terminal::open_link(url),
            Event::NoticeExpired(id, serial) => {
                if let Some(view) = self.views.get_mut(&id)
                    && view
                        .processes
                        .notice
                        .as_ref()
                        .is_some_and(|(n, _, _)| *n == serial)
                {
                    view.processes.notice = None;
                }
            }
        }
        Task::none()
    }

    /// Every open session's list; a session that is not running has none, nor ports.
    fn poll_processes(&mut self) -> Task<Message> {
        if self.demo || self.actions.is_none() {
            return Task::none();
        }
        let ids: Vec<String> = self.views.keys().cloned().collect();
        Task::batch(ids.iter().map(|id| self.poll_one(id)).collect::<Vec<_>>())
    }

    fn poll_one(&mut self, id: &str) -> Task<Message> {
        let running = self.session_running(id);
        let (Some(actions), Some(view)) = (self.actions.clone(), self.views.get_mut(id)) else {
            return Task::none();
        };
        if !running {
            view.processes.read(vec![], None);
            view.processes.jail = JailProcesses::default();
            return Task::none();
        }
        if view.processes.reading.is_some() {
            return Task::none();
        }
        self.process_serial += 1;
        let serial = self.process_serial;
        view.processes.reading = Some(serial);
        let known = view.processes.known();
        let policy = self
            .all_sessions
            .iter()
            .find(|s| s.session_id == id)
            .map(|s| s.policy.clone())
            .unwrap_or_default();
        let session = id.to_string();
        let id = id.to_string();
        blocking(
            move || {
                let list = actions
                    .reading(|client| procs::list(client, &session))
                    .map_err(|e| format!("{e:#}"));
                let specs = match &list {
                    Ok(list) if undecided(&known, list) => actions
                        .reading(|client| procs::session_exposable(client, &session, &policy))
                        .ok(),
                    _ => None,
                };
                let jail = actions.reading(|client| procs::jail(client, &session)).unwrap_or_default();
                (list, specs, jail)
            },
            move |(list, specs, jail)| Message::Procs(Event::Read(id, serial, list, specs, jail)),
        )
    }

    fn read_log(&mut self, id: &str, name: String) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let Some(view) = self.views.get_mut(id) else {
            return Task::none();
        };
        if view.processes.log_reading.is_some() {
            return Task::none();
        }
        self.process_serial += 1;
        let serial = self.process_serial;
        view.processes.log_reading = Some(serial);
        let (session, id) = (id.to_string(), id.to_string());
        blocking(
            {
                let name = name.clone();
                move || {
                    actions
                        .read::<sill_proto::Text>(
                            "_sill/procs.tail",
                            json!({"session_id": session, "name": name, "lines": procs::LOG_LINES}),
                        )
                        .map(|t| t.text)
                        .map_err(|e| format!("{e:#}"))
                }
            },
            move |result| Message::Procs(Event::LogRead(id, name, serial, result)),
        )
    }

    /// `method` for each of `names`, once each; the first that did not take effect is the outcome.
    fn process_action(
        &mut self,
        id: String,
        method: &'static str,
        names: Vec<String>,
    ) -> Task<Message> {
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let session = id.clone();
        blocking(
            move || {
                let mut outcome = Outcome::Done(json!({}));
                for name in names {
                    let done = actions.call(method, json!({"session_id": session, "name": name}));
                    if outcome.is_done() && !done.is_done() {
                        outcome = done;
                    }
                }
                outcome
            },
            move |outcome| Message::Procs(Event::Done(id, outcome)),
        )
    }

    fn expose_port(&mut self, id: &str, port: u16, name: &str) -> Task<Message> {
        // A second queued click (or two process rows declaring the same port) reuses the
        // existing listener instead of moving the URL to a fallback port.
        if self
            .views
            .get(id)
            .is_none_or(|view| view.processes.bridges.contains_key(&port))
        {
            return Task::none();
        }
        let Some(actions) = self.actions.clone() else {
            return Task::none();
        };
        let opener = procs::opener(actions, id.to_string(), port);
        let (said, failed) = match procs::expose(port, opener) {
            Ok(bridge) => {
                let said = if bridge.host_port == port {
                    format!(
                        "{name}: port {port} is open on this machine at {}",
                        bridge.url()
                    )
                } else {
                    format!(
                        "{name}: port {port} is open on this machine at {} ({port} was taken)",
                        bridge.url()
                    )
                };
                if let Some(view) = self.views.get_mut(id) {
                    view.processes.bridges.insert(port, bridge);
                }
                (said, false)
            }
            Err(e) => (
                format!("{name}: port {port} could not be exposed: {e:#}"),
                true,
            ),
        };
        self.process_note(id, said, failed)
    }

    fn process_note(&mut self, id: &str, said: String, failed: bool) -> Task<Message> {
        self.process_serial += 1;
        let serial = self.process_serial;
        let Some(view) = self.views.get_mut(id) else {
            return Task::none();
        };
        view.processes.note(serial, said, failed);
        let id = id.to_string();
        blocking(
            move || std::thread::sleep(NOTICE),
            move |()| Message::Procs(Event::NoticeExpired(id, serial)),
        )
    }
}

/// The composer bar's indicator: the count of running processes, in the warning colour while an
/// ended one failed; nothing while the session has none.
/// Services in the jail that are not managed processes: programs listening on a port.
fn unmanaged_services(view: &SessionView) -> Vec<&JailProcess> {
    view.processes.jail.services.iter().filter(|s| s.name.is_none()).collect()
}

/// A command's name as one would type it: a shell's script, else the program and its arguments.
pub fn command_name(argv: &[String]) -> String {
    let said = match argv {
        [_, flag, script, ..] if flag == "-c" || flag == "-lc" => script.clone(),
        [program, rest @ ..] => {
            let base = std::path::Path::new(program).file_name().map_or(program.clone(), |b| b.to_string_lossy().into_owned());
            std::iter::once(base).chain(rest.iter().cloned()).collect::<Vec<_>>().join(" ")
        }
        [] => String::new(),
    };
    crate::ui::one_line(&said, 48)
}

/// How long something has run: 45s, 3m, 1h 05m.
pub fn elapsed(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h {:02}m", s / 3600, s / 60 % 60),
    }
}

/// The bar's chip: how many commands run now and how many services, each behind its own icon;
/// the tooltip names them, and it opens the panel.
pub fn indicator(view: &SessionView) -> Option<Element<'_, Message>> {
    let list = &view.processes.list;
    let commands = view.processes.jail.commands.len();
    let managed = list.iter().filter(|p| procs::running(p)).count();
    let services = managed + unmanaged_services(view).len();
    if list.is_empty() && commands == 0 && services == 0 {
        return None;
    }
    let failed = list.iter().any(procs::failed);
    let color = if failed {
        P.warning
    } else if view.processes.open {
        P.accent
    } else {
        P.fg_dim
    };
    // a count behind each kind's own icon: commands running now, then services (the only one
    // shown when no command runs, so ended managed processes keep their place)
    let counted = |icon: &'static str, n: usize| {
        row![icons::glyph(icon, crate::ui::BAR_ICON, color), text(n.to_string()).size(style::TINY).color(color)]
            .spacing(3)
            .align_y(Alignment::Center)
    };
    let mut content = iced::widget::Row::new().spacing(8).align_y(Alignment::Center);
    if commands > 0 {
        content = content.push(counted("running", commands));
    }
    if services > 0 || commands == 0 {
        content = content.push(counted("processes", services));
    }
    let mut tip: Vec<String> =
        view.processes.jail.commands.iter().map(|c| format!("running: {}", command_name(&c.argv))).collect();
    tip.extend(list.iter().filter(|p| procs::running(p)).map(|p| format!("service: {}", p.name)));
    tip.extend(unmanaged_services(view).iter().map(|s| format!("service: {}", command_name(&s.argv))));
    if failed {
        tip.push("a service has failed".into());
    }
    Some(
        iced::widget::tooltip(
            keys::press(
                button(container(content).center_y(Fill))
                    .height(crate::ui::BAR_BUTTON)
                    .padding([0, 6])
                    .style(style::quiet),
                Message::Procs(Event::Toggle(view.id.clone())),
            ),
            container(text(tip.join("\n")).size(style::SMALL))
                .padding([4, 8])
                .style(style::fill(P.code_bg)),
            iced::widget::tooltip::Position::Top,
        )
        .into(),
    )
}

fn small_button(label: &str, message: Message) -> Element<'_, Message> {
    keys::press(
        button(text(label).size(style::TINY))
            .padding([2, 8])
            .style(style::quiet),
        message,
    )
}

/// The cards asking about ports, the note on the last exposure or action, and the list when shown.
pub fn panel(view: &SessionView) -> Element<'_, Message> {
    let state = &view.processes;
    let anything = !state.list.is_empty() || !state.jail.commands.is_empty() || !unmanaged_services(view).is_empty();
    if state.questions.is_empty() && state.notice.is_none() && !(state.open && anything) {
        return space().height(0).into();
    }
    let id = &view.id;
    let mut content = Column::new().spacing(6);
    for (port, name) in &state.questions {
        let card = column![
            text(format!(
                "{name} listens on port {port} inside the jail. Expose it on this machine, loopback only (http://127.0.0.1:{port})?"
            ))
            .size(style::SMALL),
            row![
                keys::press(
                    button(text("Expose").size(style::SMALL))
                        .padding([4, 10])
                        .style(style::solid(P.accent)),
                    Message::Procs(Event::Expose(id.clone(), *port)),
                ),
                keys::press(
                    button(text("Not now").size(style::SMALL))
                        .padding([4, 10])
                        .style(style::quiet),
                    Message::Procs(Event::NotNow(id.clone(), *port)),
                ),
            ]
            .spacing(8),
        ]
        .spacing(8);
        content = content.push(crate::ui::ruled(
            2.0,
            P.accent,
            container(card)
                .padding([6, 10])
                .width(Fill)
                .style(style::card_right(alpha(P.accent, 0.08))),
        ));
    }
    if state.open && anything {
        content = content.push(list(view));
    }
    if let Some((_, note, failed)) = &state.notice {
        content = content.push(text(note).size(style::TINY).color(if *failed {
            P.error
        } else {
            alpha(P.fg, 0.7)
        }));
    }
    // set off from the prompt as the queued prompts are: a faint band, a rule, a gap
    column![
        container(content)
            .padding([6, 12])
            .width(Fill)
            .style(style::fill(alpha(P.accent, 0.05))),
        rule::horizontal(1).style(style::rule_style(P.border)),
        space().height(4),
    ]
    .into()
}

fn list(view: &SessionView) -> Element<'_, Message> {
    let state = &view.processes;
    let id = &view.id;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mono = Font::with_name(crate::fonts::MONO_FAMILY);
    let mut rows = Column::new().spacing(8);
    let now_ms = now * 1000;
    let heading = |label: &'static str| text(label).size(style::TINY).color(P.fg_dim);
    // what the agent started and is running now: one line each, stoppable
    let commands = &state.jail.commands;
    if !commands.is_empty() {
        rows = rows.push(heading("RUNNING NOW"));
        for c in commands {
            let mut said = elapsed(now_ms.saturating_sub(c.started_ms));
            if c.count > 1 {
                said.push_str(&format!(" · {} processes", c.count));
            }
            rows = rows.push(
                column![
                    row![
                        text(command_name(&c.argv)).size(style::SMALL).font(Font {
                            weight: iced::font::Weight::Bold,
                            ..Font::with_name(crate::fonts::SANS_FAMILY)
                        }),
                        text(said).size(style::TINY).color(P.fg_dim),
                        space().width(Fill),
                        small_button("Stop", Message::Procs(Event::StopCommand(id.clone(), c.pid))),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                    text(crate::ui::one_line(&c.argv.join(" "), 100))
                        .size(style::TINY)
                        .font(mono)
                        .color(P.fg_dim)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(2),
            );
        }
    }
    let others = unmanaged_services(view);
    if !state.list.is_empty() || !others.is_empty() {
        rows = rows.push(heading("SERVICES"));
    }
    for s in others {
        let mut said = format!("running · {}", elapsed(now_ms.saturating_sub(s.started_ms)));
        if let Some(port) = s.port {
            said.push_str(&format!(" · port {port}"));
        }
        rows = rows.push(
            column![
                row![
                    text(command_name(&s.argv)).size(style::SMALL).font(Font {
                        weight: iced::font::Weight::Bold,
                        ..Font::with_name(crate::fonts::SANS_FAMILY)
                    }),
                    text(said).size(style::TINY).color(P.fg_dim),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                text(crate::ui::one_line(&s.argv.join(" "), 100))
                    .size(style::TINY)
                    .font(mono)
                    .color(P.fg_dim)
                    .wrapping(text::Wrapping::None),
            ]
            .spacing(2),
        );
    }
    for p in &state.list {
        let running = procs::running(p);
        let shown = state.log.as_ref().is_some_and(|(n, _)| *n == p.name);
        let head = row![
            text(&p.name).size(style::SMALL).font(Font {
                weight: iced::font::Weight::Bold,
                ..Font::with_name(crate::fonts::SANS_FAMILY)
            }),
            text(procs::state(p, now))
                .size(style::TINY)
                .color(if procs::failed(p) {
                    P.warning
                } else {
                    P.fg_dim
                }),
            space().width(Fill),
            small_button(
                if shown { "Hide log" } else { "Log" },
                Message::Procs(Event::Log(id.clone(), p.name.clone())),
            ),
            if running {
                small_button(
                    "Stop",
                    Message::Procs(Event::Stop(id.clone(), p.name.clone())),
                )
            } else {
                small_button(
                    "Dismiss",
                    Message::Procs(Event::Dismiss(id.clone(), p.name.clone())),
                )
            },
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let mut entry = Column::new().spacing(2).push(head).push(
            text(crate::ui::one_line(&p.argv.join(" "), 100))
                .size(style::TINY)
                .font(mono)
                .color(P.fg_dim)
                .wrapping(text::Wrapping::None),
        );
        if !p.last_line.is_empty() {
            entry = entry.push(
                text(crate::ui::one_line(&p.last_line, 100))
                    .size(style::TINY)
                    .font(mono)
                    .color(alpha(P.fg, 0.7))
                    .wrapping(text::Wrapping::None),
            );
        }
        if let Some(port) = p.port {
            match state.bridges.get(&port) {
                Some(bridge) => {
                    let url = bridge.url();
                    entry = entry.push(
                        row![
                            keys::press(
                                button(text(url.clone()).size(style::TINY).color(P.link))
                                    .padding(0)
                                    .style(style::quiet),
                                Message::Procs(Event::OpenUrl(url.clone())),
                            ),
                            small_button("Copy", Message::Link(url)),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                    );
                }
                None if running => {
                    entry = entry.push(
                        row![
                            text(format!("port {port} inside the jail, not exposed"))
                                .size(style::TINY)
                                .color(P.fg_dim),
                            small_button("Expose", Message::Procs(Event::Expose(id.clone(), port))),
                        ]
                        .spacing(6)
                        .align_y(Alignment::Center),
                    );
                }
                None => {}
            }
        }
        if let Some((_, content)) = state.log.as_ref().filter(|_| shown) {
            let session = id.clone();
            entry = entry.push(
                text_editor(content)
                    .height(220)
                    .padding(8)
                    .size(style::TINY)
                    .font(mono)
                    .style(style::reader)
                    .on_action(move |a| Message::Procs(Event::LogText(session.clone(), a)))
                    .key_binding(crate::ui::reading_keys),
            );
        }
        rows = rows.push(entry);
    }
    if state.list.iter().filter(|p| procs::running(p)).count() > 1 {
        rows = rows.push(small_button(
            "Stop all",
            Message::Procs(Event::Stop(id.clone(), String::new())),
        ));
    }
    crate::ui::ruled(
        2.0,
        P.fg_dim,
        container(scrollable(rows).style(style::feed_scroll))
            .padding([6, 10])
            .max_height(420)
            .width(Fill)
            .style(style::card_right(P.surface)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(name: &str, state: &str, port: Option<u16>) -> Process {
        Process {
            name: name.into(),
            argv: vec!["serve".into()],
            cwd: sill_proto::sessions::HostPath::from_path(std::path::Path::new("/tmp")),
            port,
            pid: 1,
            started: "0".into(),
            state: state.into(),
            exit_code: None,
            last_line: String::new(),
            kept: false,
            location: "jail".into(),
        }
    }

    #[test]
    fn a_delayed_process_read_cannot_populate_a_reopened_view() {
        let (mut app, _) = App::new(true, None);
        app.views.get_mut("demo").unwrap().processes.reading = Some(7);
        app.views.insert("demo".into(), SessionView::new("demo"));
        app.views.get_mut("demo").unwrap().processes.reading = Some(8);
        let _ = app.update(Message::Procs(Event::Read(
            "demo".into(),
            7,
            Ok(vec![process("old", "running", Some(3000))]),
            Some(vec![]),
            Default::default(),
        )));
        let state = &app.views["demo"].processes;
        assert!(state.list.is_empty());
        assert!(state.questions.is_empty());
        assert_eq!(state.reading, Some(8));
    }

    #[test]
    fn a_delayed_process_read_cannot_expose_a_stopped_sessions_port() {
        let (mut app, _) = App::new(true, None);
        app.demo = false;
        app.views.get_mut("demo").unwrap().processes.reading = Some(7);
        let _ = app.update(Message::Procs(Event::Read(
            "demo".into(),
            7,
            Ok(vec![process("old", "running", Some(3000))]),
            Some(vec!["3000".into()]),
            Default::default(),
        )));
        let state = &app.views["demo"].processes;
        assert!(state.list.is_empty());
        assert!(state.bridges.is_empty());
        assert_eq!(state.reading, None);
    }

    #[test]
    fn an_action_completion_does_not_clear_an_outstanding_poll() {
        let (mut app, _) = App::new(true, None);
        app.views.get_mut("demo").unwrap().processes.reading = Some(7);
        let _ = app.update(Message::Procs(Event::Done(
            "demo".into(),
            Outcome::Done(json!({})),
        )));
        assert_eq!(app.views["demo"].processes.reading, Some(7));
        assert!(app.views["demo"].processes.refresh_pending);
        // Its pre-action list must not bring the stopped process back into the UI.
        let _ = app.update(Message::Procs(Event::Read(
            "demo".into(),
            7,
            Ok(vec![process("old", "running", Some(3000))]),
            Some(vec![]),
            Default::default(),
        )));
        assert!(app.views["demo"].processes.list.is_empty());
        assert_eq!(app.views["demo"].processes.reading, None);
    }

    /// What runs in the jail (docs/jail-processes.md): the chip counts commands and services
    /// apart, the panel lists them in their own sections, and a command can be stopped.
    #[test]
    fn the_jails_commands_and_services_are_shown_apart() {
        let (mut app, _) = App::new(true, None);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let jail = JailProcesses {
            commands: vec![JailProcess {
                pid: 10,
                argv: vec!["bash".into(), "-c".into(), "cargo build".into()],
                started_ms: now - 125_000,
                count: 3,
                name: None,
                port: None,
            }],
            services: vec![JailProcess {
                pid: 30,
                argv: vec!["/usr/bin/python3".into(), "-m".into(), "http.server".into()],
                started_ms: now - 5_000,
                count: 1,
                name: None,
                port: Some(8000),
            }],
        };
        app.views.get_mut("demo").unwrap().processes.reading = Some(7);
        let _ = app.update(Message::Procs(Event::Read("demo".into(), 7, Ok(vec![]), None, jail)));
        let _ = app.update(Message::Procs(Event::Toggle("demo".into())));
        let mut ui = crate::tests::simulator(&app, (720.0, 860.0));
        ui.find("RUNNING NOW").unwrap();
        ui.find("cargo build").unwrap();
        ui.find("2m · 3 processes").unwrap();
        ui.find("SERVICES").unwrap();
        ui.find("python3 -m http.server").unwrap();
        ui.click("Stop").unwrap();
        assert!(ui.into_messages().any(|m| matches!(m, Message::Procs(Event::StopCommand(id, 10)) if id == "demo")));
        assert_eq!(command_name(&["/bin/sleep".into(), "300".into()]), "sleep 300");
        assert_eq!((elapsed(45_000), elapsed(3_900_000)), ("45s".into(), "1h 05m".into()));
    }

    #[test]
    fn an_old_log_read_cannot_replace_a_reopened_log() {
        let (mut app, _) = App::new(true, None);
        let state = &mut app.views.get_mut("demo").unwrap().processes;
        state.log = Some(("web".into(), text_editor::Content::with_text("current")));
        state.log_reading = Some(8);
        let _ = app.update(Message::Procs(Event::LogRead(
            "demo".into(),
            "web".into(),
            7,
            Ok("old".into()),
        )));
        let state = &app.views["demo"].processes;
        assert_eq!(state.log.as_ref().unwrap().1.text(), "current");
        assert_eq!(state.log_reading, Some(8));
        let _ = app.update(Message::Procs(Event::LogRead(
            "demo".into(),
            "web".into(),
            8,
            Ok("new".into()),
        )));
        assert_eq!(
            app.views["demo"].processes.log.as_ref().unwrap().1.text(),
            "new"
        );
    }

    #[test]
    fn an_unchanged_log_refresh_preserves_the_reading_position() {
        let (mut app, _) = App::new(true, None);
        let state = &mut app.views.get_mut("demo").unwrap().processes;
        let mut content = text_editor::Content::with_text("first\nlast");
        content.perform(text_editor::Action::Move(
            text_editor::Motion::DocumentStart,
        ));
        let cursor = content.cursor();
        state.log = Some(("web".into(), content));
        state.log_reading = Some(8);
        let _ = app.update(Message::Procs(Event::LogRead(
            "demo".into(),
            "web".into(),
            8,
            Ok("first\nlast\n".into()),
        )));
        assert_eq!(
            app.views["demo"].processes.log.as_ref().unwrap().1.cursor(),
            cursor
        );
    }

    #[test]
    fn repeated_exposure_reuses_the_existing_listener() {
        let (mut app, _) = App::new(true, None);
        let (client, _daemon) = crate::tests::daemon();
        app.actions = Some(sill_client::actions::Actions::new(crate::tests::connector(
            vec![client],
        )));
        let open: procs::Opener = std::sync::Arc::new(|| anyhow::bail!("not in this test"));
        let bridge = procs::expose(0, open).unwrap();
        let host_port = bridge.host_port;
        app.views
            .get_mut("demo")
            .unwrap()
            .processes
            .bridges
            .insert(3000, bridge);
        let _ = app.expose_port("demo", 3000, "web");
        assert_eq!(
            app.views["demo"].processes.bridges[&3000].host_port,
            host_port
        );
    }

    #[test]
    fn listing_a_stopped_session_closes_bridges_and_invalidates_polls() {
        let (mut app, _) = App::new(true, None);
        let state = &mut app.views.get_mut("demo").unwrap().processes;
        let open: procs::Opener = std::sync::Arc::new(|| anyhow::bail!("not in this test"));
        state.bridges.insert(3000, procs::expose(0, open).unwrap());
        state.reading = Some(7);
        let session = serde_json::from_value(json!({
            "session_id": "demo", "agent": "claude", "title": "demo", "title_kind": "",
            "created_at": "1", "last_used": "1", "ended_at": null, "running": false,
            "policy": "default", "auto_allow": false, "memory_scope": "",
            "summary": "", "summary_kind": "", "ghost": false, "pins": [], "settings": {},
            "follows": null, "compacted": "0"
        }))
        .unwrap();
        let _ = app.listed(Ok(vec![session]));
        let state = &app.views["demo"].processes;
        assert!(state.bridges.is_empty());
        assert_eq!(state.reading, None);
    }

    #[test]
    fn a_new_port_is_exposed_by_policy_or_asked_about_once() {
        let mut state = Procs::default();
        let list = vec![
            process("web", "running", Some(3000)),
            process("api", "running", Some(9000)),
        ];
        assert!(undecided(&state.known(), &list));
        let specs = vec!["3000-3999".to_string()];
        let expose = state.read(list.clone(), Some(&specs));
        assert_eq!(expose, vec![(3000, "web".to_string())]);
        assert_eq!(state.questions, vec![(9000, "api".to_string())]);
        // asked once: a later read neither asks again nor needs the policy for it
        state.questions.clear();
        assert!(
            undecided(&state.known(), &list),
            "3000 is not bridged in this test"
        );
        let expose = state.read(list.clone(), Some(&specs));
        assert_eq!(expose, vec![(3000, "web".to_string())]);
        assert!(state.questions.is_empty());
        // the person said yes later: exposed from then on
        state.approved.insert(9000);
        assert_eq!(state.read(list.clone(), Some(&specs)).len(), 2);
        // without the policy only what the person approved goes
        assert_eq!(state.read(list, None), vec![(9000, "api".to_string())]);
    }

    #[test]
    fn without_the_policy_nothing_is_exposed_or_asked() {
        let mut state = Procs::default();
        let list = vec![process("web", "running", Some(3000))];
        assert!(state.read(list.clone(), None).is_empty());
        assert!(state.questions.is_empty());
        assert!(
            undecided(&state.known(), &list),
            "asked when the policy can be read"
        );
    }

    #[test]
    fn an_ended_process_takes_its_question_and_log_with_it() {
        let mut state = Procs::default();
        state.read(vec![process("api", "running", Some(9000))], Some(&[]));
        assert_eq!(state.questions.len(), 1);
        state.log = Some(("api".into(), text_editor::Content::new()));
        state.read(vec![process("api", "stopped", Some(9000))], Some(&[]));
        assert!(state.questions.is_empty());
        assert!(state.log.is_some(), "an ended process keeps its log");
        state.read(vec![], None);
        assert!(state.log.is_none());
    }

    /// Stop all asks for each running process by name: the daemon refuses an empty name.
    #[test]
    fn stop_all_stops_each_running_process_by_name() {
        use crate::tests::{connector, daemon, settle_task};
        let (client, mut daemon) = daemon();
        let (mut app, _) = App::new(true, None);
        app.demo = false;
        app.actions = Some(sill_client::actions::Actions::new(connector(vec![client])));
        app.views.get_mut("demo").unwrap().processes.list = vec![
            process("api", "running", None),
            process("old", "exited", None),
            process("web", "running", Some(3000)),
        ];
        let fake = std::thread::spawn(move || {
            let mut stopped = vec![];
            for _ in 0..2 {
                let stop = daemon.expect("_sill/procs.stop");
                stopped.push(stop["params"]["name"].as_str().unwrap().to_string());
                daemon.reply(&stop, json!({}));
            }
            stopped
        });
        let task = app.update(Message::Procs(Event::Stop("demo".into(), String::new())));
        settle_task(&mut app, task);
        assert_eq!(fake.join().unwrap(), vec!["api", "web"]);
    }

    /// A port bridged for a running process closes when the process ends.
    #[test]
    fn a_bridge_goes_when_its_process_ends() {
        let mut state = Procs::default();
        let open: procs::Opener = std::sync::Arc::new(|| anyhow::bail!("not in this test"));
        state.bridges.insert(3000, procs::expose(0, open).unwrap());
        state.read(vec![process("web", "running", Some(3000))], None);
        assert_eq!(state.bridges.len(), 1);
        state.read(vec![process("web", "exited", Some(3000))], None);
        assert!(state.bridges.is_empty());
    }
}
