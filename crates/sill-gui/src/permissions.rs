//! A session's Permissions (docs/permission-modes.md, Controls): a shield in the composer's bar
//! opens a compact panel above the composer with two groups, **Tool approvals** (Sill's
//! auto-allow of the agent's permission requests, which never widens jail access) and **Jail
//! access** (the daemon's capability states: public web, test desktop, private bus, GPU, and the
//! hosts approved for this session), and a separate **Host actions** group. While the session has
//! more than its policy's baseline, a "Jailed · expanded access" caption stays over the composer.
//!
//! The daemon decides everything shown: the next build's preview and, for a running jail, its
//! launch snapshot. A change goes to the daemon and the panel reads both again; what differs
//! between them is marked as applying after a restart, with a Restart now. Choices need the
//! frontend-declared `sill.permission_modes` capability, which this window offers because it
//! shows this state. Desktop development and Host control have no backend yet: shown, never
//! offered.
use iced::widget::{Column, button, column, container, row, rule, space, text, tooltip};
use iced::{Alignment, Element, Fill, Task};
use sill_client::actions::Outcome;
use sill_proto::admin::{PermissionState, PermissionsChanged, PolicySnapshot, PolicyView};
use sill_proto::sessions::Session;

use crate::style::{self, P, alpha};
use crate::{App, Message, SessionView, blocking, icons, json, keys};

/// The capability this window declares (docs/protocol.md): it shows permission state.
pub const CAPABILITY: &str = "sill.permission_modes";

/// How long a note on a change stays.
const NOTICE: std::time::Duration = std::time::Duration::from_secs(8);

/// What a capability is called here, and what it means, in the panel's order.
const ROWS: &[(&str, &str, &str)] = &[
    (
        "public_web",
        "Public web access",
        "Every public HTTP(S) host through the session's proxy. Local and LAN addresses stay blocked.",
    ),
    (
        "test_desktop",
        "Test desktop",
        "An isolated desktop for running GUI applications, never the real one.",
    ),
    (
        "private_bus",
        "Private D-Bus",
        "A session bus of the jail's own, for test applications and services.",
    ),
    ("gpu", "GPU rendering", "A selected render device for the jail."),
];

#[derive(Debug, Clone)]
pub enum Event {
    /// Show or hide the panel.
    Toggle(String),
    /// The next build and, for a running jail, its launch (a read is numbered: only the latest
    /// one counts), and the agent's name when it was asked for.
    Read(String, u64, Result<Views, String>),
    /// A capability choice: on, off, or the policy default.
    Set(String, String, Option<bool>),
    Restore(String),
    SaveHost(String, String),
    AutoAllow(String, bool),
    Restart(String),
    Done(String, Change, Outcome),
    NoticeExpired(String, u64),
}

#[derive(Debug, Clone)]
pub struct Views {
    pub preview: PolicyView,
    /// The running jail's launch snapshot (none when not running, or never recorded).
    pub launch: Option<PolicyView>,
    pub agent_name: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Change {
    Choice(String, Option<bool>),
    Restore,
    SaveHost(String),
    AutoAllow(bool),
    Restart,
}

/// A session's permission state in this window.
#[derive(Default)]
pub struct Perms {
    pub open: bool,
    pub views: Option<Views>,
    pub error: Option<String>,
    reading: Option<u64>,
    /// A read finished while a change was on its way: read again after it.
    reread: bool,
    /// A change is on its way: the buttons wait.
    pub busy: bool,
    /// The last change said the running jail needs a restart (until the next read shows it).
    restart_required: bool,
    agent_name: Option<String>,
    /// A note on the last change: its number, the text, whether it is a failure.
    notice: Option<(u64, String, bool)>,
}

fn state<'a>(view: &'a PolicyView, capability: &str) -> Option<&'a PermissionState> {
    view.permissions.iter().find(|p| p.capability == capability)
}

fn applied_hosts(view: &PolicyView) -> impl Iterator<Item = &str> {
    view.session_hosts.iter().filter(|h| h.applied).map(|h| h.host.as_str())
}

impl Perms {
    /// What the session has beyond its policy's baseline: of the running jail (its launch, and
    /// hosts approved since, which reach its proxy live), else of the next build.
    pub fn expanded(&self, running: bool) -> Vec<String> {
        let Some(views) = &self.views else {
            return vec![];
        };
        let current = match (&views.launch, running) {
            (Some(launch), true) => launch,
            _ => &views.preview,
        };
        let mut out = vec![];
        for (capability, short) in [
            ("test_desktop", "Test desktop"),
            ("private_bus", "Private D-Bus"),
            ("gpu", "GPU"),
            ("public_web", "Public web"),
        ] {
            if state(current, capability).is_some_and(|s| s.effective) {
                out.push(short.to_string());
            }
        }
        let mut hosts: Vec<&str> = applied_hosts(current).collect();
        if running {
            hosts.extend(applied_hosts(&views.preview));
        }
        hosts.sort_unstable();
        hosts.dedup();
        match hosts.len() {
            0 => {}
            1 => out.push("1 approved host".into()),
            n => out.push(format!("{n} approved hosts")),
        }
        out
    }

