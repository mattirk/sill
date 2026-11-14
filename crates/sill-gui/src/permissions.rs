//! A session's Permissions (docs/permission-modes.md, Controls): a shield in the composer's bar
//! opens a compact panel above the composer with two groups, **Tool approvals** (Sill's
//! auto-allow of the agent's permission requests, which never widens jail access) and **Jail
//! access** (the daemon's capability states: public web, test desktop, private bus, GPU, and the
//! hosts approved for this session), and a separate **Host actions** group. While the session has
//! more than its policy's baseline, the shield is coloured and its tooltip names what it has (the
//! "Jailed · expanded access" caption over the composer went 2026-11-13, at the person's request).
//!
//! The daemon decides everything shown: the next build's preview and, for a running jail, its
//! launch snapshot. A change goes to the daemon and the panel reads both again; what differs
//! between them is marked as applying after a restart, with a Restart now. Choices need the
//! frontend-declared `sill.permission_modes` capability, which this window offers because it
//! shows this state. Host control is `host_control.rs`; the shield's right-click menu
//! opens the dropdown of Basic switches on a left click and this panel on a right click.
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
        "An isolated Weston desktop with Xwayland for running GUI applications, never the real one. Headless: watch it from the monitor button. The agent can take screenshots of it and drive X11 programs.",
    ),
    (
        "private_bus",
        "Private D-Bus",
        "A D-Bus session bus of the jail's own, for test applications and the services they activate, which run in the jail too. Your session and system buses stay out of reach.",
    ),
    ("gpu", "GPU rendering", "Intel/AMD render device access, including GPU driver ioctls and shared GPU resources. Requires restart."),
];

#[derive(Debug, Clone)]
pub enum Event {
    /// Show or hide the panel.
    Toggle(String),
    Filesystem(String),
    /// The next build and, for a running jail, its launch (a read is numbered: only the latest
    /// one counts), and the agent's name when it was asked for.
    Read(String, u64, Result<Views, String>),
    /// A capability choice: on, off, or the policy default.
    Set(String, String, Option<bool>),
    Restore(String),
    SaveHost(String, String),
    AutoAllow(String, bool),
    /// Apply development permissions: the test desktop and private D-Bus together.
    ApplyDevelopment(String),
    /// The shield's right press, or a press anywhere while its menu is open: the menu of
    /// shortcuts opens or closes.
    ShieldMenu(String),
    /// The shield menu's first choice: development permissions, then a restart where the
    /// running jail needs one. The panel is left as it is; the shield's tooltip and caption
    /// carry the outcome.
    DevelopmentRestarting(String),
    /// The Basic Development switch: on as `DevelopmentRestarting`; off returns the test desktop
    /// and private D-Bus to the policy, then restarts where the running jail needs it.
    SetDevelopment(String, bool),
    /// A Basic Host control switch (the panel's or the shield menu's): a group on or off.
    HostGroup(String, crate::host_control::Group, bool),
    /// Stop and start the jail: for new permissions, or for changed pins (binds are made at
    /// launch, so a pin added to a running session reaches it only this way).
    Restart(String, Change),
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
    Development,
    /// Development, followed by a restart where the running jail needs one (the shield menu).
    DevelopmentRestarting,
    /// The Basic switch's off, followed by a restart where the running jail needs one.
    DevelopmentOff,
    Restore,
    SaveHost(String),
    AutoAllow(bool),
    Restart,
}

