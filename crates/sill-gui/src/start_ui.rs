//! The window with no open session: a button per agent that can start one, or *Install agent*
//! when none is installed.
use iced::widget::{Row, button, center, column, container, row, svg, text};
use iced::{Alignment, Element, Fill};
use sill_proto::agent_admin::Definition;

use crate::style::{self, P, alpha};
use crate::{App, Message, Page, keys, menu};

impl App {
    pub(crate) fn start_view<'a>(&'a self, agents: &'a [Definition]) -> Element<'a, Message> {
        let body: Element<'_, Message> = if agents.is_empty() {
            keys::press(
                button(row![crate::icons::glyph("plus", 15.0, P.accent), text("Install agent").size(style::TEXT),].spacing(8).align_y(Alignment::Center))
                    .padding([8, 14])
                    .style(style::outlined_accent),
                Message::ShowPage(Page::Agents),
            )
        } else {
            let mut buttons = Row::new().spacing(10).align_y(Alignment::Center);
            for agent in agents {
                let ready = agent.home_paths.is_some();
                let ink = if ready { P.fg } else { alpha(P.fg, 0.5) };
                let mut label =
                    row![svg(self.agent_icon(&agent.id)).width(16).height(16).style(style::icon(ink)), text(&agent.name).size(style::TEXT).color(ink),]
                        .spacing(9)
                        .align_y(Alignment::Center);
                if !ready {
                    label = label.push(text("needs setup").size(style::TINY).color(P.warning));
                }
                buttons = buttons.push(keys::press(
                    button(label).padding([8, 14]).style(style::outlined),
                    Message::Menu(if ready {
                        menu::Event::New { agent: agent.id.clone(), ghost: false, workspaces: None }
                    } else {
                        menu::Event::Setup(agent.id.clone())
                    }),
                ));
            }
            column![text("Start a session with").size(style::TEXT).color(P.fg_dim), buttons.wrap().vertical_spacing(10),]
                .spacing(16)
                .align_x(Alignment::Center)
                .into()
        };
        container(center(body)).padding(24).height(Fill).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, name: &str, ready: bool) -> Definition {
        let mut value = serde_json::json!({"id": id, "name": name, "command": [id]});
        if ready {
            value["home_paths"] = serde_json::json!([{"path": format!("~/.{id}"), "kind": "directory"}]);
        }
        serde_json::from_value(value).unwrap()
    }

    fn empty(agents: Vec<Definition>) -> App {
        let (mut app, _) = App::new(true, None);
        app.tabs.clear();
        app.selected = None;
        app.listed = true;
        app.start_agents = Some(Ok(agents));
        app
    }

    #[test]
    fn without_agents_the_empty_window_offers_to_install_one() {
        let app = empty(vec![]);
        let mut ui = crate::tests::simulator(&app, (720.0, 860.0));
        ui.click("Install agent").unwrap();
        let messages: Vec<Message> = ui.into_messages().collect();
        assert!(matches!(&messages[..], [Message::ShowPage(Page::Agents)]), "{messages:?}");
    }

    #[test]
    fn the_empty_window_starts_a_session_with_an_installed_agent() {
        let mut app = empty(vec![agent("claude", "Claude Code", true), agent("antigravity-acp", "Antigravity", false)]);
        let mut ui = crate::tests::simulator(&app, (720.0, 860.0));
        assert!(ui.find("Install agent").is_err());
        ui.find("needs setup").unwrap();
        ui.click("Claude Code").unwrap();
        ui.click("Antigravity").unwrap();
        let messages: Vec<Message> = ui.into_messages().collect();
        assert!(
            matches!(&messages[..], [Message::Menu(menu::Event::New { agent, ghost: false, workspaces: None }), Message::Menu(menu::Event::Setup(id))] if agent == "claude" && id == "antigravity-acp"),
            "{messages:?}"
        );
        // the + menu's list, once read, is the empty window's too
        let _ = app.update(Message::Menu(menu::Event::Agents(Ok(vec![]))));
        assert!(matches!(&app.start_agents, Some(Ok(agents)) if agents.is_empty()));
    }

    #[test]
    #[ignore = "writes the empty window's PNGs for visual review"]
    fn render_start() {
        let (mut app, _) = App::new(true, None);
        app.tabs.clear();
        app.selected = None;
        app.listed = true;
        let size = (1006.0, 774.0);
        app.start_agents = Some(Ok(vec![]));
        crate::snapshots::render_size(&app, "start-none", size);
        app.start_agents = Some(Ok(vec![agent("claude", "Claude Code", true)]));
        crate::snapshots::render_size(&app, "start-one", size);
        app.start_agents = Some(Ok(vec![
            agent("claude", "Claude Code", true),
            agent("codex", "Codex", true),
            agent("opencode", "OpenCode", true),
            agent("antigravity-acp", "Antigravity", false),
        ]));
        crate::snapshots::render_size(&app, "start-several", size);
        crate::snapshots::render_size(&app, "start-several-narrow", (480.0, 774.0));
    }
}