    /// The running jail was built with other permissions than the next build would have: a
    /// capability's state, or a host it had that is no longer approved (approved ones reach it
    /// live).
    pub fn restart_pending(&self, running: bool) -> bool {
        if !running {
            return false;
        }
        if self.restart_required {
            return true;
        }
        let Some(Views {
            preview,
            launch: Some(launch),
            ..
        }) = &self.views
        else {
            return false;
        };
        let differs = ROWS.iter().any(|(capability, _, _)| {
            state(launch, capability).map(|s| s.effective)
                != state(preview, capability).map(|s| s.effective)
        });
        let now: Vec<&str> = applied_hosts(preview).collect();
        differs || applied_hosts(launch).any(|h| !now.contains(&h))
    }

    fn note(&mut self, serial: u64, text: String, failed: bool) {
        self.notice = Some((serial, text, failed));
    }
}

impl App {
    fn session(&self, id: &str) -> Option<&Session> {
        self.all_sessions.iter().find(|s| s.session_id == id)
    }

    fn running(&self, id: &str) -> bool {
        self.session(id)
            .is_some_and(|s| s.running && s.ended_at.is_none())
    }

    pub(crate) fn permissions_event(&mut self, event: Event) -> Task<Message> {
        match event {
            Event::Toggle(id) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                view.permissions.open = !view.permissions.open;
                if view.permissions.open {
                    return self.read_permissions(&id);
                }
            }
            Event::Read(id, serial, result) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                let perms = &mut view.permissions;
                if perms.reading != Some(serial) {
                    return Task::none();
                }
                perms.reading = None;
                match result {
                    Ok(mut views) => {
                        if let Some(name) = views.agent_name.take() {
                            perms.agent_name = Some(name);
                        }
                        views.agent_name = None;
                        perms.views = Some(views);
                        perms.error = None;
                        perms.restart_required = false;
                    }
                    Err(error) => perms.error = Some(error),
                }
                if std::mem::take(&mut perms.reread) {
                    return self.read_permissions(&id);
                }
            }
            Event::Set(id, capability, enabled) => {
                let params = json!({"session_id": id, "capability": capability, "enabled": enabled});
                return self.permission_change(
                    id,
                    Change::Choice(capability, enabled),
                    "_sill/permissions.set",
                    params,
                );
            }
            Event::Restore(id) => {
                let params = json!({"session_id": id});
                return self.permission_change(
                    id,
                    Change::Restore,
                    "_sill/permissions.restore_defaults",
                    params,
                );
            }
            Event::SaveHost(id, host) => {
                let params = json!({"session_id": id, "host": host});
                return self.permission_change(
                    id,
                    Change::SaveHost(host),
                    "_sill/policy.save_host",
                    params,
                );
            }
            Event::AutoAllow(id, enabled) => {
                let params = json!({"session_id": id,
                    "change": {"type": "auto_allow", "enabled": enabled}});
                return self.permission_change(
                    id,
                    Change::AutoAllow(enabled),
                    "_sill/sessions.update",
                    params,
                );
            }
            Event::Restart(id) => {
                let (Some(actions), Some(view)) = (self.actions.clone(), self.views.get_mut(&id))
                else {
                    return Task::none();
                };
                if view.permissions.busy {
                    return Task::none();
                }
                view.permissions.busy = true;
                let session = id.clone();
                return blocking(
                    move || {
                        let key = json!({"session_id": session});
                        let stopped = actions.call("_sill/sessions.stop", key.clone());
                        if !stopped.is_done() {
                            return stopped;
                        }
                        actions.call("_sill/sessions.start", key)
                    },
                    move |outcome| Message::Perms(Event::Done(id, Change::Restart, outcome)),
                );
            }
            Event::Done(id, change, outcome) => return self.permission_done(id, change, outcome),
            Event::NoticeExpired(id, serial) => {
                if let Some(view) = self.views.get_mut(&id)
                    && view
                        .permissions
                        .notice
                        .as_ref()
                        .is_some_and(|(n, _, _)| *n == serial)
                {
                    view.permissions.notice = None;
                }
            }
        }
        Task::none()
    }

    /// The session's next build and, while it runs, its launch.
    pub(crate) fn read_permissions(&mut self, id: &str) -> Task<Message> {
        if self.demo {
            return Task::none();
        }
        let running = self.running(id);
        let agent = self.session(id).map(|s| s.agent.clone());
        let (Some(actions), Some(view)) = (self.actions.clone(), self.views.get_mut(id)) else {
            return Task::none();
        };
        let perms = &mut view.permissions;
        if perms.busy || perms.reading.is_some() {
            perms.reread = true;
            return Task::none();
        }
        self.permission_serial += 1;
        let serial = self.permission_serial;
        perms.reading = Some(serial);
        let want_name = perms.agent_name.is_none();
        let (session, id) = (id.to_string(), id.to_string());
        blocking(
            move || {
                let key = json!({"session_id": session});
                let preview: PolicyView = actions
                    .read("_sill/policy.preview", key.clone())
                    .map_err(|e| format!("{e:#}"))?;
                let launch = if running {
                    actions
                        .read::<PolicySnapshot>("_sill/policy.snapshot", key)
                        .map_err(|e| format!("{e:#}"))?
                        .snapshot
                } else {
                    None
                };
                let agent_name = match agent.filter(|_| want_name) {
                    Some(agent) => actions
                        .read::<serde_json::Value>("_sill/agents.get", json!({"agent_id": agent}))
                        .ok()
                        .and_then(|a| a["name"].as_str().map(str::to_string)),
                    None => None,
                };
                Ok(Views {
                    preview,
                    launch,
                    agent_name,
                })
            },
            move |result| Message::Perms(Event::Read(id, serial, result)),
        )
    }

    fn permission_change(
        &mut self,
        id: String,
        change: Change,
        method: &'static str,
        params: serde_json::Value,
    ) -> Task<Message> {
        let (Some(actions), Some(view)) = (self.actions.clone(), self.views.get_mut(&id)) else {
            return Task::none();
        };
        if view.permissions.busy {
            return Task::none();
        }
        view.permissions.busy = true;
        let needs = !matches!(change, Change::AutoAllow(_));
        blocking(
            move || {
                if needs {
                    actions.call_needing(CAPABILITY, method, params)
                } else {
                    actions.call(method, params)
                }
            },
            move |outcome| Message::Perms(Event::Done(id, change, outcome)),
        )
    }

    fn permission_done(&mut self, id: String, change: Change, outcome: Outcome) -> Task<Message> {
        let running = self.running(&id);
        if outcome.is_done()
            && let Change::AutoAllow(enabled) = change
            && let Some(session) = self.all_sessions.iter_mut().find(|s| s.session_id == id)
        {
            session.auto_allow = enabled;
        }
        let agent = self
            .views
            .get(&id)
            .and_then(|v| v.permissions.agent_name.clone())
            .unwrap_or_else(|| "the agent".into());
        let Some(view) = self.views.get_mut(&id) else {
            return Task::none();
        };
        let perms = &mut view.permissions;
        perms.busy = false;
        let mut restart_required = false;
        if let Outcome::Done(value) = &outcome
            && let Ok(changed) = serde_json::from_value::<PermissionsChanged>(value.clone())
        {
            restart_required = changed.restart_required && running;
            if let Some(views) = &mut perms.views {
                views.preview = changed.preview;
            }
            perms.restart_required = restart_required;
        }
        let later = if restart_required {
            " It applies when the session restarts."
        } else {
            ""
        };
        let (said, failed) = match outcome.problem() {
            Some(problem) => (problem.to_string(), true),
            None => (
                match &change {
                    Change::Choice(capability, enabled) => {
                        let name = ROWS
                            .iter()
                            .find(|(c, _, _)| c == capability)
                            .map_or(capability.as_str(), |(_, name, _)| name);
                        match enabled {
                            Some(true) => format!("{name} on for this session.{later}"),
                            Some(false) => format!("{name} off for this session.{later}"),
                            None => format!("{name} follows the policy again.{later}"),
                        }
                    }
                    Change::Restore => format!(
                        "Defaults restored: added permissions and approved hosts removed; the policy and pins stay.{later}"
                    ),
                    Change::SaveHost(host) => {
                        format!("{host} saved for {agent}'s future sessions.")
                    }
                    Change::AutoAllow(true) => {
                        "Auto-allow on: Sill answers the agent's requests \"allow once\".".into()
                    }
                    Change::AutoAllow(false) => {
                        "Auto-allow off: the agent's requests wait for you.".into()
                    }
                    Change::Restart => "The session started again with its new permissions.".into(),
                },
                false,
            ),
        };
        self.permission_serial += 1;
        let serial = self.permission_serial;
        if let Some(view) = self.views.get_mut(&id) {
            view.permissions.note(serial, said, failed);
        }
        if matches!(change, Change::Restart) && outcome.is_done() {
            self.reattach(&id);
        }
        let session = id.clone();
        // (tests read the note; its expiry is `NoticeExpired`'s own)
        let expire = if cfg!(test) {
            Task::none()
        } else {
            blocking(
                move || std::thread::sleep(NOTICE),
                move |()| Message::Perms(Event::NoticeExpired(session, serial)),
            )
        };
        // nothing changed when it was refused or never sent: no need to read again
        if matches!(outcome, Outcome::Refused { .. } | Outcome::NotSent(_)) {
            return expire;
        }
        Task::batch([expire, self.list(), self.read_permissions(&id)])
    }
}