/// A session's permission state in this window.
#[derive(Default)]
pub struct Perms {
    pub open: bool,
    /// The shield's right-click menu is open.
    pub menu: bool,
    pub filesystem_open: bool,
    /// Host control's operations to activate again after the Development switch's restart.
    reactivate: Vec<String>,
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
        if launch.filesystem.as_ref().map(|f| &f.rules) != preview.filesystem.as_ref().map(|f| &f.rules) || launch.filesystem.is_none() { return true; }
        let differs = ROWS.iter().any(|(capability, _, _)| {
            state(launch, capability).map(|s| s.effective)
                != state(preview, capability).map(|s| s.effective)
        });
        let now: Vec<&str> = applied_hosts(preview).collect();
        differs || applied_hosts(launch).any(|h| !now.contains(&h))
    }

    /// The Basic Development switch is on: the test desktop and private D-Bus are both on for
    /// the next build.
    pub fn development_on(&self) -> bool {
        self.views.as_ref().is_some_and(|views| {
            DEVELOPMENT.iter().all(|c| state(&views.preview, c).is_some_and(|s| s.effective))
        })
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
        // a Basic switch waits for a session that is still attaching (after a restart, say):
        // a second restart or an activation on top of it leaves the session half started
        if let Event::SetDevelopment(id, _) | Event::HostGroup(id, ..) = &event
            && let Some(view) = self.views.get_mut(id)
            && matches!(view.status, crate::Status::Connecting | crate::Status::Reconnecting(_) | crate::Status::Loading)
        {
            self.permission_serial += 1;
            view.permissions.note(self.permission_serial, "The session is still starting; try again in a moment.".into(), true);
            return Task::none();
        }
        match event {
            Event::Filesystem(id) => {
                if let Some(view) = self.views.get_mut(&id) { view.permissions.filesystem_open = !view.permissions.filesystem_open; }
            }
            Event::Toggle(id) => {
                let Some(view) = self.views.get_mut(&id) else {
                    return Task::none();
                };
                view.permissions.open = !view.permissions.open;
                if view.permissions.open {
                    return Task::batch([self.read_permissions(&id), self.read_host_control(&id)]);
                }
            }
            Event::ShieldMenu(id) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.permissions.menu = !view.permissions.menu;
                    if view.permissions.menu {
                        // its switches show the daemon's state: read as the panel does
                        return Task::batch([self.read_permissions(&id), self.read_host_control(&id)]);
                    }
                }
            }
            Event::DevelopmentRestarting(id) => {
                let params = json!({"session_id": id});
                return self.permission_change(
                    id,
                    Change::DevelopmentRestarting,
                    "_sill/permissions.apply_development",
                    params,
                );
            }
            Event::SetDevelopment(id, true) => return self.permissions_event(Event::DevelopmentRestarting(id)),
            Event::SetDevelopment(id, false) => {
                let (Some(actions), Some(view)) = (self.actions.clone(), self.views.get_mut(&id)) else {
                    return Task::none();
                };
                if view.permissions.busy {
                    return Task::none();
                }
                view.permissions.busy = true;
                let session = id.clone();
                return blocking(
                    move || {
                        let mut last = None;
                        for capability in DEVELOPMENT {
                            let params = json!({"session_id": session, "capability": capability, "enabled": null});
                            let outcome = actions.call_needing(CAPABILITY, "_sill/permissions.set", params);
                            let done = outcome.is_done();
                            last = Some(outcome);
                            if !done {
                                break;
                            }
                        }
                        last.expect("DEVELOPMENT is not empty")
                    },
                    move |outcome| Message::Perms(Event::Done(id, Change::DevelopmentOff, outcome)),
                );
            }
            Event::HostGroup(id, group, on) => {
                return self.host_control_event(crate::host_control::Event::SetGroup(id, group, on));
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
            Event::ApplyDevelopment(id) => {
                let params = json!({"session_id": id});
                return self.permission_change(
                    id,
                    Change::Development,
                    "_sill/permissions.apply_development",
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
            Event::Restart(id, change) => {
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
                    move |outcome| Message::Perms(Event::Done(id, change, outcome)),
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
            // the daemon's word (it knows whether the jail runs; this window's session record
            // can be seconds old, and a new session's says stopped until the next list)
            restart_required = changed.restart_required;
            if let Some(views) = &mut perms.views {
                views.preview = changed.preview;
            }
            perms.restart_required = restart_required;
        }
        let later = if restart_required { "; applies after a restart" } else { "" };
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
                            Some(true) => format!("{name} on{later}."),
                            Some(false) => format!("{name} off{later}."),
                            None => format!("{name} follows the policy{later}."),
                        }
                    }
                    Change::Development => format!("Development permissions on{later}."),
                    Change::DevelopmentRestarting if restart_required => "Development permissions on; restarting…".into(),
                    Change::DevelopmentRestarting => "Development permissions on.".into(),
                    Change::DevelopmentOff if restart_required => "Development permissions off; restarting…".into(),
                    Change::DevelopmentOff => "Development permissions off.".into(),
                    Change::Restore => format!("Defaults restored{later}."),
                    Change::SaveHost(host) => format!("{host} saved for new {agent} sessions."),
                    Change::AutoAllow(true) => "Auto-allow on.".into(),
                    Change::AutoAllow(false) => "Auto-allow off.".into(),
                    Change::Restart => "Restarted with the new permissions.".into(),
                },
                false,
            ),
        };
        self.permission_serial += 1;
        let serial = self.permission_serial;
        if let Some(view) = self.views.get_mut(&id) {
            view.permissions.note(serial, said, failed);
        }
        let mut reactivate = Task::none();
        if matches!(change, Change::Restart) {
            let operations = self.views.get_mut(&id).map(|v| std::mem::take(&mut v.permissions.reactivate)).unwrap_or_default();
            if outcome.is_done() {
                self.reattach(&id);
                if !operations.is_empty()
                    && let Some(view) = self.views.get_mut(&id)
                {
                    view.host_control.selected = operations.into_iter().collect();
                    reactivate = self.host_control_event(crate::host_control::Event::Activate(id.clone()));
                }
            }
        }
        // the Development switch and the shield menu's choice go straight on to the restart it needs
        if matches!(change, Change::DevelopmentRestarting | Change::DevelopmentOff) && outcome.is_done() && restart_required {
            // a restart ends Host control (the daemon revokes it when the jail stops): what the
            // person had switched on comes back after it
            let active: Vec<String> = self
                .session(&id)
                .and_then(|s| s.host_control.as_ref())
                .map(|brief| brief.operations.clone())
                .unwrap_or_default();
            if let Some(view) = self.views.get_mut(&id) {
                view.permissions.reactivate = active;
            }
            return self.permissions_event(Event::Restart(id, Change::Restart));
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
        Task::batch([expire, reactivate, self.list(), self.read_permissions(&id)])
    }
}

// --- the view -------------------------------------------------------------------------------

/// The composer bar's shield: warning-coloured while the session has expanded access.
pub fn indicator<'a>(view: &'a SessionView, session: Option<&Session>) -> Element<'a, Message> {
    let running = session.is_some_and(|s| s.running);
    let expanded = view.permissions.expanded(running);
    // orange while the agent may act on the host; yellow, the lesser caution, for expanded
    // access in the jail and for control of the VM (the person, 2026-11-13)
    let on = session.map(crate::host_control::groups_on).unwrap_or_default();
    let color = if on.contains(&crate::host_control::Group::Host) {
        P.warning
    } else if !expanded.is_empty() || !on.is_empty() || view.permissions.development_on() {
        alpha(style::yellow(), 0.85)
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
    for (group, label, _) in &crate::host_control::groups() {
        if on.contains(group) {
            tip.push_str(&format!(" · {label} control active"));
        }
    }
    if pending {
        tip.push_str(" · changes apply after a restart");
    }
    tip.push_str(" · right click: all permissions");
    let shield = keys::press_maybe(
        crate::ui::bar_button(icons::glyph("shield", crate::ui::BAR_ICON, color)).style(style::quiet),
        session.map(|_| Message::Perms(Event::ShieldMenu(view.id.clone()))),
    );
    // a right press opens the panel of granular toggles
    let shield: Element<'a, Message> = if session.is_some() {
        iced::widget::mouse_area(shield).on_right_press(Message::Perms(Event::Toggle(view.id.clone()))).into()
    } else {
        shield
    };
    // no tooltip while the menu is open: both sit above the button and the tooltip is drawn
    // over the menu's rows
    let shield = container(shield).id("permissions-button");
    if view.permissions.menu {
        return shield.into();
    }
    crate::ui::bar_tip(shield, tip)
}

