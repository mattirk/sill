//! The subagents at work, listed above the prompt: one row each, a small spinner, what the agent
//! calls it and the last thing it did. Read from the conversation alone: a subagent is its
//! delegation card (a tool item of kind `Delegate`, SPEC §4.1), at work while the card's status
//! says so, and what it does are the tool items under that card. A card of an earlier agent
//! process is not listed: nothing will ever finish it.
use iced::widget::{Column, column, container, row, rule, scrollable, space, text};
use iced::{Alignment, Element, Fill};
use sill_client::conversation::Conversation;

use crate::style::{self, P, alpha};
use crate::{Message, SessionView};

/// More rows than this scroll.
const SHOWN: usize = 6;
const ROW_HEIGHT: f32 = 20.0;

/// A subagent at work.
#[derive(Debug, PartialEq, Eq)]
pub struct Subagent<'a> {
    pub title: &'a str,
    /// Its card says it works; false while it is only announced.
    pub working: bool,
    /// The title of its latest call.
    pub activity: Option<&'a str>,
    /// How many subagents it is under.
    pub depth: usize,
}

/// A delegation card's title without the mark `sill-core` gives it.
fn name(title: &str) -> &str {
    title.strip_prefix("sub-agent · ").unwrap_or(title)
}

/// The session's subagents at work, in the order they started.
pub fn at_work(conversation: &Conversation) -> Vec<Subagent<'_>> {
    let Some(session) = conversation.session.as_ref().filter(|s| s.lifecycle != "ended" && !s.deleted) else { return vec![] };
    let mut listed: Vec<Subagent<'_>> = Vec::new();
    // each listed card's place in `listed`
    let mut places = std::collections::HashMap::new();
    for item in conversation.visible_items() {
        let Some(tool) = &item.tool else { continue };
        let parent = tool.parent_item_id.as_deref().and_then(|id| places.get(id).copied());
        if let Some(parent) = parent
            && !tool.title.trim().is_empty()
            // its own subagent has a row of its own
            && !tool.kind.eq_ignore_ascii_case("delegate")
        {
            let under: &mut Subagent<'_> = &mut listed[parent];
            under.activity = Some(name(&tool.title));
        }
        let status = tool.status.to_ascii_lowercase().replace('_', "");
        if !tool.kind.eq_ignore_ascii_case("delegate")
            || item.agent_generation_id != session.agent_generation_id
            || !matches!(status.as_str(), "inprogress" | "pending")
        {
            continue;
        }
        places.insert(item.item_id.as_str(), listed.len());
        listed.push(Subagent { title: name(&tool.title), working: status == "inprogress", activity: None, depth: parent.map_or(0, |p| listed[p].depth + 1) });
    }
    listed
}

/// The list above the prompt; nothing while no subagent works.
pub fn panel(view: &SessionView) -> Option<Element<'_, Message>> {
    let subagents = at_work(&view.conversation);
    if subagents.is_empty() {
        return None;
    }
    let count = subagents.len();
    let mut list = Column::new();
    for subagent in subagents {
        let mark: Element<'_, Message> =
            if subagent.working { crate::ui::small_spinner(P.accent) } else { container(text("·").size(style::SMALL).color(P.fg_dim)).center_x(11).into() };
        let mut line = row![
            space().width(14.0 * subagent.depth as f32),
            mark,
            text(crate::ui::one_line(subagent.title, 80)).size(style::SMALL).wrapping(text::Wrapping::None)
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(activity) = subagent.activity {
            line = line.push(
                container(text(crate::ui::one_line(activity, 120)).size(style::TINY).color(P.fg_dim).wrapping(text::Wrapping::None)).width(Fill).clip(true),
            );
        }
        list = list.push(container(line).height(ROW_HEIGHT).align_y(Alignment::Center).padding([0, 12]).width(Fill).clip(true));
    }
    let list: Element<'_, Message> = if count > SHOWN {
        container(scrollable(list).direction(scrollable::Direction::Vertical(style::bar())).style(style::feed_scroll))
            .max_height(ROW_HEIGHT * SHOWN as f32)
            .into()
    } else {
        list.into()
    };
    // set off from the prompt as the queued prompts are: a faint band, a rule, a gap
    Some(
        column![
            container(list).padding([3, 0]).width(Fill).style(style::fill(alpha(P.accent, 0.05))),
            rule::horizontal(1).style(style::rule_style(P.border)),
            space().height(4),
        ]
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sill_proto::sfp::{Item, Tool};

    fn card(id: &str, seq: u64, generation: &str, kind: &str, title: &str, status: &str, parent: Option<&str>) -> Item {
        Item {
            item_id: id.into(),
            created_seq: seq.to_string(),
            turn_id: None,
            submission_id: None,
            agent_generation_id: generation.into(),
            kind: "tool".into(),
            content: vec![],
            tool: Some(Tool {
                title: title.into(),
                kind: kind.into(),
                status: status.into(),
                detail: String::new(),
                diff: String::new(),
                parent_item_id: parent.map(str::to_string),
                command: None,
            }),
        }
    }

    /// The rows are the delegation cards still at work in the agent process that runs now, each
    /// with its latest call, a subagent's own subagent under it; finished cards, other tools and
    /// an earlier process's cards are not listed, and an ended session lists nothing.
    #[test]
    fn the_subagents_at_work_are_the_open_delegation_cards() {
        let mut conversation = crate::demo::conversation();
        let generation = conversation.session.as_ref().unwrap().agent_generation_id.clone();
        let g = generation.as_str();
        for item in [
            card("old", 100, "earlier", "Delegate", "sub-agent · Left behind", "InProgress", None),
            card("a", 101, g, "Delegate", "sub-agent · Count words", "InProgress", None),
            card("b", 102, g, "Delegate", "sub-agent · Done already", "Completed", None),
            card("a1", 103, g, "Execute", "wc -w a.txt", "Completed", Some("a")),
            card("b1", 104, g, "Read", "Read b.txt", "InProgress", Some("b")),
            card("c", 105, g, "Delegate", "sub-agent · Nested", "Pending", Some("a")),
            card("a2", 106, g, "Read", "Read a.txt", "InProgress", Some("a")),
            card("plain", 107, g, "Execute", "ls", "InProgress", None),
        ] {
            conversation.insert_item(item);
        }
        assert_eq!(
            at_work(&conversation),
            [
                Subagent { title: "Count words", working: true, activity: Some("Read a.txt"), depth: 0 },
                Subagent { title: "Nested", working: false, activity: None, depth: 1 },
            ]
        );
        conversation.session.as_mut().unwrap().lifecycle = "ended".into();
        assert_eq!(at_work(&conversation), []);
    }
}