// --- the view -------------------------------------------------------------------------------

/// The composer bar's shield: warning-coloured while the session has expanded access.
pub fn indicator<'a>(view: &'a SessionView, session: Option<&Session>) -> Element<'a, Message> {
    let running = session.is_some_and(|s| s.running);
    let expanded = view.permissions.expanded(running);
    let color = if !expanded.is_empty() {
        P.warning
    } else if view.permissions.open {
        P.accent
    } else {
        P.fg_dim
    };
    let pending = view.permissions.restart_pending(running) || session.is_some_and(|s| s.policy_changed);
    let mut tip = if expanded.is_empty() {
        "Permissions".to_string()
    } else {
        format!("Permissions · expanded access: {}", expanded.join(" · "))
    };
    if pending {
        tip.push_str(" · changes apply after a restart");
    }
    let shield = keys::press_maybe(
        crate::ui::bar_button(icons::glyph("shield", crate::ui::BAR_ICON, color)).style(style::quiet),
        session.map(|_| Message::Perms(Event::Toggle(view.id.clone()))),
    );
    crate::ui::bar_tip(container(shield).id("permissions-button"), tip)
}

/// "Jailed · expanded access" over the composer, in the host terminal's warning treatment but
/// naming the jail: the agent is still inside it. Without expanded access but with changes
/// waiting for a restart, a quiet line says so (the running jail is what counts).
pub fn caption<'a>(view: &'a SessionView, session: Option<&Session>) -> Option<Element<'a, Message>> {
    let running = session.is_some_and(|s| s.running);
    let expanded = view.permissions.expanded(running);
    let pending = view.permissions.restart_pending(running) || session.is_some_and(|s| s.policy_changed);
    let (mut said, color) = match (expanded.is_empty(), pending) {
        (true, false) => return None,
        (true, true) => ("Default · jailed".to_string(), P.fg_dim),
        (false, _) => (format!("Jailed · expanded access · {}", expanded.join(" · ")), P.warning),
    };
    if pending {
        said.push_str(" · permission changes apply after a restart");
    }
    Some(
        column![
            rule::horizontal(1).style(style::rule_style(if expanded.is_empty() { P.border } else { P.warning })),
            container(
                text(said)
                    .size(style::TINY)
                    .color(color)
                    .wrapping(text::Wrapping::None)
            )
            .width(Fill)
            .clip(true)
            .padding([2, 12]),
        ]
        .into(),
    )
}