/// The shield's dropdown (a left click; the person, 2026-11-13): the Basic switches alone, small,
/// floating over the window's corner above the shield. The panel (a right click) has the
/// granular toggles and never these.
pub fn dropdown<'a>(view: &'a SessionView, session: &'a Session) -> Element<'a, Message> {
    let perms = &view.permissions;
    let id = view.id.as_str();
    let can_change = !session.ghost && !perms.busy;
    // Basic: three switches that act at once (the person, 2026-11-13). Development is the test
    // desktop and private D-Bus; VM and Host are Host control's operations in two groups (Host
    // alone where the daemon's host is this machine). Advanced holds the granular toggles.
    let development_on = perms.development_on();
    let development_usable = perms.views.as_ref().is_some_and(|views| {
        DEVELOPMENT.iter().all(|c| state(&views.preview, c).is_some_and(|s| s.available && s.allowed))
    });
    let mut basic = Column::new();
    basic = basic.push(basic_switch(
        DEVELOPMENT_SWITCH,
        "A test desktop and a private D-Bus in the jail, for building and trying desktop apps. Switching restarts the jail when it needs it, which stops the agent's current turn.".into(),
        development_on,
        style::yellow(),
        (can_change && (development_on || development_usable)).then(|| {
            let id = id.to_string();
            move |on: bool| Message::Perms(Event::SetDevelopment(id.clone(), on))
        }),
    ));
    let groups_on = crate::host_control::groups_on(session);
    let host_free = can_change && !view.host_control.busy && crate::host_control::ban(view).is_none();
    for (group, label, operations) in crate::host_control::groups() {
        let names: Vec<String> = operations.iter().map(|op| crate::host_control::operation_label(op)).collect();
        let (said, color) = match (group, cfg!(target_os = "macos")) {
            (crate::host_control::Group::Vm, _) => (
                format!("The agent may act in Sill's virtual machine, outside its jail: {}. Until you switch it off, the window stops showing the session, or the session stops.", names.join(", ")),
                style::yellow(),
            ),
            (crate::host_control::Group::Host, true) => (
                format!("The agent may act on your Mac, as you: {}. Programs run unrestricted from shared folders. Until you switch it off or Sill quits.", names.join(", ")),
                P.warning,
            ),
            (crate::host_control::Group::Host, false) => (
                format!("The agent may act on this machine outside its jail, as you: {}. Until you switch it off, the window stops showing the session, or the session stops.", names.join(", ")),
                P.warning,
            ),
        };
        let on = groups_on.contains(&group);
        basic = basic.push(basic_switch(
            label,
            said,
            on,
            color,
            host_free.then(|| {
                let id = id.to_string();
                move |on: bool| Message::Perms(Event::HostGroup(id.clone(), group, on))
            }),
        ));
    }
    if let Some(error) = &view.host_control.error {
        basic = basic.push(text(error).size(style::TINY).color(P.error));
    } else if let Some(ban) = crate::host_control::ban(view) {
        basic = basic.push(ban);
    }
    if let Some((_, note, true)) = &perms.notice {
        basic = basic.push(text(note).size(style::TINY).color(P.error));
    }
    container(basic.width(DROPDOWN + 2.0 * ROW_INSET))
        .padding([4, 4])
        .style(|theme| {
            let menu = style::control_menu(theme);
            container::Style {
                background: Some(menu.background),
                border: menu.border,
                shadow: menu.shadow,
                ..container::Style::default()
            }
        })
        .into()
}

/// Room inside a dropdown row's highlight, on each side of its name and switch.
const ROW_INSET: f32 = 8.0;
/// The dropdown's width: a name and its switch.
const DROPDOWN: f32 = 170.0;

/// The Basic switch for the development permissions, in the shield's dropdown.
pub const DEVELOPMENT_SWITCH: &str = "Development";

pub(crate) fn small<'a>(label: &'a str, message: Option<Message>) -> Element<'a, Message> {
    keys::press_maybe(
        button(text(label).size(style::TINY))
            .padding([2, 0])
            .style(style::quiet),
        message,
    )
}

/// What Apply development permissions turns on together (sill-core's DEVELOPMENT_CAPABILITIES).
const DEVELOPMENT: [&str; 2] = ["test_desktop", "private_bus"];

/// Fixed slots, so nothing moves when a state changes: the switch and Restart now are always
/// there (or a gap their size).
const SWITCH_SLOT: f32 = 40.0;
/// The two columns' widths (the jail column carries the longer button row) and the gap between
/// them; a narrower pane stacks them.
const JAIL_COLUMN: f32 = 290.0;
const HOST_COLUMN: f32 = 210.0;
const GUTTER: f32 = 16.0;
/// The width under which the columns stack: both columns, the gutter and the panel's padding.
pub const NARROW: f32 = JAIL_COLUMN + GUTTER + HOST_COLUMN + 24.0;

