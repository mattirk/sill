//! The subagents at work, listed above the prompt: one row each, a small spinner, what the agent
//! calls it and the last thing it did. Read from the conversation alone: a subagent is its
//! delegation card (a tool item of kind `Delegate`, SPEC §4.1), at work while the card's status
//! says so, and what it does are the tool items under that card. A card of an earlier agent
//! process is not listed: nothing will ever finish it.
use iced::widget::{Column, column, container, row, rule, scrollable, space, text};
use iced::{Alignment, Element, Fill};
use sill_client::conversation::Conversation;
use sill_proto::sfp::Item;
use std::{collections::HashMap, time::Instant};

use crate::style::{self, P, alpha};
use crate::{Message, SessionView};

/// More rows than this scroll.
const SHOWN: usize = 6;
const ROW_HEIGHT: f32 = 20.0;

/// A subagent at work.
#[derive(Debug, PartialEq, Eq)]
pub struct Subagent<'a> {
    pub title: &'a str,
    pub id: &'a str,
    pub status: Status,
    /// The title of its latest call.
    pub activity: Option<&'a str>,
    /// How many subagents it is under.
    pub depth: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Running,
    NeedsInput,
    Unknown,
}

impl Status {
    fn of(item: &Item, conversation: &Conversation) -> Option<Self> {
        let session = conversation.session.as_ref()?;
        let tool = item.tool.as_ref()?;
        if session.lifecycle == "ended"
            || session.deleted
            || item.agent_generation_id != session.agent_generation_id
            || !tool.kind.eq_ignore_ascii_case("delegate")
        {
            return None;
        }
        match tool.status.to_ascii_lowercase().replace('_', "").as_str() {
            "inprogress" => Some(Self::Running),
            "requiresaction" => Some(Self::NeedsInput),
            "pending" | "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::NeedsInput => "Needs input",
            Self::Unknown => "Status unknown",
        }
    }
}

#[derive(Default)]
pub(crate) struct Clocks(HashMap<String, (Status, Instant)>);

impl Clocks {
    pub(crate) fn observe(&mut self, conversation: &Conversation, id: &str, now: Instant) {
        match conversation.item(id).and_then(|item| Status::of(item, conversation)) {
            Some(status) if self.0.get(id).is_none_or(|(previous, _)| *previous != status) => {
                self.0.insert(id.to_owned(), (status, now));
            }
            None => {
                self.0.remove(id);
            }
            Some(_) => {}
        }
    }

    pub(crate) fn retain(&mut self, conversation: &Conversation) {
        self.0.retain(|id, _| conversation.item(id).and_then(|item| Status::of(item, conversation)).is_some());
    }

    pub(crate) fn ticking(&self) -> bool {
        !self.0.is_empty()
    }
}

/// A delegation card's title without the mark `sill-core` gives it.
fn name(title: &str) -> &str {
    title.strip_prefix("sub-agent · ").unwrap_or(title)
}

/// The session's subagents at work, in the order they started.
pub fn at_work(conversation: &Conversation) -> Vec<Subagent<'_>> {
    if conversation.session.as_ref().is_none_or(|s| s.lifecycle == "ended" || s.deleted) {
        return vec![];
    }
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
        let Some(status) = Status::of(item, conversation) else { continue };
        places.insert(item.item_id.as_str(), listed.len());
        listed.push(Subagent { id: &item.item_id, title: name(&tool.title), status, activity: None, depth: parent.map_or(0, |p| listed[p].depth + 1) });
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
        let mark: Element<'_, Message> = match subagent.status {
            Status::Running => crate::ui::small_spinner(P.accent),
            Status::NeedsInput => container(text("!").size(style::SMALL).color(P.warning)).center_x(11).into(),
            Status::Unknown => container(text("·").size(style::SMALL).color(P.fg_dim)).center_x(11).into(),
        };
        let mut description = row![
            text(crate::ui::one_line(subagent.title, 48)).size(style::SMALL).wrapping(text::Wrapping::None),
            text(subagent.status.label()).size(style::TINY).color(P.fg_dim).wrapping(text::Wrapping::None)
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(activity) = subagent.activity {
            description = description.push(text(crate::ui::one_line(activity, 120)).size(style::TINY).color(P.fg_dim).wrapping(text::Wrapping::None));
        }
        let elapsed = view.subagent_clocks.0.get(subagent.id).map_or(0, |(_, since)| since.elapsed().as_millis() as u64);
        let line = row![
            space().width(14.0 * subagent.depth as f32),
            mark,
            container(description).width(Fill).clip(true),
            text(crate::processes::elapsed(elapsed)).font(crate::ui::MONO_FONT).size(style::TINY).color(P.fg_dim),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
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
    use sill_proto::sfp::Tool;

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

    #[test]
    fn elapsed_time_starts_over_only_when_the_reported_status_changes() {
        use std::time::Duration;

        let mut conversation = crate::demo::conversation();
        let generation = conversation.session.as_ref().unwrap().agent_generation_id.clone();
        conversation.insert_item(card("child", 100, &generation, "Delegate", "Timer", "InProgress", None));
        let mut clocks = Clocks::default();
        let started = Instant::now();
        let later = started + Duration::from_secs(5);
        clocks.observe(&conversation, "child", started);
        conversation.item_mut("child").unwrap().tool.as_mut().unwrap().title = "Timer two".into();
        clocks.observe(&conversation, "child", later);
        assert_eq!(clocks.0["child"], (Status::Running, started));

        conversation.item_mut("child").unwrap().tool.as_mut().unwrap().status = "RequiresAction".into();
        clocks.observe(&conversation, "child", later);
        assert_eq!(at_work(&conversation)[0].status, Status::NeedsInput);
        assert_eq!(clocks.0["child"], (Status::NeedsInput, later));

        conversation.item_mut("child").unwrap().tool.as_mut().unwrap().status = "Completed".into();
        clocks.observe(&conversation, "child", later);
        assert!(!clocks.ticking());
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
                Subagent { id: "a", title: "Count words", status: Status::Running, activity: Some("Read a.txt"), depth: 0 },
                Subagent { id: "c", title: "Nested", status: Status::Unknown, activity: None, depth: 1 },
            ]
        );
        conversation.session.as_mut().unwrap().lifecycle = "ended".into();
        assert_eq!(at_work(&conversation), []);
    }
}