fn small<'a>(label: &'a str, message: Option<Message>) -> Element<'a, Message> {
    keys::press_maybe(
        button(text(label).size(style::TINY))
            .padding([2, 8])
            .style(style::quiet),
        message,
    )
}

fn group<'a>(title: &'a str) -> Element<'a, Message> {
    text(title.to_uppercase())
        .size(style::TINY)
        .color(alpha(P.fg, 0.55))
        .into()
}

fn hint<'a>(said: impl text::IntoFragment<'a>) -> Element<'a, Message> {
    text(said).size(style::TINY).color(P.fg_dim).into()
}

/// One capability: its name and meaning, its state for the next build (and when the running
/// jail differs, that it applies after a restart), why it cannot be on, and the choice.
fn capability_row<'a>(
    id: &'a str,
    capability: &'a str,
    name: &'a str,
    meaning: &'a str,
    views: &'a Views,
    running: bool,
    can_change: bool,
) -> Element<'a, Message> {
    let Some(s) = state(&views.preview, capability) else {
        return space().height(0).into();
    };
    let mut status = if !s.allowed {
        "Forbidden".to_string()
    } else if s.effective {
        format!(
            "On · {}",
            if s.source == "session" {
                "this session"
            } else {
                "policy"
            }
        )
    } else if !s.available {
        if s.default_enabled || s.requested == Some(true) {
            "Requested · unavailable".to_string()
        } else {
            "Unavailable".to_string()
        }
    } else if s.blocked {
        "Blocked".to_string()
    } else {
        "Off".to_string()
    };
    if running
        && let Some(launch) = views.launch.as_ref().and_then(|l| state(l, capability))
        && launch.effective != s.effective
    {
        status.push_str(if s.effective {
            " · after restart"
        } else {
            " · until restart"
        });
    }
    let color = if s.effective {
        P.warning
    } else if !s.allowed || !s.available || s.blocked {
        P.fg_dim
    } else {
        P.fg
    };
    let mut actions = row![].spacing(4).align_y(Alignment::Center);
    if can_change && s.allowed && s.available {
        let (label, enabled) = if s.effective {
            ("Turn off", Some(false))
        } else {
            ("Turn on", Some(true))
        };
        actions = actions.push(small(
            label,
            Some(Message::Perms(Event::Set(id.into(), capability.into(), enabled))),
        ));
    }
    if can_change && s.requested.is_some() {
        actions = actions.push(small(
            "Policy default",
            Some(Message::Perms(Event::Set(id.into(), capability.into(), None))),
        ));
    }
    let mut block = Column::new().spacing(1).push(
        row![
            text(name).size(style::SMALL).width(150),
            text(status).size(style::SMALL).color(color).width(Fill),
            actions,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );
    let why = if !s.allowed || !s.available || s.blocked {
        format!("{meaning} {}.", capitalised(&s.reason))
    } else {
        meaning.to_string()
    };
    block = block.push(hint(why));
    block.into()
}

