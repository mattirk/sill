//! One session's public SFP view, rebuilt from a snapshot and sequenced events (docs/protocol.md).
//! No store, overlay or daemon type is read: `_sill/sessions.snapshot` and `_sill/session.event`
//! are the only source. Each applied event says what it changed, so a frontend redraws that item
//! instead of scanning the whole history.
use sill_proto::method;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use sill_proto::{Client, sfp::*};

/// What one applied event changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// An item appeared, or its text, content or tool changed.
    Item(String),
    ItemRemoved(String),
    /// The session's state: activity, controls, usage, title…
    Session,
    Question(String),
    Submission(String),
    Turn(String),
}

#[derive(Debug, Clone, Default)]
pub struct Conversation {
    pub session_id: String,
    pub subscription_id: String,
    /// The last applied event's sequence (the snapshot's cursor before any).
    pub seq: u64,
    pub session: Option<SessionState>,
    items: BTreeMap<String, Item>,
    /// Item identities in conversation order: creation sequence, then identity.
    order: BTreeSet<(u64, String)>,
    pub questions: BTreeMap<String, Question>,
    /// Where each question was asked, as a place in `order`: a question shows under the last item
    /// shown at or before it (a question carries no position of its own). From a snapshot, after
    /// the item listed before it (records come in creation order); live, at its event.
    asked_at: HashMap<String, (u64, String)>,
    pub submissions: BTreeMap<String, Submission>,
    /// Turn outcomes, for the note a stopped or failed turn ends with.
    pub turns: BTreeMap<String, Turn>,
}

fn position(item: &Item) -> (u64, String) {
    (item.created_seq.parse().unwrap_or_default(), item.item_id.clone())
}

impl Conversation {
    /// An empty view of `session_id` at `seq` (a snapshot's cursor), to install records into.
    pub fn new(session_id: &str, subscription_id: &str, seq: u64) -> Self {
        Self { session_id: session_id.into(), subscription_id: subscription_id.into(), seq, ..Self::default() }
    }

    /// Attach afresh and read the whole snapshot. Notifications arriving meanwhile stay queued on
    /// `client`; apply them afterwards (events through the snapshot's cursor are ignored).
    pub fn attach(client: &Client, session_id: &str) -> Result<Self> {
        let attached: Attached = client.call(method::SESSIONS_ATTACH, Attach { session_id: session_id.into(), since: None })?;
        let Attached::Snapshot { subscription_id, snapshot } = attached else {
            bail!("expected a snapshot for a fresh attachment");
        };
        Self::read_snapshot(client, session_id, &subscription_id, &snapshot)
    }

    /// Read every page of a snapshot lease into a new view.
    pub fn read_snapshot(client: &Client, session_id: &str, subscription_id: &str, snapshot: &SnapshotLease) -> Result<Self> {
        ensure!(snapshot.at.session_id == session_id, "snapshot belongs to another session");
        let seq = snapshot.at.seq.parse().context("invalid snapshot sequence")?;
        let mut view = Self::new(session_id, subscription_id, seq);
        let mut decoder = SnapshotDecoder::default();
        let mut page = None;
        loop {
            let part: SnapshotPage = client.call(method::SESSIONS_SNAPSHOT, SnapshotRequest { token: snapshot.token.clone(), page })?;
            ensure!(part.at == snapshot.at, "snapshot cursor changed during pagination");
            for record in decoder.records(&part).map_err(anyhow::Error::msg)? {
                view.install(record)?;
            }
            page = part.next_page;
            if page.is_none() {
                break;
            }
        }
        ensure!(view.session.is_some(), "snapshot has no session record");
        Ok(view)
    }

