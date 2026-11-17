//! The `+` menu's logic (its view and events are `menu.rs`): what it lists, and starting,
//! reopening and deleting sessions from it.
use iced::Task;
use serde_json::json;
use sill_proto::method;

use crate::{Act, App, Message, Page, blocking, menu};

impl App {
    pub(crate) fn menu_event(&mut self, event: menu::Event) -> Task<Message> {
        use menu::Event;
        match event {
            Event::View(mode) => {
                self.menu = None;
                return self.set_mode(mode);
            }
            Event::Leave => {
                self.menu = None;
            }
            Event::Toggle | Event::Hover => {
                if matches!(event, Event::Toggle) && self.menu.take().is_some() {
                    return Task::none();
                }
                if self.menu.is_some() {
                    return Task::none();
                }
                self.menu = Some(menu::Menu::default());
                let Some(actions) = self.actions.clone() else {
                    // the demo has two agents and nothing to reopen
                    self.menu.as_mut().unwrap().agents = Some(Ok(demo_agents()));
                    return Task::none();
                };
                let settings = actions.clone();
                return Task::batch([
                    blocking(
                        move || actions.reading(sill_client::sessions::startable).map_err(|e| format!("{e:#}")),
                        |agents| Message::Menu(Event::Agents(agents)),
                    ),
                    blocking(
                        move || {
                            settings.read::<sill_proto::admin::Settings>(method::SETTINGS_GET, json!({})).ok().and_then(|s| s.remember_pins).unwrap_or(false)
                        },
                        |on| Message::Menu(Event::RememberPins(on)),
                    ),
                    self.list(),
                ]);
            }
            Event::Agents(agents) => {
                let ids: Vec<String> = agents.as_ref().map(|a| a.iter().map(|d| d.id.clone()).collect()).unwrap_or_default();
                // the window with no open session offers the same agents
                if agents.is_ok() {
                    self.start_agents = Some(agents.clone());
                }
                if let Some(menu) = &mut self.menu {
                    menu.agents = Some(agents);
                }
                return self.agent_icons_load(ids);
            }
            Event::RememberPins(on) => {
                if let Some(menu) = &mut self.menu {
                    menu.remember_pins = on;
                }
            }
            Event::Filter(value) => {
                if let Some(menu) = &mut self.menu {
                    menu.filter = value;
                }
            }
            Event::More => {
                if let Some(menu) = &mut self.menu {
                    menu.expanded = true;
                }
            }
            Event::New { agent, ghost, workspaces } => {
                let remember =
                    self.menu.take().map(|m| m.remember_pins).unwrap_or_else(|| self.daemon_settings.as_ref().and_then(|s| s.remember_pins).unwrap_or(false));
                if self.demo {
                    return Task::none();
                }
                let (method, params) = sill_client::sessions::create(&agent, ghost, remember, workspaces);
                return self.act("", Act::Create, method, params);
            }
            Event::AlwaysApply(agent) => {
                let remember = self.menu.take().is_some_and(|menu| menu.remember_pins);
                if self.demo {
                    return Task::none();
                }
                let (method, mut params) = sill_client::sessions::create(&agent, false, remember, Some(true));
                params["always_apply"] = true.into();
                return self.act("", Act::Create, method, params);
            }
            Event::Reopen(id) => {
                self.menu = None;
                let ended = self.all_sessions.iter().any(|s| s.session_id == id && s.ended_at.is_some());
                if !ended {
                    return self.choose(id);
                }
                let (method, params) = sill_client::sessions::reopen(&id);
                return self.act(&id, Act::Reopen(id.clone()), method, params);
            }
            Event::Setup(_) => {
                self.menu = None;
                return self.update(Message::ShowPage(Page::Agents));
            }
            Event::NewHere(id) => {
                self.menu = None;
                let Some(from) = self.all_sessions.iter().find(|s| s.session_id == id) else {
                    return Task::none();
                };
                let (method, params) = sill_client::sessions::new_here(from);
                return self.act("", Act::Create, method, params);
            }
            Event::Delete(id) => {
                let Some(menu) = &mut self.menu else {
                    return Task::none();
                };
                if !menu.armed.press(id.clone()) {
                    return Task::none();
                }
                let (method, params) = sill_client::sessions::delete(&id);
                return self.act(&id, Act::Delete, method, params);
            }
        }
        Task::none()
    }
}

/// The demo has two agents.
pub(crate) fn demo_agents() -> Vec<sill_proto::agent_admin::Definition> {
    ["claude", "codex"]
        .iter()
        .map(|id| {
            serde_json::from_value(json!({
                "id": id, "name": if *id == "claude" { "Claude Code" } else { "Codex" },
                "command": [], "home_paths": [{"path": format!("~/.{id}"), "kind": "directory"}]
            }))
            .expect("a demo agent")
        })
        .collect()
}