/// Every row of both columns is this tall, so the columns' rows line up with each other.
const ROW: f32 = 26.0;
const HEADING_ROW: f32 = 22.0;

/// A column's heading: the only words in the panel that are not a row.
fn heading<'a>(label: &'static str, color: iced::Color) -> Element<'a, Message> {
    container(text(label).size(style::TINY).color(color))
        .height(HEADING_ROW)
        .align_y(Alignment::Center)
        .into()
}

/// One row of a column, at the rows' fixed height.
fn slot<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content).height(ROW).width(Fill).align_y(Alignment::Center).into()
}
const RESTART_SLOT: f32 = 92.0;

/// An on/off switch in a fixed slot: the warning colour when on (expanded access), dimmed when it
/// cannot be changed.
pub(crate) fn switch<'a>(on: bool, set: Option<impl Fn(bool) -> Message + 'a>) -> Element<'a, Message> {
    container(crate::settings_ui::switch_styled(on, P.warning, 16.0, set))
        .width(SWITCH_SLOT)
        .into()
}

/// A Basic switch: its name with the switch at the row's right, yellow for development and
/// the VM, the warning colour for the host. The whole row flips it (the person, 2026-11-13).
fn basic_switch<'a>(
    label: &'a str,
    said: String,
    on: bool,
    color: iced::Color,
    set: Option<impl Fn(bool) -> Message + Clone + 'a>,
) -> Element<'a, Message> {
    let flip = set.clone().map(|set| set(!on));
    let line = row![
        text(label).size(style::SMALL).width(Fill),
        crate::settings_ui::switch_styled(on, color, 16.0, set),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    tip(
        // the hover's highlight is the row: the name and the switch inset in it alike, the line
        // centred in its height
        keys::press_maybe(
            button(container(line).height(Fill).align_y(Alignment::Center))
                .width(Fill)
                .height(ROW + 4.0)
                .padding([0.0, ROW_INSET])
                .style(style::quiet),
            flip,
        ),
        said,
    )
}

/// `content` with its explanation in a tooltip: the panel shows names and states only.
pub(crate) fn tip<'a>(content: impl Into<Element<'a, Message>>, said: String) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(said).size(style::SMALL))
            .padding([4, 8])
            .max_width(380)
            .style(style::fill(P.code_bg)),
        tooltip::Position::Top,
    )
    .into()
}

/// A row's name, explained on hover.
pub(crate) fn name<'a>(name: &'a str, said: String) -> Element<'a, Message> {
    tip(text(name).size(style::SMALL).width(140), said)
}