    /// One snapshot record into the view.
    pub fn install(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Session { session } => {
                ensure!(session.session_id == self.session_id, "wrong session record");
                self.session = Some(session);
            }
            Record::Item { item } => self.insert_item(item),
            Record::Question { question } => {
                if let Some(last) = self.order.last() {
                    self.asked_at.insert(question.question_id.clone(), last.clone());
                }
                self.questions.insert(question.question_id.clone(), question);
            }
            Record::Submission { submission } => {
                self.submissions.insert(submission.submission_id.clone(), submission);
            }
            Record::Turn { turn } => {
                self.turns.insert(turn.turn_id.clone(), turn);
            }
        }
        Ok(())
    }

    /// An item into the view (a snapshot record, or a synthetic one), replacing one of its identity.
    pub fn insert_item(&mut self, item: Item) {
        if let Some(old) = self.items.remove(&item.item_id) {
            self.order.remove(&position(&old));
        }
        self.order.insert(position(&item));
        self.items.insert(item.item_id.clone(), item);
    }

    pub fn items(&self) -> &BTreeMap<String, Item> {
        &self.items
    }

    pub fn item(&self, item_id: &str) -> Option<&Item> {
        self.items.get(item_id)
    }

    /// An item's fields, for adjusting a synthetic view. Its `created_seq` must not change here.
    pub fn item_mut(&mut self, item_id: &str) -> Option<&mut Item> {
        self.items.get_mut(item_id)
    }

    /// `None` is a foreign subscription's event or an already applied one. A gap is an error: the
    /// caller takes a fresh snapshot. On error nothing changed and the cursor stays.
    pub fn accept(&mut self, notification: SessionEvent) -> Result<Option<Change>> {
        if notification.session_id != self.session_id || notification.subscription_id != self.subscription_id {
            return Ok(None);
        }
        let seq = notification.seq.parse::<u64>().context("invalid event sequence")?;
        if seq <= self.seq {
            return Ok(None);
        }
        ensure!(self.seq.checked_add(1) == Some(seq), "conversation event gap after {}", self.seq);
        let change = match notification.event {
            Event::ItemCreated { item } => {
                ensure!(!self.items.contains_key(&item.item_id), "duplicate item identity");
                let id = item.item_id.clone();
                self.insert_item(item);
                Change::Item(id)
            }
            Event::ItemPatched { item_id, set } => {
                let item = self.items.get_mut(&item_id).context("missing patch target")?;
                if let Some(content) = set.content {
                    item.content = content;
                }
                if let Some(tool) = set.tool {
                    item.tool = Some(tool);
                }
                Change::Item(item_id)
            }
            Event::ItemTextAppended { item_id, block_id, text } => {
                self.items
                    .get_mut(&item_id)
                    .context("missing append target")?
                    .content
                    .iter_mut()
                    .find(|b| b.block_id == block_id)
                    .context("missing text block")?
                    .text
                    .as_mut()
                    .context("append target is not text")?
                    .push_str(&text);
                Change::Item(item_id)
            }
            Event::ItemRemoved { item_id, .. } => {
                let item = self.items.remove(&item_id).context("missing removal target")?;
                self.order.remove(&position(&item));
                Change::ItemRemoved(item_id)
            }
            Event::SessionUpdated { session } => {
                ensure!(session.session_id == self.session_id, "wrong session update");
                self.session = Some(session);
                Change::Session
            }
            Event::QuestionUpdated { question } => {
                let id = question.question_id.clone();
                self.asked_at.entry(id.clone()).or_insert((seq, String::new()));
                self.questions.insert(id.clone(), question);
                Change::Question(id)
            }
            Event::SubmissionUpdated { submission } => {
                let id = submission.submission_id.clone();
                self.submissions.insert(id.clone(), submission);
                Change::Submission(id)
            }
            Event::TurnUpdated { turn } => {
                let id = turn.turn_id.clone();
                self.turns.insert(id.clone(), turn);
                Change::Turn(id)
            }
        };
        self.seq = seq;
        Ok(Some(change))
    }

    /// A prompt still queued is not yet part of the agent's conversation.
    fn queued(&self, item: &Item) -> bool {
        item.kind == "user" && item.submission_id.as_ref().and_then(|id| self.submissions.get(id)).is_some_and(|s| s.delivery == "queued")
    }

    /// The items to show, in conversation order. Linear in the history, never a sort.
    pub fn visible_items(&self) -> Vec<&Item> {
        self.order.iter().filter_map(|(_, id)| self.items.get(id)).filter(|item| !self.queued(item)).collect()
    }

    /// Which shown item each question goes under, in the order they were asked: its tool's item
    /// when that is shown, else the last item shown where it was asked. Questions with neither (asked
    /// before anything was shown) are not here; a frontend puts them at the end.
    pub fn question_places(&self) -> HashMap<&str, Vec<&Question>> {
        let shown = self.visible_items();
        let ids: HashSet<&str> = shown.iter().map(|i| i.item_id.as_str()).collect();
        let positions: Vec<(u64, String)> = shown.iter().map(|i| position(i)).collect();
        let mut asked: Vec<(&Question, Option<&(u64, String)>)> = self.questions.values().map(|q| (q, self.asked_at.get(&q.question_id))).collect();
        asked.sort_by_key(|(_, at)| *at);
        let mut places: HashMap<&str, Vec<&Question>> = HashMap::new();
        for (question, at) in asked {
            let place = match question.tool_item_id.as_deref() {
                Some(tool) if ids.contains(tool) => Some(tool),
                _ => at.and_then(|at| {
                    let before = positions.partition_point(|p| p <= at);
                    before.checked_sub(1).map(|i| shown[i].item_id.as_str())
                }),
            };
            if let Some(item) = place {
                places.entry(item).or_default().push(question);
            }
        }
        places
    }

    /// Why sill, not the person, started what `item` belongs to (`Turn::purpose`): its turn's
    /// purpose, or for the person's item its submission's (`Submit::purpose`). None for the
    /// person's own work.
    pub fn purpose(&self, item: &Item) -> Option<&str> {
        if item.kind == "user" {
            let id = item.submission_id.as_ref()?;
            return self.submissions.get(id)?.purpose.as_deref();
        }
        self.turns.get(item.turn_id.as_ref()?)?.purpose.as_deref()
    }

    /// Whether `turn_id` is still running.
    pub fn turn_pending(&self, turn_id: &str) -> bool {
        self.turns.get(turn_id).is_some_and(|t| t.outcome == "pending")
    }

    /// The note a turn ends with, when it did not complete: stopped by the person, or failed.
    pub fn turn_end(&self, turn_id: &str) -> Option<String> {
        let turn = self.turns.get(turn_id)?;
        match turn.outcome.as_str() {
            "cancelled" => Some("stopped".into()),
            "failed" => Some(match &turn.reason {
                Some(reason) => format!("failed: {}", reason.message),
                None => "failed".into(),
            }),
            _ => None,
        }
    }

    /// The most recent turn that has items, by conversation order.
    pub fn last_turn_id(&self) -> Option<&str> {
        self.order.iter().rev().filter_map(|(_, id)| self.items.get(id)).find_map(|item| item.turn_id.as_deref())
    }

    /// Questions waiting for the person.
    pub fn open_questions(&self) -> impl Iterator<Item = &Question> {
        self.questions.values().filter(|q| q.state == "open")
    }

    /// Prompts the daemon still holds unsent: removable before stopping.
    pub fn queued_submission_ids(&self) -> Vec<String> {
        self.submissions
            .values()
            .filter(|s| s.delivery == "queued" && s.insertion == "pending" && s.outcome == "pending")
            .map(|s| s.submission_id.clone())
            .collect()
    }

    /// The agent is working or waiting on a question.
    pub fn busy(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.activity == "working" || s.activity == "waiting")
    }

    pub fn transcript(&self) -> String {
        self.visible_items().into_iter().map(|item| format!("{}\n{}", item.kind, item_text(item))).collect::<Vec<_>>().join("\n\n")
    }
}