/// A named policy as it is; a policy file by its name, without the directories.
fn policy_name(name: &str) -> &str {
    std::path::Path::new(name)
        .file_stem()
        .and_then(|n| n.to_str())
        .filter(|_| name.contains('/'))
        .unwrap_or(name)
}

fn capitalised(reason: &str) -> String {
    let mut chars = reason.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
        .trim_end_matches('.')
        .to_string()
}

/// The panel over the composer, when open.
pub fn panel<'a>(view: &'a SessionView, session: Option<&'a Session>) -> Element<'a, Message> {
    let perms = &view.permissions;
    if !perms.open || session.is_none() {
        return space().height(0).into();
    }
    let session = session.unwrap();
    let id = view.id.as_str();
    let running = session.running;
    let ghost = session.ghost;
    let can_change = !ghost && !perms.busy;
    let expanded = perms.expanded(running);
    // Desktop development is the test desktop and private bus; anything else stays Default
    let desktop = perms.views.as_ref().is_some_and(|v| {
        ["test_desktop", "private_bus"]
            .iter()
            .any(|c| state(&v.preview, c).is_some_and(|s| s.effective))
    });
    let mode = match (desktop, expanded.is_empty()) {
        (true, _) => "Desktop development",
        (false, true) => "Default",
        (false, false) => "Default · expanded access",
    };
    let mut content = Column::new().spacing(8);
    content = content.push(
        row![
            text("Permissions").size(style::SMALL).font(iced::Font {
                weight: iced::font::Weight::Bold,
                ..iced::Font::with_name(crate::fonts::SANS_FAMILY)
            }),
            text(mode)
                .size(style::TINY)
                .color(if expanded.is_empty() { P.fg_dim } else { P.warning })
                .width(Fill),
            small("Close", Some(Message::Perms(Event::Toggle(id.into())))),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    );
    if ghost {
        content = content.push(hint(
            "Ghost session: Default only. Permission choices and saved defaults are refused.",
        ));
    }

    // Tool approvals: Sill's answer to the agent's own requests, never jail access
    content = content.push(group("Tool approvals"));
    content = content.push(
        column![
            row![
                text("Auto-allow requests").size(style::SMALL).width(150),
                text(if session.auto_allow { "On" } else { "Off" })
                    .size(style::SMALL)
                    .color(if session.auto_allow { P.warning } else { P.fg })
                    .width(Fill),
                small(
                    if session.auto_allow { "Turn off" } else { "Turn on" },
                    (!perms.busy).then(|| {
                        Message::Perms(Event::AutoAllow(id.into(), !session.auto_allow))
                    }),
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            hint("Sill answers the agent's permission requests \"allow once\"; each is still listed. Jail access does not change."),
        ]
        .spacing(1),
    );

    match &perms.views {
        None => {
            content = content.push(hint(match &perms.error {
                Some(error) => format!("Permissions could not be read: {error}"),
                None => "Reading permissions…".into(),
            }));
        }
        Some(views) => {
            content = content.push(
                row![
                    group("Jail access"),
                    space().width(Fill),
                    hint(format!("policy {}", policy_name(&views.preview.name))),
                ]
                .align_y(Alignment::Center),
            );
            for diagnostic in &views.preview.diagnostics {
                content = content.push(text(diagnostic).size(style::TINY).color(P.error));
            }
            for (capability, name, meaning) in ROWS {
                content = content.push(capability_row(
                    id, capability, name, meaning, views, running, can_change,
                ));
                if *capability == "public_web" {
                    content = content.push(hosts(id, views, perms, can_change));
                }
            }
            content = content.push(group("Host actions"));
            content = content.push(
                column![
                    row![
                        text("Host control").size(style::SMALL).width(150),
                        text("Not available").size(style::SMALL).color(P.fg_dim).width(Fill),
                    ]
                    .spacing(8),
                    hint("Real desktop and host processes, outside the jail. Needs an explicit activation that is not implemented yet; a policy or a choice here never turns it on."),
                ]
                .spacing(1),
            );
            let restorable = views
                .preview
                .permissions
                .iter()
                .any(|p| p.requested.is_some())
                || !views.preview.session_hosts.is_empty();
            let develop = tooltip(
                small("Apply development permissions", None),
                container(
                    text("Desktop development needs the test desktop and private D-Bus backends (managed Weston); not implemented yet.")
                        .size(style::SMALL),
                )
                .padding([4, 8])
                .max_width(360)
                .style(style::fill(P.code_bg)),
                tooltip::Position::Top,
            );
            content = content.push(
                row![
                    small(
                        "Restore defaults",
                        (can_change && restorable).then(|| Message::Perms(Event::Restore(id.into()))),
                    ),
                    develop,
                ]
                .spacing(8),
            );
        }
    }
    if perms.restart_pending(running) || session.policy_changed {
        let said = if view.conversation.busy() {
            "The running jail was built with other permissions; they apply when the session restarts. Restarting stops the agent's current turn."
        } else {
            "The running jail was built with other permissions; they apply when the session restarts."
        };
        content = content.push(
            row![
                text(said)
                    .size(style::TINY)
                    .color(P.warning)
                    .width(Fill),
                small(
                    "Restart now",
                    (!perms.busy).then(|| Message::Perms(Event::Restart(id.into()))),
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    if let Some((_, note, failed)) = &perms.notice {
        content = content.push(text(note).size(style::TINY).color(if *failed {
            P.error
        } else {
            alpha(P.fg, 0.7)
        }));
    }
    column![
        container(content)
            .padding([8, 12])
            .width(Fill)
            .style(style::fill(alpha(P.accent, 0.05))),
        rule::horizontal(1).style(style::rule_style(P.border)),
        space().height(4),
    ]
    .into()
}

/// The hosts approved for this session: applied or blocked (and why), each with Save for future
/// sessions, which names the agent whose defaults it changes.
fn hosts<'a>(id: &'a str, views: &'a Views, perms: &'a Perms, can_change: bool) -> Element<'a, Message> {
    let agent = perms.agent_name.as_deref().unwrap_or("this agent");
    let mut list = Column::new().spacing(2);
    if views.preview.session_hosts.is_empty() {
        list = list.push(hint(
            "Hosts you Allow when the proxy refuses them are approved for this session only.",
        ));
    }
    for grant in &views.preview.session_hosts {
        let mut line = row![
            text(&grant.host)
                .size(style::SMALL)
                .color(if grant.applied { P.warning } else { P.fg_dim })
                .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if !grant.applied {
            line = line.push(hint("blocked"));
        }
        if can_change {
            line = line.push(tooltip(
                small(
                    "Save for future sessions",
                    Some(Message::Perms(Event::SaveHost(id.into(), grant.host.clone()))),
                ),
                container(
                    text(format!(
                        "Adds {} to {agent}'s network defaults: every new {agent} session may reach it. Existing sessions do not change.",
                        grant.host
                    ))
                    .size(style::SMALL),
                )
                .padding([4, 8])
                .max_width(360)
                .style(style::fill(P.code_bg)),
                tooltip::Position::Top,
            ));
        }
        list = list.push(line);
        if !grant.applied && !grant.reason.is_empty() {
            list = list.push(hint(capitalised(&grant.reason) + "."));
        }
    }
    row![
        text("Approved hosts").size(style::SMALL).width(150),
        container(list).width(Fill),
    ]
    .spacing(8)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{daemon, live_app, session_record, settle_task, simulator};
    use serde_json::{Value, json};

    fn grant(capability: &str, effective: bool, source: &str) -> Value {
        json!({"capability": capability, "allowed": true, "default_enabled": false,
               "requested": (source == "session").then_some(effective), "blocked": false,
               "source": source, "effective": effective, "available": capability == "public_web",
               "reason": if capability == "public_web" { "off" } else { "enforcement backend not implemented" }})
    }

    fn view(kind: &str, public_web: bool, hosts: Value) -> Value {
        json!({"view": kind, "session_id": "demo", "name": "default", "text": null,
               "fragment_ro": [], "fragment_masks": [], "sources": [], "suggestions": [],
               "fingerprint": null, "diagnostics": [],
               "permissions": [grant("test_desktop", false, "policy"), grant("private_bus", false, "policy"),
                               grant("gpu", false, "policy"),
                               grant("public_web", public_web, if public_web { "session" } else { "policy" }),
                               grant("host_control", false, "policy")],
               "session_hosts": hosts})
    }

    /// A client whose daemon granted the permission-modes capability, and that daemon's end.
    fn modes_client() -> (sill_proto::Client, crate::tests::Daemon) {
        let (client, mut daemon) = daemon();
        let initializing = std::thread::spawn(move || {
            client.initialize().unwrap();
            client
        });
        let request = daemon.expect("_sill/initialize");
        let answer = sill_proto::initialize::Initialized {
            protocol: sill_proto::initialize::PROTOCOL_ID.into(),
            daemon: sill_proto::initialize::Peer { name: "sill".into(), version: "test".into() },
            capabilities: [(CAPABILITY.to_string(), 1)].into(),
            limits: Default::default(),
        };
        daemon.reply(&request, serde_json::to_value(answer).unwrap());
        (initializing.join().unwrap(), daemon)
    }

    /// Opening the panel reads the next build, the running jail's launch and the agent's name.
    /// Public web chosen since the launch is expanded access that applies after a restart; the
    /// approved host already reaches the running proxy.
    #[test]
    fn the_panel_reads_both_views_and_shows_what_waits_for_a_restart() {
        let (client, mut daemon) = daemon();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let fake = std::thread::spawn(move || {
            let preview = daemon.expect("_sill/policy.preview");
            daemon.reply(&preview, view("preview", true,
                json!([{"host": "example.org", "applied": true, "reason": ""},
                       {"host": "blocked.example", "applied": false, "reason": "offline: no network"}])));
            let snapshot = daemon.expect("_sill/policy.snapshot");
            daemon.reply(&snapshot, json!({"snapshot": view("launch", false, json!([]))}));
            let agent = daemon.expect("_sill/agents.get");
            assert_eq!(agent["params"]["agent_id"], "claude");
            daemon.reply(&agent, json!({"id": "claude", "name": "Claude Code"}));
        });
        let task = app.update(Message::Perms(Event::Toggle("demo".into())));
        settle_task(&mut app, task);
        fake.join().unwrap();
        let perms = &app.views["demo"].permissions;
        assert!(perms.open && perms.error.is_none());
        assert_eq!(perms.expanded(true), vec!["1 approved host".to_string()], "the launch had no public web");
        assert!(perms.restart_pending(true));
        assert_eq!(perms.expanded(false), vec!["Public web".to_string(), "1 approved host".to_string()]);
        assert_eq!(perms.agent_name.as_deref(), Some("Claude Code"));
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("Jailed · expanded access · 1 approved host · permission changes apply after a restart").unwrap();
        ui.find("Default · expanded access").unwrap();
        ui.find("On · this session · after restart").unwrap();
        ui.find("Restart now").unwrap();
        ui.find("blocked.example").unwrap();
        ui.find("Offline: no network.").unwrap();
        ui.find("Requested · unavailable").is_err().then_some(()).unwrap();
        ui.find("Not available").unwrap();
    }

    /// A choice goes to the daemon only over a connection that negotiated the capability; its
    /// answer says whether the running jail needs a restart, and the panel says so.
    #[test]
    fn choices_need_the_capability_and_say_when_a_restart_applies_them() {
        let (client, _quiet) = daemon();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let task = app.update(Message::Perms(Event::Set("demo".into(), "public_web".into(), Some(true))));
        settle_task(&mut app, task);
        let (_, said, failed) = app.views["demo"].permissions.notice.clone().expect("refused here");
        assert!(failed && said.contains(CAPABILITY), "{said}");

        let (client, mut daemon) = modes_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let fake = std::thread::spawn(move || {
            let set = daemon.expect("_sill/permissions.set");
            assert_eq!(set["params"], json!({"session_id": "demo", "capability": "public_web", "enabled": true}));
            daemon.reply(&set, json!({"preview": view("preview", true, json!([])), "restart_required": true}));
            // then the list and a fresh read, in either order; the read is refused here
            for _ in 0..2 {
                let request = daemon.next();
                match request["method"].as_str().unwrap() {
                    "_sill/sessions.list" => daemon.reply(
                        &request,
                        json!({"sessions": [session_record("demo", "demo", false, false)], "next": null}),
                    ),
                    "_sill/policy.preview" => daemon.refuse(&request, "unavailable", "later"),
                    other => panic!("unexpected {other}"),
                }
            }
        });
        let task = app.update(Message::Perms(Event::Set("demo".into(), "public_web".into(), Some(true))));
        settle_task(&mut app, task);
        fake.join().unwrap();
        let perms = &app.views["demo"].permissions;
        let (_, said, failed) = perms.notice.clone().unwrap();
        assert!(!failed && said == "Public web access on for this session. It applies when the session restarts.", "{said}");
        assert!(!perms.busy);
    }

    /// Restore defaults, Save for future sessions and auto-allow send what they say; auto-allow
    /// needs no capability (it is the session's own setting, not jail access).
    #[test]
    fn restore_save_and_auto_allow_send_their_requests() {
        let (client, mut daemon) = modes_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let fake = std::thread::spawn(move || {
            let mut seen = vec![];
            while seen.len() < 3 {
                let request = daemon.next();
                let method = request["method"].as_str().unwrap().to_string();
                match method.as_str() {
                    "_sill/permissions.restore_defaults" => {
                        assert_eq!(request["params"], json!({"session_id": "demo"}));
                        daemon.reply(&request, json!({"preview": view("preview", false, json!([])), "restart_required": false}));
                    }
                    "_sill/policy.save_host" => {
                        assert_eq!(request["params"], json!({"session_id": "demo", "host": "example.org"}));
                        daemon.reply(&request, json!({"agent_id": "claude", "agent_name": "Claude Code"}));
                    }
                    "_sill/sessions.update" => {
                        assert_eq!(request["params"]["change"], json!({"type": "auto_allow", "enabled": true}));
                        daemon.reply(&request, json!({}));
                    }
                    "_sill/sessions.list" => {
                        daemon.reply(&request, json!({"sessions": [session_record("demo", "demo", false, false)], "next": null}));
                        continue;
                    }
                    "_sill/policy.preview" => {
                        daemon.refuse(&request, "unavailable", "later");
                        continue;
                    }
                    other => panic!("unexpected {other}"),
                }
                seen.push(method);
            }
            seen
        });
        for event in [
            Event::Restore("demo".into()),
            Event::SaveHost("demo".into(), "example.org".into()),
            Event::AutoAllow("demo".into(), true),
        ] {
            let task = app.update(Message::Perms(event));
            settle_task(&mut app, task);
        }
        let seen = fake.join().unwrap();
        assert_eq!(seen.len(), 3, "{seen:?}");
        let (_, said, failed) = app.views["demo"].permissions.notice.clone().unwrap();
        assert!(!failed && said.starts_with("Auto-allow on"), "{said}");
    }

    /// A change waiting for a restart shows over the composer even with nothing expanded yet:
    /// the running jail is what counts, quietly.
    #[test]
    fn a_pending_change_shows_quietly_without_expanded_access() {
        let (mut app, _) = App::new(true, None);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let preview: PolicyView = serde_json::from_value(view("preview", true, json!([]))).unwrap();
        let launch: PolicyView = serde_json::from_value(view("launch", false, json!([]))).unwrap();
        app.views.get_mut("demo").unwrap().permissions.views =
            Some(Views { preview, launch: Some(launch), agent_name: None });
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("Default · jailed · permission changes apply after a restart").unwrap();
        assert!(ui.find("Jailed · expanded access").is_err(), "the running jail has no public web yet");
    }

    /// A ghost's panel offers no choices: the daemon refuses them for ghosts anyway.
    #[test]
    fn a_ghost_panel_offers_no_choices() {
        let (mut app, _) = App::new(true, None);
        let mut ghost = session_record("demo", "demo", false, true);
        ghost.ghost = true;
        app.all_sessions = vec![ghost];
        let perms = &mut app.views.get_mut("demo").unwrap().permissions;
        perms.open = true;
        perms.views = Some(serde_json::from_value::<PolicyView>(view("preview", false, json!([]))).map(|preview| Views {
            preview,
            launch: None,
            agent_name: None,
        }).unwrap());
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("Ghost session: Default only. Permission choices and saved defaults are refused.").unwrap();
        assert!(ui.find("Turn on").is_ok(), "auto-allow stays the session's own setting");
        assert!(ui.find("Policy default").is_err());
    }
}