/// One capability on one line: name (meaning and reason on hover), state, and the choice.
fn capability_row<'a>(
    id: &'a str,
    capability: &'a str,
    label: &'a str,
    meaning: &'a str,
    views: &'a Views,
    running: bool,
    can_change: bool,
) -> Element<'a, Message> {
    let Some(s) = state(&views.preview, capability) else {
        return space().height(0).into();
    };
    // the switch says on or off; the text only what it cannot
    let mut status = if !s.allowed {
        "Forbidden"
    } else if s.effective {
        ""
    } else if !s.available {
        "Unavailable"
    } else if s.blocked {
        "Blocked"
    } else {
        ""
    }
    .to_string();
    if running
        && let Some(launch) = views.launch.as_ref().and_then(|l| state(l, capability))
        && launch.effective != s.effective
    {
        if !status.is_empty() {
            status.push_str(" · ");
        }
        status.push_str("needs restart");
    }
    let color = if s.effective {
        P.warning
    } else if !s.allowed || !s.available || s.blocked {
        P.fg_dim
    } else {
        P.fg
    };
    let mut said = meaning.to_string();
    if s.effective {
        said.push_str(if s.source == "session" {
            " On by this session's choice."
        } else {
            " On by the policy."
        });
    } else if !s.allowed || !s.available || s.blocked {
        said.push_str(&format!(" {}.", capitalised(&s.reason)));
    }
    let changeable = can_change && s.allowed && s.available;
    let set = changeable.then(|| {
        let (id, capability) = (id.to_string(), capability.to_string());
        move |on: bool| Message::Perms(Event::Set(id.clone(), capability.clone(), Some(on)))
    });
    row![
        name(label, said),
        switch(s.effective, set),
        text(status)
            .size(style::SMALL)
            .color(color)
            .wrapping(text::Wrapping::None)
            .width(Fill),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
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

/// A capability that cannot be had and nobody asks for: folded into one quiet line.
/// The panel over the composer, when open: one line per thing, explanations on hover.
pub fn panel<'a>(view: &'a SessionView, session: Option<&'a Session>, narrow: bool) -> Element<'a, Message> {
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
    // Desktop development is the test desktop and private bus; anything else stays Default. Of
    // the running jail, as the caption: a change shows as needing a restart until then.
    let desktop = perms.views.as_ref().is_some_and(|v| {
        let current = match (&v.launch, running) {
            (Some(launch), true) => launch,
            _ => &v.preview,
        };
        ["test_desktop", "private_bus"]
            .iter()
            .any(|c| state(current, c).is_some_and(|s| s.effective))
    });
    let mode = match (ghost, desktop, expanded.is_empty()) {
        (true, _, _) => "Ghost · Default only",
        (_, true, _) => "Desktop development",
        (_, false, true) => "Default",
        (_, false, false) => "Default · expanded access",
    };
    let mut about = match &perms.views {
        Some(views) => format!("Policy {}.", policy_name(&views.preview.name)),
        None => String::new(),
    };
    if ghost {
        about.push_str(" A ghost session supports Default only: choices and saved defaults are refused.");
    }
    let mut content = Column::new().spacing(6);
    content = content.push(
        row![
            tip(
                text("Permissions").size(style::SMALL).font(iced::Font {
                    weight: iced::font::Weight::Bold,
                    ..iced::Font::with_name(crate::fonts::SANS_FAMILY)
                }),
                about.trim().to_string(),
            ),
            text(mode)
                .size(style::TINY)
                .color(if expanded.is_empty() { P.fg_dim } else { P.warning })
                .width(Fill),
            tip(
                keys::press(
                    button(icons::glyph("x", 12.0, alpha(P.fg, 0.55)))
                        .padding([4, 6])
                        .style(style::quiet),
                    Message::Perms(Event::Toggle(id.into())),
                ),
                "Close".into(),
            ),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    );

    // IN THE JAIL: tool approvals, then jail access; ON THE HOST: Host control. Two columns of
    // fixed width (the second under the first in a narrow window); nothing moves between states.
    let mut jail = Column::new().width(JAIL_COLUMN);
    jail = jail.push(heading("IN THE JAIL", P.fg_dim));
    // a tool approval, not jail access: said on hover
    jail = jail.push(slot(
        row![
            name(
                "Auto-allow",
                "Tool approvals: Sill answers the agent's permission requests \"allow once\"; each is still listed. Jail access does not change.".into(),
            ),
            switch(
                session.auto_allow,
                (!perms.busy).then(|| {
                    let id = id.to_string();
                    move |on: bool| Message::Perms(Event::AutoAllow(id.clone(), on))
                }),
            ),
            space().width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    ));
    match &perms.views {
        None => {
            jail = jail.push(text(match &perms.error {
                Some(error) => format!("Permissions could not be read: {error}"),
                None => "Reading permissions…".into(),
            }).size(style::TINY).color(P.fg_dim));
        }
        Some(views) => {
            for diagnostic in &views.preview.diagnostics {
                jail = jail.push(text(diagnostic).size(style::TINY).color(P.error));
            }
            for (capability, label, meaning) in ROWS {
                jail = jail.push(slot(capability_row(id, capability, label, meaning, views, running, can_change)));
                if *capability == "public_web" {
                    // one host (or none) is a row; more take the lines they need
                    let list = hosts(id, views, perms, can_change);
                    jail = jail.push(if views.preview.session_hosts.len() <= 1 { slot(list) } else { list });
                }
            }
        }
    }
    let restorable = perms.views.as_ref().is_some_and(|views| {
        views.preview.permissions.iter().any(|p| p.requested.is_some())
            || !views.preview.session_hosts.is_empty()
    });
    // Apply development permissions: the test desktop and private D-Bus together, for the next
    // build. Greyed, with the reason on hover, where either cannot be turned on or both are on.
    let (development, development_tip) = match &perms.views {
        Some(views) => {
            let states: Vec<_> = DEVELOPMENT.iter().map(|c| state(&views.preview, c)).collect();
            let unusable: Vec<String> = DEVELOPMENT
                .iter()
                .zip(&states)
                .filter(|(_, s)| !s.is_some_and(|s| s.available && s.allowed))
                .map(|(c, s)| {
                    let name = ROWS.iter().find(|(r, _, _)| r == c).map_or(*c, |(_, n, _)| n);
                    match s {
                        Some(s) => format!("{name}: {}", s.reason),
                        None => format!("{name}: unknown here"),
                    }
                })
                .collect();
            if !unusable.is_empty() {
                (false, unusable.join("\n"))
            } else if states.iter().all(|s| s.is_some_and(|s| s.effective)) {
                (false, "The test desktop and the private D-Bus are already on for the next build.".into())
            } else {
                (true, "Turns on the test desktop and the private D-Bus together for this session. GPU rendering stays a separate choice; Host control is never included.".into())
            }
        }
        None => (false, String::new()),
    };
    let jail_buttons: Element<'a, Message> = row![
        tip(
            small(
                "Restore defaults",
                (can_change && restorable).then(|| Message::Perms(Event::Restore(id.into()))),
            ),
            "Removes this session's added permissions and approved hosts. The policy and pins stay.".into(),
        ),
        tip(
            small(
                "Apply development permissions",
                (can_change && development).then(|| Message::Perms(Event::ApplyDevelopment(id.into()))),
            ),
            development_tip,
        ),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into();

    // the host column: docs/permission-modes.md's separately labelled Host actions, independent
    // of the policy read, in the warning colour while active
    let (host_heading, active) = crate::host_control::heading(session);
    let mut host = Column::new().width(HOST_COLUMN);
    host = host.push(heading(host_heading, if active { P.error } else { P.fg_dim }));
    for row in crate::host_control::rows(view, session, can_change) {
        host = host.push(slot(row));
    }
    if let Some(ban) = crate::host_control::ban(view) {
        host = host.push(ban);
    }
    let host_buttons = crate::host_control::buttons(view, session, can_change);

    // the buttons on one line under both columns, each under its own; stacked, each column
    // keeps its buttons
    let columns: Element<'a, Message> = if narrow {
        column![jail, space().height(4), jail_buttons, space().height(12), host, space().height(4), host_buttons].into()
    } else {
        column![
            row![jail, space().width(GUTTER), host].align_y(Alignment::Start),
            space().height(4),
            row![
                container(jail_buttons).width(JAIL_COLUMN),
                space().width(GUTTER),
                container(host_buttons).width(HOST_COLUMN),
            ]
            .align_y(Alignment::Center),
        ]
        .into()
    };
    content = content.push(columns);
    content = content.push(keys::press(button(text(if perms.filesystem_open { "▾ Filesystem" } else { "▸ Filesystem" }).size(style::TINY)).style(style::quiet), Message::Perms(Event::Filesystem(id.into()))));
    if perms.filesystem_open {
        let mut files = Column::new().spacing(4);
        if let Some(views) = &perms.views {
            let current = if running { views.launch.as_ref() } else { Some(&views.preview) };
            files = files.push(text(if running { "Current filesystem" } else { "Next launch" }).size(style::TINY).color(P.fg_dim));
            let show = |label: &str, fs: Option<&sill_proto::admin::FilesystemView>| {
                let mut rows = Column::new().spacing(3).push(text(label.to_string()).size(style::TINY).color(P.fg_dim));
                match fs {
                    None => rows = rows.push(text("Filesystem plan not recorded; rebuild this session.").size(style::TINY).color(P.warning)),
                    Some(fs) => {
                        for rule in fs.rules.iter().filter(|rule| {
                            rule.behavior != "agent_mask"
                                || !fs.rules.iter().any(|agent| {
                                    agent.behavior == "agent" && agent.target == rule.target
                                })
                        }) {
                            let backing = rule.backing.as_ref().map(|p| format!(" → {}", p.display)).unwrap_or_default();
                            rows = rows.push(tip(text(format!("{}: {}{}", rule.behavior, rule.target.display, backing)).size(style::TINY), format!("{} — {}", rule.source, rule.reason)));
                        }
                        for path in &fs.unused { rows = rows.push(text(format!("Stored but unused: {}", path.display)).size(style::TINY).color(P.fg_dim)); }
                    }
                }
                rows
            };
            files = files.push(show("", current.and_then(|v| v.filesystem.as_ref())));
            if running && current.and_then(|v| v.filesystem.as_ref()).map(|f| &f.rules) != views.preview.filesystem.as_ref().map(|f| &f.rules) {
                files = files.push(show("After restart", views.preview.filesystem.as_ref()));
            }
        }
        content = content.push(iced::widget::scrollable(files).height(iced::Length::Fixed(200.0)));
    }

    let pending = perms.restart_pending(running) || session.policy_changed;
    // one line for the last change's note, else the restart that is waiting
    let (said, color): (String, _) = match &perms.notice {
        Some((_, note, true)) => (note.clone(), P.error),
        Some((_, note, false)) => (note.clone(), alpha(P.fg, 0.7)),
        None if pending => ("Applies after a restart".into(), P.warning),
        None => (String::new(), P.fg_dim),
    };
    let restart: Element<'a, Message> = if pending {
        let mut why = "The running jail was built with other permissions; a restart applies them.".to_string();
        if view.conversation.busy() {
            why.push_str(" Restarting stops the agent's current turn.");
        }
        tip(
            keys::press_maybe(
                button(text("Restart now").size(style::TINY))
                    .width(RESTART_SLOT)
                    .padding([2, 8])
                    .style(style::quiet),
                (!perms.busy).then(|| Message::Perms(Event::Restart(id.into(), Change::Restart))),
            ),
            why,
        )
    } else {
        space().width(RESTART_SLOT).into()
    };
    content = content.push(
        row![
            container(
                text(said)
                    .size(style::TINY)
                    .color(color)
                    .wrapping(text::Wrapping::None),
            )
            .width(Fill)
            .clip(true),
            restart,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );
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

/// The hosts approved for this session, one line each (none: no line). Blocked ones say why on
/// hover; Save names the agent whose defaults it changes.
fn hosts<'a>(id: &'a str, views: &'a Views, perms: &'a Perms, can_change: bool) -> Element<'a, Message> {
    let agent = perms.agent_name.as_deref().unwrap_or("this agent");
    let mut list = Column::new().spacing(2);
    for grant in &views.preview.session_hosts {
        let host: Element<'a, Message> = if grant.applied {
            text(&grant.host).size(style::SMALL).color(P.warning).into()
        } else {
            tip(
                text(format!("{} · blocked", grant.host)).size(style::SMALL).color(P.fg_dim),
                capitalised(&grant.reason) + ". It stays approved and applies again when that goes.",
            )
        };
        let mut line = row![container(host).width(Fill)].spacing(8).align_y(Alignment::Center);
        if can_change {
            line = line.push(tip(
                small("Save", Some(Message::Perms(Event::SaveHost(id.into(), grant.host.clone())))),
                format!(
                    "Save for future sessions: adds {} to {agent}'s network defaults, so every new {agent} session may reach it. Existing sessions do not change.",
                    grant.host
                ),
            ));
        }
        list = list.push(line);
    }
    if views.preview.session_hosts.is_empty() {
        list = list.push(text("None").size(style::SMALL).color(P.fg_dim));
    }
    row![
        name("Approved hosts", "Hosts you allowed when the proxy refused them: this session only.".into()),
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
        json!({"filesystem": {"rules": [], "unused": []},"view": kind, "session_id": "demo", "name": "default", "text": null,
               "fingerprint": null, "diagnostics": [],
               "permissions": [grant("test_desktop", false, "policy"), grant("private_bus", false, "policy"),
                               grant("gpu", false, "policy"),
                               grant("public_web", public_web, if public_web { "session" } else { "policy" }),
                               grant("host_control", false, "policy")],
               "session_hosts": hosts})
    }

    /// The shield's left click opens the dropdown of Basic switches (Development and, without a
    /// Mac's VM, Host); the panel has the granular toggles and never the switches.
    #[test]
    fn the_dropdown_has_the_basic_switches_and_the_panel_the_granular_ones() {
        let (mut app, _) = App::new(true, None);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let _ = app.update(Message::Perms(Event::ShieldMenu("demo".into())));
        assert!(app.views["demo"].permissions.menu && !app.views["demo"].permissions.open);
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find(DEVELOPMENT_SWITCH).unwrap();
        ui.find("Host").unwrap();
        assert!(ui.find("VM").is_err() && ui.find("IN THE JAIL").is_err());
        // the whole row flips its switch, and the press does not close the dropdown
        ui.click("Host").unwrap();
        let messages: Vec<Message> = ui.into_messages().collect();
        assert!(messages.iter().any(|m| matches!(m, Message::Perms(Event::HostGroup(_, _, true)))));
        assert!(!messages.iter().any(|m| matches!(m, Message::Perms(Event::ShieldMenu(_)))));
        let _ = app.update(Message::Perms(Event::ShieldMenu("demo".into())));
        app.views.get_mut("demo").unwrap().permissions.open = true;
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("IN THE JAIL").unwrap();
        ui.find("ON THE HOST").unwrap();
        assert!(ui.find(DEVELOPMENT_SWITCH).is_err());
    }

    #[test]
    fn filesystem_changes_require_restart_but_unused_inventory_does_not() {
        let launch: PolicyView = serde_json::from_value(view("launch", false, json!([]))).unwrap();
        let mut preview = launch.clone();
        preview.filesystem.as_mut().unwrap().unused.push(sill_proto::sessions::HostPath::from_path(std::path::Path::new("/stored/.cargo")));
        let mut perms = Perms { views: Some(Views { launch: Some(launch), preview, agent_name: None }), ..Default::default() };
        assert!(!perms.filesystem_open);
        assert!(!perms.restart_pending(true));
        perms.views.as_mut().unwrap().preview.filesystem.as_mut().unwrap().rules.push(sill_proto::admin::PathRule {
            target: sill_proto::sessions::HostPath::from_path(std::path::Path::new("/home/me/.codex")),
            backing: None, behavior: "agent".into(), source: "home_paths".into(), reason: "new definition".into(),
        });
        assert!(perms.restart_pending(true));
        assert!(!perms.restart_pending(false));
        perms.views.as_mut().unwrap().launch.as_mut().unwrap().filesystem = None;
        assert!(perms.restart_pending(true));
    }

    #[test]
    fn filesystem_folds_only_agent_masks_that_duplicate_agent_paths() {
        let (mut app, _) = App::new(true, None);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let mut preview: PolicyView = serde_json::from_value(view("preview", false, json!([]))).unwrap();
        let rule = |behavior: &str, target: &str| sill_proto::admin::PathRule {
            target: sill_proto::sessions::HostPath::from_path(std::path::Path::new(target)),
            backing: None,
            behavior: behavior.into(),
            source: "test".into(),
            reason: "test rule".into(),
        };
        preview.filesystem.as_mut().unwrap().rules = vec![
            rule("agent", "/home/me/.claude"),
            rule("agent_mask", "/home/me/.claude"),
            rule("agent_mask", "/home/me/.codex"),
            rule("mask", "/home/me/.claude"),
        ];
        app.views.get_mut("demo").unwrap().permissions = Perms {
            open: true,
            filesystem_open: true,
            views: Some(Views { preview, launch: None, agent_name: None }),
            ..Default::default()
        };
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("agent: /home/me/.claude").unwrap();
        assert!(ui.find("agent_mask: /home/me/.claude").is_err());
        ui.find("agent_mask: /home/me/.codex").unwrap();
        ui.find("mask: /home/me/.claude").unwrap();
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
        ui.find("Default · expanded access").unwrap();
        ui.find("needs restart").unwrap();
        ui.find("Restart now").unwrap();
        ui.find("Applies after a restart").unwrap();
        ui.find("blocked.example · blocked").unwrap();
        ui.find("Requested · unavailable").is_err().then_some(()).unwrap();
        ui.find("IN THE JAIL").unwrap();
        ui.find("ON THE HOST").unwrap();
        ui.find("GPU rendering").unwrap();
        assert!(ui.find("TOOL APPROVALS").is_err(), "no group headings beyond the two columns'");
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
        assert!(!failed && said == "Public web access on; applies after a restart.", "{said}");
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
        assert!(!failed && said == "Auto-allow on.", "{said}");
    }

    /// Apply development permissions sends its one request; its button is live only where both
    /// halves can be turned on.
    #[test]
    fn apply_development_permissions_sends_one_request_when_both_halves_can_be_on() {
        let (client, mut daemon) = modes_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let mut preview = view("preview", false, json!([]));
        for (i, capability) in ["test_desktop", "private_bus"].iter().enumerate() {
            preview["permissions"][i] = json!({"capability": capability, "allowed": true,
                "default_enabled": false, "requested": null, "blocked": false, "source": "policy",
                "effective": false, "available": true, "reason": "off; explicit permission required to widen access"});
        }
        let preview: PolicyView = serde_json::from_value(preview).unwrap();
        {
            let perms = &mut app.views.get_mut("demo").unwrap().permissions;
            perms.open = true;
            perms.views = Some(Views { launch: None, preview: preview.clone(), agent_name: None });
        }
        {
            let mut ui = simulator(&app, (720.0, 860.0));
            ui.find("Apply development permissions").unwrap();
        }
        let fake = std::thread::spawn(move || {
            loop {
                let request = daemon.next();
                match request["method"].as_str().unwrap() {
                    "_sill/permissions.apply_development" => {
                        assert_eq!(request["params"], json!({"session_id": "demo"}));
                        daemon.reply(&request, json!({"preview": view("preview", false, json!([])), "restart_required": true}));
                        return;
                    }
                    "_sill/sessions.list" => daemon.reply(&request, json!({"sessions": [session_record("demo", "demo", false, false)], "next": null})),
                    "_sill/policy.preview" => daemon.refuse(&request, "unavailable", "later"),
                    other => panic!("unexpected {other}"),
                }
            }
        });
        let task = app.update(Message::Perms(Event::ApplyDevelopment("demo".into())));
        settle_task(&mut app, task);
        fake.join().unwrap();
        let (_, said, failed) = app.views["demo"].permissions.notice.clone().unwrap();
        assert!(!failed && said == "Development permissions on; applies after a restart.", "{said}");
        // with one half unavailable the button is there but dead, and says why on hover
        let mut unavailable = preview.clone();
        unavailable.permissions[1].available = false;
        unavailable.permissions[1].reason = "dbus-daemon is not installed (the private bus needs the dbus package)".into();
        let perms = &mut app.views.get_mut("demo").unwrap().permissions;
        perms.views = Some(Views { launch: None, preview: unavailable, agent_name: None });
        perms.notice = None;
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("Apply development permissions").unwrap();
    }

    /// The shield menu's development choice opens the panel, applies development permissions and
    /// restarts the running session in one go; Escape closes the menu.
    #[test]
    fn the_shield_menu_applies_development_and_restarts() {
        let (client, mut daemon) = modes_client();
        let mut app = live_app(vec![client]);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let _ = app.update(Message::Perms(Event::ShieldMenu("demo".into())));
        assert!(app.views["demo"].permissions.menu);
        let _ = app.update(Message::Escape);
        assert!(!app.views["demo"].permissions.menu, "Escape closes the menu");
        let _ = app.update(Message::Perms(Event::ShieldMenu("demo".into())));
        let fake = std::thread::spawn(move || {
            let mut seen = Vec::new();
            loop {
                let request = daemon.next();
                let method = request["method"].as_str().unwrap().to_string();
                match method.as_str() {
                    "_sill/permissions.apply_development" => {
                        assert_eq!(request["params"], json!({"session_id": "demo"}));
                        daemon.reply(&request, json!({"preview": view("preview", false, json!([])), "restart_required": true}));
                    }
                    "_sill/sessions.stop" | "_sill/sessions.start" => {
                        assert_eq!(request["params"], json!({"session_id": "demo"}));
                        daemon.reply(&request, json!({}));
                    }
                    "_sill/sessions.list" => {
                        daemon.reply(&request, json!({"sessions": [session_record("demo", "demo", false, false)], "next": null}));
                    }
                    "_sill/host_control.state" => daemon.refuse(&request, "unavailable", "later"),
                    "_sill/policy.preview" => daemon.refuse(&request, "unavailable", "later"),
                    other => panic!("unexpected {other}"),
                }
                if method == "_sill/sessions.list" && seen.contains(&"_sill/sessions.start".to_string()) {
                    return seen;
                }
                seen.push(method);
            }
        });
        let task = app.update(Message::Perms(Event::DevelopmentRestarting("demo".into())));
        settle_task(&mut app, task);
        let seen = fake.join().unwrap();
        let order: Vec<&str> = seen.iter().map(String::as_str).filter(|m| m.starts_with("_sill/permissions") || m.starts_with("_sill/sessions.st")).collect();
        assert_eq!(order, ["_sill/permissions.apply_development", "_sill/sessions.stop", "_sill/sessions.start"]);
        let perms = &app.views["demo"].permissions;
        assert!(!perms.open && perms.menu, "the dropdown stays; the panel stays closed");
        let (_, said, failed) = perms.notice.clone().unwrap();
        assert!(!failed && said == "Restarted with the new permissions.", "{said}");
    }

    /// Pending changes alone do not add a caption above the composer.
    #[test]
    fn a_pending_change_has_no_caption_without_expanded_access() {
        let (mut app, _) = App::new(true, None);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let preview: PolicyView = serde_json::from_value(view("preview", true, json!([]))).unwrap();
        let launch: PolicyView = serde_json::from_value(view("launch", false, json!([]))).unwrap();
        app.views.get_mut("demo").unwrap().permissions.views =
            Some(Views { preview, launch: Some(launch), agent_name: None });
        let mut ui = simulator(&app, (720.0, 860.0));
        assert!(ui.find("Default · jailed · permission changes apply after a restart").is_err());
        assert!(app.views["demo"].permissions.expanded(true).is_empty(), "the running jail has no public web yet");
    }

    /// Where the machine has a test desktop it is a row of its own (a switch), not folded with
    /// what has no backend; on, it makes the session Desktop development.
    #[test]
    fn an_available_test_desktop_is_a_row_and_sets_the_mode() {
        let (mut app, _) = App::new(true, None);
        app.all_sessions = vec![session_record("demo", "demo", false, false)];
        let mut preview = view("preview", false, json!([]));
        preview["permissions"][0] = json!({"capability": "test_desktop", "allowed": true,
            "default_enabled": false, "requested": true, "blocked": false, "source": "session",
            "effective": true, "available": true,
            "reason": "an isolated Weston desktop with Xwayland; the real display stays out of reach"});
        let preview: PolicyView = serde_json::from_value(preview).unwrap();
        let perms = &mut app.views.get_mut("demo").unwrap().permissions;
        perms.open = true;
        perms.views = Some(Views { launch: Some(preview.clone()), preview, agent_name: None });
        let mut ui = simulator(&app, (720.0, 860.0));
        ui.find("Desktop development").unwrap();
        ui.find("Private D-Bus").unwrap();
        ui.find("GPU rendering").unwrap();
        assert!(ui.find("Jailed · expanded access · Test desktop").is_err(), "no caption: the shield says it");
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
        ui.find("Ghost · Default only").unwrap();
        assert!(ui.find("Restore defaults").is_ok());
    }
}