/// An item as text: a tool's title, output and diff, then every block's text. A block of a type
/// this client does not know shows by its text, or not at all (docs/protocol.md, content blocks).
pub fn item_text(item: &Item) -> String {
    let mut parts = Vec::new();
    if let Some(tool) = &item.tool {
        parts.push(format!("{} · {}", tool.title, tool.status));
        if !tool.detail.is_empty() {
            parts.push(tool.detail.clone());
        }
        if !tool.diff.is_empty() {
            parts.push(tool.diff.clone());
        }
    }
    for block in &item.content {
        if let Some(text) = &block.text {
            parts.push(text.clone());
        } else if block.kind == "image" {
            parts.push("[Image]".into());
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, seq: u64, kind: &str, text: &str) -> Item {
        Item {
            item_id: id.into(),
            created_seq: seq.to_string(),
            turn_id: Some("t".into()),
            submission_id: None,
            agent_generation_id: "g".into(),
            kind: kind.into(),
            content: vec![Content::text(text)],
            tool: None,
        }
    }

    fn event(view: &Conversation, payload: Event) -> SessionEvent {
        SessionEvent { session_id: view.session_id.clone(), subscription_id: view.subscription_id.clone(), seq: (view.seq + 1).to_string(), event: payload }
    }

    fn view() -> Conversation {
        let mut view = Conversation::new("s", "sub", 2);
        view.insert_item(item("b", 2, "agent", "second"));
        view.insert_item(item("a", 1, "user", "first"));
        view
    }

    #[test]
    fn events_report_what_they_changed_and_order_is_kept_without_sorting() {
        let mut view = view();
        let ids = |v: &Conversation| -> Vec<String> { v.visible_items().iter().map(|i| i.item_id.clone()).collect() };
        assert_eq!(ids(&view), ["a", "b"]);
        let created = event(&view, Event::ItemCreated { item: item("c", 3, "agent", "third") });
        assert_eq!(view.accept(created).unwrap(), Some(Change::Item("c".into())));
        let appended = event(&view, Event::ItemTextAppended { item_id: "b".into(), block_id: "text".into(), text: " more".into() });
        assert_eq!(view.accept(appended).unwrap(), Some(Change::Item("b".into())));
        assert_eq!(item_text(view.item("b").unwrap()), "second more");
        let removed = event(&view, Event::ItemRemoved { item_id: "a".into(), reason: Reason { code: "x".into(), message: "x".into() } });
        assert_eq!(view.accept(removed).unwrap(), Some(Change::ItemRemoved("a".into())));
        assert_eq!(ids(&view), ["b", "c"]);
        assert_eq!(view.last_turn_id(), Some("t"));
    }

    #[test]
    fn duplicates_foreign_events_and_gaps_leave_the_view_unchanged() {
        let mut view = view();
        let next = event(&view, Event::ItemTextAppended { item_id: "b".into(), block_id: "text".into(), text: "!".into() });
        assert!(view.accept(next.clone()).unwrap().is_some());
        assert!(view.accept(next).unwrap().is_none(), "already applied");
        let mut foreign = event(&view, Event::ItemTextAppended { item_id: "b".into(), block_id: "text".into(), text: "wrong".into() });
        foreign.subscription_id = "old".into();
        assert!(view.accept(foreign).unwrap().is_none());
        let mut gap = event(&view, Event::ItemTextAppended { item_id: "b".into(), block_id: "text".into(), text: "gap".into() });
        gap.seq = (view.seq + 2).to_string();
        assert!(view.accept(gap).is_err());
        assert_eq!(view.seq, 3);
        assert_eq!(item_text(view.item("b").unwrap()), "second!");
    }

    fn question(id: &str) -> Question {
        serde_json::from_value(json!({
            "question_id": id, "agent_generation_id": "g", "kind": "form", "title": "Input?",
            "tool_item_id": null, "options": [], "fields": [], "state": "open",
            "answer_id": null, "answer": null, "delivery": null, "reason": null
        }))
        .unwrap()
    }

    /// A question without a tool stays where it was asked, under the item before it, as the
    /// conversation goes on below it: from a snapshot and live.
    #[test]
    fn a_question_stays_where_it_was_asked() {
        let mut view = Conversation::new("s", "sub", 2);
        view.install(Record::Item { item: item("a", 1, "user", "first") }).unwrap();
        view.install(Record::Question { question: question("from-snapshot") }).unwrap();
        view.install(Record::Item { item: item("b", 2, "agent", "second") }).unwrap();
        view.accept(event(&view, Event::QuestionUpdated { question: question("live") })).unwrap();
        view.accept(event(&view, Event::ItemCreated { item: item("c", 4, "agent", "third") })).unwrap();
        let places = view.question_places();
        let ids = |item: &str| -> Vec<&str> { places.get(item).map(|q| q.iter().map(|q| q.question_id.as_str()).collect()).unwrap_or_default() };
        assert_eq!(ids("a"), ["from-snapshot"]);
        assert_eq!(ids("b"), ["live"]);
        assert!(ids("c").is_empty(), "not pulled to the end");
    }

    #[test]
    fn a_queued_prompt_is_hidden_until_dispatched() {
        let mut view = view();
        view.item_mut("a").unwrap().submission_id = Some("q".into());
        let mut submission: Submission = serde_json::from_value(json!({
            "submission_id":"q", "session_id":"s", "accepted_seq":"1",
            "agent_generation_id":"g", "text":"first", "attachments":[],
            "acceptance":"accepted", "delivery":"queued", "held":false,
            "insertion":"pending", "outcome":"pending"
        }))
        .unwrap();
        view.accept(event(&view, Event::SubmissionUpdated { submission: submission.clone() })).unwrap();
        assert!(!view.visible_items().iter().any(|i| i.item_id == "a"));
        assert_eq!(view.queued_submission_ids(), ["q"]);
        submission.delivery = "sent".into();
        view.accept(event(&view, Event::SubmissionUpdated { submission })).unwrap();
        assert!(view.visible_items().iter().any(|i| i.item_id == "a"));
        assert!(view.queued_submission_ids().is_empty());
    }
}
